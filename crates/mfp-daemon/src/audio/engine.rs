//! The audio thread.
//!
//! `rodio` 0.22 has no `Sink`: the playback handle is `rodio::Player`, connected to a
//! `DeviceSink`. `DeviceSink::log_on_drop(false)` returns `()` and so cannot be chained off
//! `open_stream()?`; it must be a separate statement, and it must be called, because the
//! drop-time warning it suppresses would otherwise corrupt the interface.
//!
//! The thread owns the player, consumes a command channel, and never awaits. It publishes
//! into [`SharedState`], which is the only thing the socket server reads, and it touches
//! only the fields it owns there so the download manager's progress survives alongside it.
//!
//! Two boundaries are deliberately left to the socket server, which is the side that can
//! answer a request:
//!
//! - Commands are queued, not answered, so a command that the spec says must fail -
//!   `play` with nothing loaded, `seek` on a source that cannot seek - is refused by the
//!   server from the snapshot before it ever reaches here. Reaching here anyway is a
//!   no-op or an error state, never a panic.
//! - The position reached at a stop is offered for persistence by reading the snapshot
//!   *before* [`AudioCommand::Stop`] is sent. End of episode sends nothing, which is why
//!   no resume position is recorded there.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::JoinHandle;
use std::time::Duration;

use mfp_core::Episode;
use mfp_core::error::{Error, ErrorCode, Result};
use mfp_core::protocol::{EpisodeRef, ErrorObject, PlaybackState, Source};

use super::seek::{self, SeekPlan};
use super::source::{SampleTap, Tap};
use super::spectrum::Analyser;
use crate::state::SharedState;

/// How long the thread waits for a command before republishing. The socket server
/// broadcasts level-triggered at 4 Hz, so a published position must be at least that fresh;
/// it is also how promptly the end of an episode is noticed.
const TICK: Duration = Duration::from_millis(100);

/// How often the spectrum is recomputed. Snapshots caused by a spectrum change alone are
/// capped at 20 per second, so a faster analysis would only produce frames nothing carries.
const ANALYSIS_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug)]
pub enum AudioCommand {
    /// Load an episode and begin playing it from `start_secs`, preferring `local_path`
    /// when it names a complete verified copy.
    Load {
        episode: Box<Episode>,
        local_path: Option<PathBuf>,
        start_secs: f64,
    },
    Play,
    Pause,
    Toggle,
    /// End playback, release the source, and clear the loaded episode.
    Stop,
    /// Seek to an absolute target. A seek accepted while another is in flight supersedes
    /// it; neither request is an error.
    Seek {
        target_secs: f64,
    },
    /// Stop audio and end the thread.
    Shutdown,
}

/// A handle on the audio thread.
pub struct AudioEngine {
    commands: Sender<AudioCommand>,
    thread: JoinHandle<()>,
}

impl AudioEngine {
    /// Starts the thread.
    ///
    /// `runtime` lets it drive the streamed reader without an async audio path; `state`
    /// is the snapshot it publishes position, playback state, and errors into.
    ///
    /// The handle must belong to a multi-threaded runtime. The audio thread reaches the
    /// network through `Handle::block_on`, and a current-thread runtime is driven only by
    /// the thread blocked on it, so a streamed source would never finish opening.
    pub fn spawn(runtime: tokio::runtime::Handle, state: SharedState) -> Result<Self> {
        Self::spawn_with(runtime, state, Output::open_device)
    }

    /// Starts the thread over an output the caller names.
    ///
    /// The output is built on the audio thread because it has to live and die there, so
    /// whether it opened comes back over a handshake channel rather than as a return value.
    fn spawn_with(
        runtime: tokio::runtime::Handle,
        state: SharedState,
        open: fn() -> Result<Output>,
    ) -> Result<Self> {
        let (commands, requests) = mpsc::channel();
        let (ready, opened) = mpsc::channel();

        let thread =
            std::thread::Builder::new()
                .name("mfp-audio".into())
                .spawn(move || match open() {
                    Ok(output) => {
                        let _ = ready.send(Ok(()));
                        Engine::new(runtime, state, output).run(&requests);
                    }
                    Err(error) => {
                        let _ = ready.send(Err(error));
                    }
                })?;

        match opened.recv() {
            Ok(Ok(())) => Ok(Self { commands, thread }),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(_) => Err(Error::PlaybackFailed(
                "the audio thread ended before it started".into(),
            )),
        }
    }

    /// Queues a command. Never blocks waiting on audio.
    pub fn send(&self, command: AudioCommand) -> Result<()> {
        self.commands
            .send(command)
            .map_err(|_| Error::PlaybackFailed("the audio thread is gone".into()))
    }

    /// Stops audio and joins the thread.
    pub fn shutdown(self) {
        let _ = self.commands.send(AudioCommand::Shutdown);
        let _ = self.thread.join();
    }

    /// An engine that collects its commands instead of playing them, together with the
    /// receiver holding them.
    ///
    /// Lets the daemon's wiring - what it asks the audio thread to do and what it persists
    /// around that - be tested where there is no audio device. Commands queue only while
    /// the caller holds the receiver.
    #[cfg(test)]
    pub(crate) fn recording() -> (Self, Receiver<AudioCommand>) {
        let (commands, requests) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("mfp-audio-recording".into())
            .spawn(|| {})
            .expect("a thread that does nothing");
        (Self { commands, thread }, requests)
    }
}

/// Where the engine's samples go.
struct Output {
    player: rodio::Player,
    /// Held for as long as the thread runs: dropping it closes the device.
    _sink: Option<rodio::MixerDeviceSink>,
}

impl Output {
    fn open_device() -> Result<Self> {
        let builder = rodio::DeviceSinkBuilder::from_default_device()
            .map_err(|error| Error::PlaybackFailed(format!("no audio output device: {error}")))?;
        let mut sink = builder
            .open_stream()
            .map_err(|error| Error::PlaybackFailed(format!("no audio output stream: {error}")))?;
        // Returns `()`, so it cannot be chained off `open_stream()?`. Without it the
        // drop-time warning goes to stderr and corrupts a terminal interface sharing it
        sink.log_on_drop(false);
        let player = rodio::Player::connect_new(sink.mixer());
        Ok(Self {
            player,
            _sink: Some(sink),
        })
    }

    /// A player with no device behind it, consumed by a thread of its own at the rate a
    /// device would.
    ///
    /// Lets the engine be exercised where there is no audio hardware, which is every
    /// continuous integration machine. The pace comes from the test fixture's own sample
    /// rate rather than from the queue, which reports the rate of whatever it is holding and
    /// so reports silence at a rate of its own between sources; consuming faster than real
    /// time would let a position race past the window a test is waiting for.
    #[cfg(test)]
    fn open_headless() -> Result<Self> {
        const CHUNK: u32 = super::testing::SAMPLE_RATE / 100;

        let (player, mut queue) = rodio::Player::new();
        std::thread::Builder::new()
            .name("mfp-audio-drain".into())
            .spawn(move || {
                loop {
                    for _ in 0..CHUNK {
                        if queue.next().is_none() {
                            return;
                        }
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            })?;
        Ok(Self {
            player,
            _sink: None,
        })
    }
}

// == Spectrum Analysis ==

/// The spectrum analysis, running on a thread of its own.
///
/// Deliberately neither the audio thread nor the engine thread: the transform must never
/// sit between the device and a sample, nor delay a published position. It reads the
/// samples the tap has collected and writes the one snapshot field nothing else writes.
struct Analysis {
    running: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Analysis {
    fn spawn(state: SharedState, tap: SampleTap) -> Self {
        let running = Arc::new(AtomicBool::new(true));
        let wanted = running.clone();
        let thread = std::thread::Builder::new()
            .name("mfp-spectrum".into())
            .spawn(move || {
                let mut analyser = Analyser::new();
                while wanted.load(Ordering::Relaxed) {
                    std::thread::sleep(ANALYSIS_INTERVAL);
                    analyse(&state, &tap, &mut analyser);
                }
            })
            .inspect_err(|error| tracing::warn!(%error, "no spectrum: the analyser did not start"))
            .ok();
        Self { running, thread }
    }
}

impl Drop for Analysis {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Computes one frame and publishes it.
///
/// Nothing is computed unless audio is actually playing: a pause, a stop, a seek in
/// flight, and a failure all mean nobody is hearing a signal to analyse. The transform
/// runs with neither the shared state nor the tap held, so a slow frame delays nothing -
/// a frame it cannot start is skipped outright rather than queued.
fn analyse(state: &SharedState, tap: &SampleTap, analyser: &mut Analyser) {
    {
        let Ok(mut snapshot) = state.lock() else {
            return;
        };
        if snapshot.playback != PlaybackState::Playing {
            snapshot.spectrum = None;
            return;
        }
    }

    let Some((samples, channels)) = tap.take() else {
        return;
    };
    let spectrum = analyser.analyse(&samples, channels);

    let Ok(mut snapshot) = state.lock() else {
        return;
    };
    // Playback can have ended while the transform ran, and a spectrum outliving the audio
    // it describes is exactly what "analysis ceases when playback does" forbids
    snapshot.spectrum = (snapshot.playback == PlaybackState::Playing).then_some(spectrum);
}

/// The episode the engine is currently holding open.
struct Loaded {
    /// Shared rather than owned: every rebuild needs it while `self` is borrowed mutably,
    /// and an enriched episode carries a whole information page to copy
    episode: Arc<Episode>,
    local_path: Option<PathBuf>,
    /// None until a chain has actually been built. A guess published here shows a client
    /// `stream` for an episode that turns out to be local
    kind: Option<Source>,
    seekable: bool,
    /// What the range probe learned about this enclosure, kept because the answer cannot
    /// change under a session and asking costs a request per rebuild
    range_support: Option<bool>,
}

/// What a requested seek target turns out to mean.
enum SeekOutcome {
    /// A target a decoder chain can be built at.
    At(f64),
    /// At or past the end, which finishes the episode rather than erroring.
    EndOfEpisode,
}

/// What arrived while a rebuild was running.
enum Supersede {
    /// Nothing that changes where this seek is going.
    None,
    /// A later seek. The chain just built is dropped unheard and the new target is built
    /// instead, so only the latest target is ever honoured.
    Target(f64),
    /// A command that abandons the seek outright: stopping, loading another episode, or
    /// shutting down. It is carried back out to be dispatched rather than run in place.
    Abandon(AudioCommand),
}

struct Engine {
    runtime: tokio::runtime::Handle,
    state: SharedState,
    output: Output,
    loaded: Option<Loaded>,
    /// Where the current decoder's zero sits in the episode. Every rebuild restarts the
    /// decoder's clock, so this is what keeps the reported position honest.
    base_offset_secs: f64,
    /// The transport state the user has asked for, which survives a rebuild spanning a pause.
    want_paused: bool,
    playback: PlaybackState,
    seek_target_secs: Option<f64>,
    error: Option<ErrorObject>,
    stopping: bool,
    /// The samples the appended chain is producing, read by [`Analysis`]
    tap: SampleTap,
    /// Stopped when the engine is dropped, which is what ends the analysis thread
    _analysis: Analysis,
}

impl Engine {
    fn new(runtime: tokio::runtime::Handle, state: SharedState, output: Output) -> Self {
        let tap = SampleTap::new();
        let analysis = Analysis::spawn(state.clone(), tap.clone());
        Self {
            runtime,
            state,
            output,
            loaded: None,
            base_offset_secs: 0.0,
            want_paused: false,
            playback: PlaybackState::Stopped,
            seek_target_secs: None,
            error: None,
            stopping: false,
            tap,
            _analysis: analysis,
        }
    }

    fn run(mut self, requests: &Receiver<AudioCommand>) {
        self.publish();
        loop {
            match requests.recv_timeout(TICK) {
                // A command that abandoned a rebuild is dispatched here rather than where it
                // was found: handling it in place re-enters the rebuild, and a client can
                // queue more loads than this thread has stack for
                Ok(command) => {
                    let mut pending = Some(command);
                    while let Some(command) = pending {
                        pending = self.handle(command, requests);
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            if self.stopping {
                break;
            }
            self.notice_end_of_audio();
            self.publish();
        }
        self.output.player.stop();
        self.playback = PlaybackState::Stopped;
        self.loaded = None;
        self.publish();
    }

    /// Runs one command, returning the command that abandoned a rebuild it started, if
    /// any, for the caller to dispatch in turn.
    fn handle(
        &mut self,
        command: AudioCommand,
        requests: &Receiver<AudioCommand>,
    ) -> Option<AudioCommand> {
        match command {
            AudioCommand::Load {
                episode,
                local_path,
                start_secs,
            } => self.load(episode, local_path, start_secs, requests),
            AudioCommand::Play => {
                self.set_paused(false);
                None
            }
            AudioCommand::Pause => {
                self.set_paused(true);
                None
            }
            AudioCommand::Toggle => {
                self.set_paused(!self.want_paused);
                None
            }
            AudioCommand::Stop => {
                self.release();
                None
            }
            AudioCommand::Seek { target_secs } => self.seek(target_secs, requests),
            AudioCommand::Shutdown => {
                self.stopping = true;
                None
            }
        }
    }

    /// Turns a requested target into what the engine should do about it.
    ///
    /// The only place this normalisation lives. Both `seek` and `drain_while_seeking`
    /// accept a target straight from a client, and the two carrying their own copies of
    /// the rule is how a superseding seek came to skip the end of an episode and reach
    /// the decoder unbounded.
    fn resolve(&self, target_secs: f64) -> SeekOutcome {
        // `max` yields the operand that is not NaN, so a nonsensical target lands at the
        // start rather than propagating into a position nothing can hold
        let target = target_secs.max(0.0);
        let duration = self
            .loaded
            .as_ref()
            .map_or(0.0, |loaded| loaded.episode.duration_secs as f64);
        if duration > 0.0 && target >= duration {
            SeekOutcome::EndOfEpisode
        } else {
            SeekOutcome::At(target)
        }
    }

    fn load(
        &mut self,
        episode: Box<Episode>,
        local_path: Option<PathBuf>,
        start_secs: f64,
        requests: &Receiver<AudioCommand>,
    ) -> Option<AudioCommand> {
        self.discard_chain();
        self.error = None;
        self.seek_target_secs = None;
        self.want_paused = false;
        self.playback = PlaybackState::Loading;

        let duration = episode.duration_secs as f64;
        let start = if duration > 0.0 {
            start_secs.clamp(0.0, (duration - 1.0).max(0.0))
        } else {
            start_secs.max(0.0)
        };

        self.loaded = Some(Loaded {
            episode: Arc::from(episode),
            local_path,
            kind: None,
            seekable: false,
            range_support: None,
        });
        self.publish();

        if let Some(abandoned) = self.rebuild_and_install(start, requests) {
            return Some(abandoned);
        }

        // A resume position cannot be honoured by a source that turned out not to seek
        // Streaming it sequentially from the start beats refusing to play it at all
        if start > 0.0
            && self
                .error
                .as_ref()
                .is_some_and(|error| error.code == ErrorCode::SeekUnsupported)
        {
            tracing::debug!("resume position dropped: this source does not seek");
            self.error = None;
            self.playback = PlaybackState::Loading;
            return self.rebuild_and_install(0.0, requests);
        }
        None
    }

    fn seek(
        &mut self,
        target_secs: f64,
        requests: &Receiver<AudioCommand>,
    ) -> Option<AudioCommand> {
        let seekable = self.loaded.as_ref().map(|loaded| loaded.seekable)?;
        if !seekable {
            self.fail(&Error::SeekUnsupported);
            return None;
        }

        let SeekOutcome::At(target) = self.resolve(target_secs) else {
            // At or past the end is the end of the episode, not an error and not a stall
            self.release();
            return None;
        };

        self.seek_target_secs = Some(target);
        self.playback = PlaybackState::Seeking;
        self.publish();

        if self.seek_in_place(target) {
            return None;
        }
        self.rebuild_and_install(target, requests)
    }

    /// The forward-only fast path.
    ///
    /// Returns false when the seek has to be rebuilt instead, which is every backward
    /// target and any target the player refused. An error here is a trigger to rebuild,
    /// never something the caller sees: it is exactly the condition that leaves the
    /// decoder wedged, and the rebuild throws that decoder away.
    fn seek_in_place(&mut self, target_secs: f64) -> bool {
        if seek::plan(self.position_secs(), target_secs) != SeekPlan::InPlaceThenRebuild {
            return false;
        }
        // `try_seek` reports success without doing anything when the queue is empty, so
        // an empty player has to take the rebuild path or the position would start lying
        if self.output.player.empty() {
            return false;
        }
        let decoder_target = target_secs - self.base_offset_secs;
        // A target no `Duration` can hold takes the rebuild path, which reports the
        // refusal once rather than having this path grow an error channel of its own
        let Ok(decoder_target) = seek::duration_from_secs(decoder_target) else {
            return false;
        };

        match self.output.player.try_seek(decoder_target) {
            Ok(()) => {
                self.seek_target_secs = None;
                // The same rule `discard_chain` keeps: audio from before the jump must not
                // be analysed alongside the audio that replaced it
                self.tap.clear();
                self.playback = self.transport_state();
                true
            }
            Err(error) => {
                tracing::debug!(%error, "in-place seek refused, rebuilding the chain");
                false
            }
        }
    }

    /// Builds the decoder chain at `target_secs` and installs it, starting over when a
    /// later seek superseded the target while the rebuild was running.
    fn rebuild_and_install(
        &mut self,
        mut target_secs: f64,
        requests: &Receiver<AudioCommand>,
    ) -> Option<AudioCommand> {
        let (episode, local_path, mut range_support) = self.loaded.as_ref().map(|loaded| {
            (
                loaded.episode.clone(),
                loaded.local_path.clone(),
                loaded.range_support,
            )
        })?;

        loop {
            let chain = match seek::rebuild_at(
                &self.runtime,
                &episode,
                local_path.as_deref(),
                target_secs,
                range_support,
            ) {
                Ok(chain) => chain,
                Err(error) => {
                    self.fail(&error);
                    return None;
                }
            };
            if chain.kind == Source::Stream {
                range_support = Some(chain.seekable);
            }

            match self.drain_while_seeking(requests) {
                Supersede::None => {
                    self.install(chain);
                    return None;
                }
                Supersede::Target(next) => {
                    target_secs = next;
                    self.seek_target_secs = Some(next);
                    self.publish();
                }
                Supersede::Abandon(command) => {
                    drop(chain);
                    self.seek_target_secs = None;
                    return Some(command);
                }
            }
        }
    }

    /// Consumes whatever arrived while a rebuild was running.
    ///
    /// Transport commands apply at once and leave the seek alone, so pausing mid-seek
    /// arrives at the target paused. A later seek replaces the target, normalised by the
    /// same rule `seek` applies. Stopping, loading another episode, or shutting down
    /// abandons the seek outright, and that command is carried back out to be dispatched.
    fn drain_while_seeking(&mut self, requests: &Receiver<AudioCommand>) -> Supersede {
        let mut target = None;
        loop {
            match requests.try_recv() {
                Ok(AudioCommand::Seek { target_secs }) => target = Some(target_secs),
                Ok(AudioCommand::Play) => self.want_paused = false,
                Ok(AudioCommand::Pause) => self.want_paused = true,
                Ok(AudioCommand::Toggle) => self.want_paused = !self.want_paused,
                // Anything else ends this seek. It is carried out rather than run here so
                // that the rebuild which found it unwinds first
                Ok(command) => return Supersede::Abandon(command),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
        match target {
            // Normalised by the same rule `seek` applies, so a target past the end still
            // finishes the episode rather than reaching the decoder as a seek that fails
            Some(target) => match self.resolve(target) {
                SeekOutcome::At(target) => Supersede::Target(target),
                SeekOutcome::EndOfEpisode => Supersede::Abandon(AudioCommand::Stop),
            },
            None => Supersede::None,
        }
    }

    fn install(&mut self, chain: seek::Chain) {
        self.discard_chain();
        self.base_offset_secs = chain.base_offset_secs;
        let (kind, seekable) = (chain.kind, chain.seekable);
        self.output
            .player
            .append(Tap::new(chain.decoder, self.tap.clone()));
        if let Some(loaded) = self.loaded.as_mut() {
            loaded.kind = Some(kind);
            loaded.seekable = seekable;
            if kind == Source::Stream {
                loaded.range_support = Some(seekable);
            }
        }
        self.seek_target_secs = None;
        self.error = None;
        if self.want_paused {
            self.output.player.pause();
        } else {
            self.output.player.play();
        }
        self.playback = self.transport_state();
    }

    /// Drops whatever the player is holding.
    ///
    /// `clear` waits for the queue to flush, and a paused source still yields silence to
    /// the device, so it is safe to call in either transport state - but it is called
    /// unpaused so the flush can never wait on a source nothing is pulling from.
    fn discard_chain(&mut self) {
        self.output.player.play();
        self.output.player.clear();
        self.base_offset_secs = 0.0;
        // Audio from before the teardown must not be analysed alongside what replaces it
        self.tap.clear();
    }

    fn set_paused(&mut self, paused: bool) {
        if self.loaded.is_none() {
            return;
        }
        self.want_paused = paused;
        if paused {
            self.output.player.pause();
        } else {
            self.output.player.play();
        }
        if matches!(
            self.playback,
            PlaybackState::Playing | PlaybackState::Paused
        ) {
            self.playback = self.transport_state();
        }
    }

    /// Ends playback and clears the loaded episode.
    ///
    /// Stopping and reaching the end of the audio land in the same place deliberately:
    /// what separates them is that a stop has its position read from the snapshot before
    /// the command is sent, and the end of an episode has nothing read at all.
    fn release(&mut self) {
        self.discard_chain();
        self.loaded = None;
        self.seek_target_secs = None;
        self.want_paused = false;
        self.error = None;
        self.playback = PlaybackState::Stopped;
    }

    /// Notices that the audio ran out. Nothing advances to another episode; the spec is
    /// explicit that the player stops.
    fn notice_end_of_audio(&mut self) {
        if !matches!(
            self.playback,
            PlaybackState::Playing | PlaybackState::Paused
        ) {
            return;
        }
        if !self.output.player.empty() {
            return;
        }
        tracing::debug!("the episode's audio ended");
        self.release();
    }

    /// Stops audio and reports the failure, keeping the last position that corresponded to
    /// audible audio. A cleared player's clock reads zero, so folding the position into the
    /// base offset first is what preserves it; audio never continues under a position that
    /// does not describe it.
    fn fail(&mut self, error: &Error) {
        let reached = self.position_secs();
        self.discard_chain();
        self.base_offset_secs = reached;
        self.seek_target_secs = None;
        self.playback = PlaybackState::Error;
        self.error = Some(ErrorObject::from(error));
        tracing::warn!(%error, "playback failed");
    }

    fn transport_state(&self) -> PlaybackState {
        if self.want_paused {
            PlaybackState::Paused
        } else {
            PlaybackState::Playing
        }
    }

    /// The position of the audio actually being produced. Every rebuild restarts the
    /// decoder's clock at zero, so this is the base offset plus the player's own clock and
    /// never the raw player position.
    fn position_secs(&self) -> f64 {
        if self.loaded.is_none() || self.playback == PlaybackState::Stopped {
            return 0.0;
        }
        reported_position(self.base_offset_secs, self.output.player.get_pos())
    }

    /// Writes the fields the audio thread owns, leaving the download manager's alone.
    fn publish(&self) {
        let Ok(mut snapshot) = self.state.lock() else {
            tracing::error!("the shared state is poisoned; playback continues unpublished");
            return;
        };
        let episode = self.loaded.as_ref().map(|loaded| &loaded.episode);
        snapshot.playback = self.playback;
        snapshot.episode = episode.map(|episode| EpisodeRef {
            slug: episode.id().into_owned(),
            title: episode.title.clone(),
            duration_secs: episode.duration_secs as f64,
        });
        snapshot.position_secs = self.position_secs();
        snapshot.seek_target_secs = self.seek_target_secs;
        snapshot.duration_secs = episode.map(|episode| episode.duration_secs as f64);
        // The total comes from the feed rather than from decoding a 441 MB file, so it is
        // always approximate
        snapshot.duration_approximate = true;
        snapshot.seekable = self.loaded.as_ref().is_some_and(|loaded| loaded.seekable);
        snapshot.source = self.loaded.as_ref().and_then(|loaded| loaded.kind);
        snapshot.error = self.error.clone();
        // [`Analysis`] writes this while playing; withdrawing it here is what makes a
        // pause or a stop take effect at once rather than at the next analysis frame
        if self.playback != PlaybackState::Playing {
            snapshot.spectrum = None;
        }
    }
}

/// The position to report for a decoder whose clock restarts at zero on every rebuild. Its
/// own function because it is the whole of the offset rule, the one place a mistake would
/// silently show the wrong time.
fn reported_position(base_offset_secs: f64, player_position: Duration) -> f64 {
    base_offset_secs + player_position.as_secs_f64()
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use mfp_core::protocol::StateSnapshot;

    use super::*;
    use crate::audio::testing;

    /// Long enough to cover a rebuild of the fixture many times over, short enough that a
    /// wrong answer is a fast failure.
    const SETTLE: Duration = Duration::from_secs(5);

    struct Harness {
        engine: Option<AudioEngine>,
        state: SharedState,
        fixture: testing::Fixture,
        _runtime: tokio::runtime::Runtime,
    }

    impl Harness {
        fn new() -> Self {
            // Multi-threaded on purpose: the audio thread reaches the network through
            // `Handle::block_on`, which cannot drive a current-thread runtime from off it
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .unwrap();
            let state: SharedState = Arc::new(Mutex::new(StateSnapshot::stopped()));
            let engine = AudioEngine::spawn_with(
                runtime.handle().clone(),
                state.clone(),
                Output::open_headless,
            )
            .unwrap();
            Self {
                engine: Some(engine),
                state,
                fixture: testing::Fixture::new(),
                _runtime: runtime,
            }
        }

        fn send(&self, command: AudioCommand) {
            self.engine.as_ref().unwrap().send(command).unwrap();
        }

        fn load(&self, start_secs: f64) {
            self.send(AudioCommand::Load {
                episode: Box::new(self.fixture.episode.clone()),
                local_path: Some(self.fixture.path.clone()),
                start_secs,
            });
        }

        fn snapshot(&self) -> StateSnapshot {
            self.state.lock().unwrap().clone()
        }

        /// Waits for the published snapshot to satisfy `settled`, returning it.
        fn settle(&self, what: &str, settled: impl Fn(&StateSnapshot) -> bool) -> StateSnapshot {
            self.settle_within(SETTLE, what, settled)
        }

        /// [`Harness::settle`] with a deadline of its own, for the streamed tests, where a
        /// distant rebuild is measured to take up to eleven seconds.
        fn settle_within(
            &self,
            patience: Duration,
            what: &str,
            settled: impl Fn(&StateSnapshot) -> bool,
        ) -> StateSnapshot {
            let deadline = std::time::Instant::now() + patience;
            loop {
                let snapshot = self.snapshot();
                if settled(&snapshot) {
                    return snapshot;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "timed out waiting for {what}; last snapshot was {snapshot:?}"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        /// Waits until playback has settled at `target`.
        ///
        /// The window matters: a predicate that only checked `position >= target` would
        /// be satisfied by the snapshot from *before* a backward seek and would pass
        /// however wrong the seek was.
        fn settled_at(&self, target: f64, playback: PlaybackState) -> StateSnapshot {
            self.settle(&format!("{playback:?} at {target}s"), |snapshot| {
                snapshot.playback == playback
                    && snapshot.seek_target_secs.is_none()
                    && snapshot.position_secs >= target
                    && snapshot.position_secs < target + 1.0
            })
        }

        fn playing_at(&self, target: f64) -> StateSnapshot {
            self.settled_at(target, PlaybackState::Playing)
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            if let Some(engine) = self.engine.take() {
                engine.shutdown();
            }
        }
    }

    #[test]
    fn the_thread_starts_and_stops_cleanly() {
        let harness = Harness::new();
        harness.settle("the initial snapshot", |snapshot| {
            snapshot.playback == PlaybackState::Stopped
        });
        harness
            .engine
            .as_ref()
            .unwrap()
            .send(AudioCommand::Pause)
            .unwrap();
        drop(harness);
    }

    #[test]
    fn a_command_sent_after_shutdown_is_refused_rather_than_panicking() {
        let mut harness = Harness::new();
        let engine = harness.engine.take().unwrap();
        let commands = engine.commands.clone();
        engine.shutdown();
        assert!(commands.send(AudioCommand::Play).is_err());
    }

    #[test]
    fn the_reported_position_is_the_base_offset_plus_the_player_clock() {
        // The decoder's clock restarts at zero after a rebuild, so a player reading 0
        // after a seek to 3600s must still report 3600s, and must advance from there
        assert_eq!(reported_position(3600.0, Duration::ZERO), 3600.0);
        assert_eq!(reported_position(3600.0, Duration::from_secs(60)), 3660.0);
        assert_eq!(reported_position(0.0, Duration::from_secs(12)), 12.0);
    }

    #[test]
    fn loading_publishes_the_episode_its_approximate_duration_and_its_source() {
        let harness = Harness::new();
        harness.load(0.0);
        let snapshot = harness.playing_at(0.0);

        let episode = snapshot.episode.unwrap();
        assert_eq!(episode.slug, "fixture");
        assert_eq!(
            snapshot.duration_secs,
            Some(f64::from(testing::DURATION_SECS))
        );
        assert!(snapshot.duration_approximate);
        assert_eq!(snapshot.source, Some(Source::Local));
        assert!(snapshot.seekable);
        assert!(snapshot.error.is_none());
    }

    /// The identifier the snapshot carries is the one the store is keyed by and the one a
    /// client passes back, so it has to be `Episode::id()` and not the enrichment slug it
    /// falls back from. An episode enrichment never reached is where the two part company.
    #[test]
    fn the_published_identifier_is_the_episode_id_even_without_a_slug() {
        let harness = Harness::new();
        let mut episode = harness.fixture.episode.clone();
        episode.slug = None;
        let id = episode.id().into_owned();
        assert!(id.starts_with("ep-"), "the fixture still has a slug: {id}");

        harness.send(AudioCommand::Load {
            episode: Box::new(episode),
            local_path: Some(harness.fixture.path.clone()),
            start_secs: 0.0,
        });
        let snapshot = harness.playing_at(0.0);

        assert_eq!(snapshot.episode.unwrap().slug, id);
    }

    /// The regression test for the bug this group exists to avoid.
    ///
    /// A backward seek on a live decoder wedges it permanently, after which every seek
    /// fails while `get_pos` keeps advancing. Driving the whole engine forward, back, and
    /// forward again proves the rebuild path is what runs and that the reported position
    /// follows the target rather than the wedged decoder's fiction.
    #[test]
    fn a_forward_then_backward_then_forward_seek_all_succeed_with_honest_positions() {
        let harness = Harness::new();
        harness.load(0.0);
        harness.playing_at(0.0);

        // The backward step is the one that used to wedge the decoder, and it is the
        // step the rebuild path always runs for; the forward steps may take the in-place
        // fast path, which `seek::plan` is unit-tested to allow only in that direction
        for target in [4.0, 1.0, 3.0] {
            harness.send(AudioCommand::Seek {
                target_secs: target,
            });
            let snapshot = harness.playing_at(target);
            assert!(snapshot.error.is_none(), "seeking to {target}s errored");
        }
    }

    #[test]
    fn a_position_after_a_seek_advances_rather_than_restarting_from_zero() {
        let harness = Harness::new();
        harness.load(0.0);
        harness.playing_at(0.0);

        harness.send(AudioCommand::Seek { target_secs: 3.0 });
        let first = harness.playing_at(3.0);
        assert!(first.position_secs >= 3.0);

        let advanced = harness.settle("the position to advance past the target", |snapshot| {
            snapshot.position_secs > first.position_secs
        });
        assert!(advanced.position_secs > 3.0);
    }

    #[test]
    fn seeking_while_paused_moves_to_the_target_and_stays_paused() {
        let harness = Harness::new();
        harness.load(0.0);
        harness.playing_at(0.0);

        harness.send(AudioCommand::Pause);
        harness.settle("the paused state", |snapshot| {
            snapshot.playback == PlaybackState::Paused
        });

        harness.send(AudioCommand::Seek { target_secs: 4.0 });
        harness.settled_at(4.0, PlaybackState::Paused);
    }

    #[test]
    fn a_seek_superseded_by_a_later_one_settles_on_the_later_target() {
        let harness = Harness::new();
        // Starting part-way in means the first seek of the pair is backward, which is
        // always a rebuild, which is the path a second seek can supersede
        harness.load(4.0);
        harness.playing_at(4.0);

        // Both are queued before the first rebuild can finish, so the first target is
        // dropped unheard rather than played
        harness.send(AudioCommand::Seek { target_secs: 1.0 });
        harness.send(AudioCommand::Seek { target_secs: 3.0 });

        let snapshot = harness.playing_at(3.0);
        assert!(snapshot.error.is_none());
        assert!(snapshot.seek_target_secs.is_none());
    }

    #[test]
    fn stopping_during_a_seek_abandons_it() {
        let harness = Harness::new();
        harness.load(4.0);
        harness.playing_at(4.0);

        // Backward, so the seek is mid-rebuild when the stop lands on it
        harness.send(AudioCommand::Seek { target_secs: 1.0 });
        harness.send(AudioCommand::Stop);

        let snapshot = harness.settle("the stop to take effect", |snapshot| {
            snapshot.playback == PlaybackState::Stopped
        });
        assert!(snapshot.episode.is_none());
        assert!(snapshot.seek_target_secs.is_none());
        assert_eq!(snapshot.position_secs, 0.0);
    }

    #[test]
    fn seeking_at_or_past_the_end_finishes_the_episode_rather_than_stalling() {
        let harness = Harness::new();
        harness.load(0.0);
        harness.playing_at(0.0);

        harness.send(AudioCommand::Seek {
            target_secs: 99_999.0,
        });
        let snapshot = harness.settle("the episode to finish", |snapshot| {
            snapshot.playback == PlaybackState::Stopped
        });
        assert!(snapshot.episode.is_none());
        assert!(snapshot.error.is_none());
    }

    #[test]
    fn a_seek_below_zero_moves_to_the_start() {
        let harness = Harness::new();
        harness.load(0.0);
        harness.playing_at(0.0);

        harness.send(AudioCommand::Seek { target_secs: 3.0 });
        harness.playing_at(3.0);

        harness.send(AudioCommand::Seek {
            target_secs: -100.0,
        });
        let snapshot = harness.playing_at(0.0);
        assert!(snapshot.error.is_none());
    }

    /// The rule `seek` applies at the end of an episode has to be the rule a seek
    /// arriving mid-rebuild gets too. `drain_while_seeking` used to carry no such rule at
    /// all, which turned the end of an episode into a decoder failure.
    #[test]
    fn a_superseding_target_past_the_end_finishes_the_episode_rather_than_erroring() {
        let harness = Harness::new();
        // Backward, so the first seek is mid-rebuild when the second lands on it
        harness.load(4.0);
        harness.playing_at(4.0);

        harness.send(AudioCommand::Seek { target_secs: 1.0 });
        harness.send(AudioCommand::Seek {
            target_secs: 99_999.0,
        });

        let snapshot = harness.settle("the episode to finish", |snapshot| {
            snapshot.playback == PlaybackState::Stopped
        });
        assert!(snapshot.episode.is_none());
        assert!(snapshot.error.is_none());
    }

    /// The critical one: a target no `Duration` can hold, on an episode whose feed entry
    /// declares no duration, so the end-of-episode rule cannot catch it first. Every
    /// conversion on the way to the decoder used to be `Duration::from_secs_f64`, which
    /// panics here and takes the audio thread - and therefore all later playback - with
    /// it. The last two lines are the ones that would have failed.
    #[test]
    fn a_target_no_duration_could_hold_is_refused_rather_than_killing_the_thread() {
        let harness = Harness::new();
        let mut undated = harness.fixture.episode.clone();
        undated.duration_secs = 0;
        harness.send(AudioCommand::Load {
            episode: Box::new(undated),
            local_path: Some(harness.fixture.path.clone()),
            start_secs: 4.0,
        });
        harness.settle("playback to start", |snapshot| {
            snapshot.playback == PlaybackState::Playing
        });

        // Backward first, so the hostile target may arrive either at `seek` directly or
        // at `drain_while_seeking` mid-rebuild; both used to reach the same panic
        harness.send(AudioCommand::Seek { target_secs: 1.0 });
        harness.send(AudioCommand::Seek { target_secs: 1e20 });

        let failed = harness.settle("the refusal", |snapshot| {
            snapshot.playback == PlaybackState::Error
        });
        assert_eq!(
            failed.error.as_ref().map(|error| error.code),
            Some(ErrorCode::InvalidParams)
        );

        harness.load(0.0);
        harness.playing_at(0.0);
    }

    /// Loading another episode mid-seek abandons the seek and runs the load. The command
    /// is returned to the run loop rather than handled where it was found, so a queue of
    /// them unwinds instead of nesting one rebuild inside the next.
    #[test]
    fn loading_another_episode_during_a_seek_abandons_it_and_loads() {
        let harness = Harness::new();
        harness.load(4.0);
        harness.playing_at(4.0);

        // Backward, so the load lands while the seek is mid-rebuild
        harness.send(AudioCommand::Seek { target_secs: 1.0 });
        let mut second = harness.fixture.episode.clone();
        second.title = "Second".into();
        harness.send(AudioCommand::Load {
            episode: Box::new(second),
            local_path: Some(harness.fixture.path.clone()),
            start_secs: 3.0,
        });

        let snapshot = harness.playing_at(3.0);
        assert!(snapshot.error.is_none());
        assert!(snapshot.seek_target_secs.is_none());
        assert_eq!(snapshot.episode.expect("an episode").title, "Second");
    }

    #[test]
    fn toggle_alternates_between_playing_and_paused() {
        let harness = Harness::new();
        harness.load(0.0);
        harness.playing_at(0.0);

        harness.send(AudioCommand::Toggle);
        harness.settle("the paused state", |snapshot| {
            snapshot.playback == PlaybackState::Paused
        });
        harness.send(AudioCommand::Toggle);
        harness.settle("the playing state", |snapshot| {
            snapshot.playback == PlaybackState::Playing
        });
    }

    #[test]
    fn a_transport_command_with_nothing_loaded_changes_nothing() {
        let harness = Harness::new();
        harness.send(AudioCommand::Toggle);
        harness.send(AudioCommand::Play);
        harness.send(AudioCommand::Pause);
        harness.send(AudioCommand::Stop);
        harness.send(AudioCommand::Seek { target_secs: 30.0 });

        let snapshot = harness.settle("the engine to stay stopped", |snapshot| {
            snapshot.playback == PlaybackState::Stopped
        });
        assert!(snapshot.episode.is_none());
        assert!(snapshot.error.is_none());

        // Loading afterwards proves the engine consumed all of that rather than dying on
        // one of the commands, which a snapshot that never changed could not tell apart
        harness.load(0.0);
        harness.playing_at(0.0);
    }

    #[test]
    fn the_end_of_the_audio_stops_playback_without_advancing_to_anything_else() {
        let harness = Harness::new();
        // Loading close to the end keeps the wait short; the fixture runs six seconds
        let start = f64::from(testing::DURATION_SECS) - 1.0;
        harness.load(start);
        // Waiting for playback first matters: without it the stopped state the assertion
        // is looking for is the one the engine started in, and the test proves nothing
        harness.playing_at(start);

        let snapshot = harness.settle("the episode to end", |snapshot| {
            snapshot.playback == PlaybackState::Stopped && snapshot.episode.is_none()
        });
        assert!(snapshot.error.is_none());
        assert_eq!(snapshot.position_secs, 0.0);
    }

    #[test]
    fn a_source_that_cannot_be_opened_enters_the_error_state_and_leaves_the_engine_usable() {
        let harness = Harness::new();
        let mut broken = harness.fixture.episode.clone();
        broken.enclosure_url = "not a url".into();
        harness.send(AudioCommand::Load {
            episode: Box::new(broken),
            local_path: None,
            start_secs: 0.0,
        });

        let failed = harness.settle("the error state", |snapshot| {
            snapshot.playback == PlaybackState::Error
        });
        assert_eq!(
            failed.error.as_ref().map(|error| error.code),
            Some(ErrorCode::PlaybackFailed)
        );
        // No chain was ever built, so there is nothing true to say about the source. A
        // guess here shows a client `stream` for an episode that might have been local
        assert!(failed.source.is_none());
        assert!(!failed.error.unwrap().message.is_empty());

        // The engine is still usable afterwards, which separates a failure from a wedge
        harness.load(0.0);
        let recovered = harness.playing_at(0.0);
        assert!(recovered.error.is_none());
    }

    #[test]
    fn a_seek_that_fails_leaves_a_later_seek_free_to_succeed() {
        let harness = Harness::new();
        harness.load(3.0);
        harness.playing_at(3.0);

        // Removing the file leaves the already-open decoder working, so the failure has
        // to fall on a rebuild - which is what a backward target always takes
        let audio = std::fs::read(&harness.fixture.path).unwrap();
        std::fs::remove_file(&harness.fixture.path).unwrap();
        harness.send(AudioCommand::Seek { target_secs: 0.5 });
        let failed = harness.settle("the failed seek", |snapshot| {
            snapshot.playback == PlaybackState::Error
        });
        assert!(failed.seek_target_secs.is_none());
        assert!(
            failed.position_secs >= 3.0,
            "the position was not preserved where the audio stopped: {}",
            failed.position_secs
        );

        // A failure must not cost the ability to seek: with the source reachable again
        // the next seek is attempted afresh and reaches its target
        std::fs::write(&harness.fixture.path, &audio).unwrap();
        harness.send(AudioCommand::Seek { target_secs: 1.0 });
        let recovered = harness.playing_at(1.0);
        assert!(recovered.error.is_none());
    }

    // == Spectrum ==

    /// The fixture's first second is silence by construction, so a spectrum test has to
    /// start where the audio has an amplitude to report.
    const AUDIBLE_START: f64 = 1.0;

    fn has_signal(snapshot: &StateSnapshot) -> bool {
        snapshot
            .spectrum
            .as_ref()
            .is_some_and(|spectrum| spectrum.0.iter().any(|&bin| bin > 0))
    }

    /// A tap holding a full window of audible audio.
    fn primed_tap() -> SampleTap {
        let tap = SampleTap::new();
        let samples: Vec<f32> = (0..super::super::spectrum::FFT_SIZE)
            .map(|index| (index % 64) as f32 / 64.0 - 0.5)
            .collect();
        let source = rodio::buffer::SamplesBuffer::new(
            std::num::NonZero::new(1).unwrap(),
            std::num::NonZero::new(44_100).unwrap(),
            samples,
        );
        Tap::new(source, tap.clone()).for_each(drop);
        tap
    }

    /// The direct check on the one condition guarding the transform. A seek in flight is
    /// the state a timing-based test could not pin down, and it must produce nothing for
    /// the same reason a pause does: nobody is hearing a signal to analyse.
    #[test]
    fn nothing_is_analysed_in_any_state_but_playing() {
        let tap = primed_tap();
        let mut analyser = Analyser::new();

        for playback in [
            PlaybackState::Stopped,
            PlaybackState::Loading,
            PlaybackState::Paused,
            PlaybackState::Seeking,
            PlaybackState::Error,
        ] {
            let state: SharedState = Arc::new(Mutex::new(StateSnapshot::stopped()));
            {
                let mut snapshot = state.lock().unwrap();
                snapshot.playback = playback;
                snapshot.spectrum = Some(mfp_core::protocol::Spectrum(vec![
                    255;
                    mfp_core::protocol::SPECTRUM_BINS
                ]));
            }
            analyse(&state, &tap, &mut analyser);
            assert!(
                state.lock().unwrap().spectrum.is_none(),
                "{playback:?} left a spectrum published"
            );
        }

        // The same call while playing does produce one, so the assertions above are the
        // guard doing its job rather than the analyser being broken
        let state: SharedState = Arc::new(Mutex::new(StateSnapshot::stopped()));
        state.lock().unwrap().playback = PlaybackState::Playing;
        analyse(&state, &tap, &mut analyser);
        assert!(state.lock().unwrap().spectrum.is_some());
    }

    #[test]
    fn a_spectrum_of_the_audible_signal_is_published_while_playing() {
        let harness = Harness::new();
        harness.load(AUDIBLE_START);
        harness.playing_at(AUDIBLE_START);

        let snapshot = harness.settle("a spectrum of the audio", has_signal);
        assert_eq!(
            snapshot.spectrum.unwrap().0.len(),
            mfp_core::protocol::SPECTRUM_BINS
        );
    }

    #[test]
    fn no_spectrum_is_produced_while_paused() {
        let harness = Harness::new();
        harness.load(AUDIBLE_START);
        harness.playing_at(AUDIBLE_START);
        harness.settle("a spectrum of the audio", has_signal);

        harness.send(AudioCommand::Pause);
        let paused = harness.settle("the spectrum to be withdrawn", |snapshot| {
            snapshot.playback == PlaybackState::Paused && snapshot.spectrum.is_none()
        });
        assert!(paused.spectrum.is_none());

        // Still absent a few analysis intervals later, which is what "no further spectra
        // are produced" means as distinct from one stale frame being cleared
        std::thread::sleep(ANALYSIS_INTERVAL * 4);
        assert!(harness.snapshot().spectrum.is_none());
    }

    #[test]
    fn no_spectrum_is_produced_once_playback_stops() {
        let harness = Harness::new();
        harness.load(AUDIBLE_START);
        harness.playing_at(AUDIBLE_START);
        harness.settle("a spectrum of the audio", has_signal);

        harness.send(AudioCommand::Stop);
        harness.settle("the spectrum to be withdrawn", |snapshot| {
            snapshot.playback == PlaybackState::Stopped && snapshot.spectrum.is_none()
        });

        std::thread::sleep(ANALYSIS_INTERVAL * 4);
        assert!(harness.snapshot().spectrum.is_none());
    }

    #[test]
    fn nothing_loaded_publishes_no_spectrum_at_all() {
        let harness = Harness::new();
        harness.settle("the initial snapshot", |snapshot| {
            snapshot.playback == PlaybackState::Stopped
        });
        std::thread::sleep(ANALYSIS_INTERVAL * 4);
        assert!(harness.snapshot().spectrum.is_none());
    }

    /// Streams the longest episode the real feed lists and exercises the whole streamed
    /// path against it: fast start, the range probe, and seeking in both directions.
    ///
    /// This is the only test that touches the network, so it is not run by default:
    /// `cargo test -p mfp-daemon -- --ignored`. The offline tests cover the same
    /// behaviour over a local fixture; what only upstream can answer is whether it still
    /// honours range requests, which is what makes seeking possible at all.
    #[test]
    #[ignore = "reaches musicforprogramming.net and datashat.net"]
    fn a_real_streamed_episode_starts_quickly_and_seeks_in_both_directions() {
        let fetch = tokio::runtime::Runtime::new().unwrap();
        let client = reqwest::Client::new();
        let mut episodes = fetch
            .block_on(mfp_core::catalog::feed::fetch(&client))
            .expect("the feed is reachable");
        episodes.sort_by_key(|episode| episode.duration_secs);
        let episode = episodes.pop().expect("the feed lists episodes");
        assert!(
            episode.duration_secs > 3_600,
            "expected a long episode, got {}s",
            episode.duration_secs
        );

        let harness = Harness::new();
        let started = std::time::Instant::now();
        harness.send(AudioCommand::Load {
            episode: Box::new(episode),
            local_path: None,
            start_secs: 0.0,
        });

        // Only the leading 64 KiB is prefetched before the decoder is handed the reader,
        // which is what keeps a 441 MB file audible this quickly
        let playing =
            harness.settle_within(Duration::from_secs(15), "audio to start", |snapshot| {
                snapshot.playback == PlaybackState::Playing && snapshot.position_secs > 0.0
            });
        let time_to_audible = started.elapsed();
        assert!(
            time_to_audible < Duration::from_secs(10),
            "audio took {time_to_audible:?} to start"
        );
        assert_eq!(playing.source, Some(Source::Stream));
        assert!(
            playing.seekable,
            "upstream stopped answering range requests with partial content"
        );

        // Forward, then backward: the pair that wedges a decoder seeked in place
        for target in [300.0, 60.0] {
            harness.send(AudioCommand::Seek {
                target_secs: target,
            });
            let snapshot = harness.settle_within(
                Duration::from_secs(30),
                &format!("the streamed seek to {target}s"),
                |snapshot| {
                    snapshot.playback == PlaybackState::Playing
                        && snapshot.seek_target_secs.is_none()
                        && snapshot.position_secs >= target
                        && snapshot.position_secs < target + 5.0
                },
            );
            assert!(snapshot.error.is_none(), "seeking to {target}s errored");
        }
    }

    /// Set on the child half of the drop test below.
    const DROP_CHILD: &str = "MFP_TEST_DROP_CHILD";

    const DROP_TEST: &str = "audio::engine::tests::dropping_the_player_writes_nothing_to_stderr";

    /// `MixerDeviceSink` starts with `log_on_drop` set, and prints its drop-time warning
    /// with `eprintln!` unless rodio's `tracing` feature is on, which it is not here. So
    /// nothing but the `log_on_drop(false)` call keeps that line off the terminal the
    /// interface is drawing into.
    ///
    /// The check runs in a child process on purpose. `libtest` redirects `eprintln!` into
    /// its own buffer, so a parent that captured file descriptor 2 would see an empty
    /// stderr whether or not the warning was suppressed - the test would pass for the
    /// wrong reason. A child run with `--nocapture` writes to a real pipe instead.
    ///
    /// Requires an audio output device, so it is not run by default:
    /// `cargo test -p mfp-daemon -- --ignored`.
    #[test]
    #[ignore = "requires an audio output device"]
    fn dropping_the_player_writes_nothing_to_stderr() {
        if std::env::var_os(DROP_CHILD).is_some() {
            drop(Output::open_device());
            return;
        }

        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", DROP_TEST, "--ignored", "--nocapture"])
            .env(DROP_CHILD, "1")
            .output()
            .expect("the test binary re-runs itself");

        assert!(
            child.status.success(),
            "the child could not open a device: {}",
            String::from_utf8_lossy(&child.stdout)
        );
        let stderr = String::from_utf8_lossy(&child.stderr);
        assert!(
            stderr.is_empty(),
            "dropping the sink wrote {stderr:?} to stderr"
        );
    }
}
