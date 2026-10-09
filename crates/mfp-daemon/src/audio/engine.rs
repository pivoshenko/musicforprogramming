use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::JoinHandle;
use std::time::Duration;

use mfp_core::Episode;
use mfp_core::error::{Error, ErrorCode, Result};
use mfp_core::protocol::{EpisodeRef, ErrorObject, PlaybackState, Source};
use rodio::Source as _;
use rodio::mixer::Mixer;
use rodio::source::SquareWave;

use super::seek::{self, SeekPlan};
use super::source::{SampleTap, Tap};
use super::spectrum::Analyser;
use crate::state::SharedState;

const TICK: Duration = Duration::from_millis(100);

const ANALYSIS_INTERVAL: Duration = Duration::from_millis(50);

const CLICK_HZ: f32 = 5_555.0;

const CLICK_GAIN: f32 = 0.125;

const CLICK_LENGTH: Duration = Duration::from_millis(20);

#[derive(Debug)]
pub enum AudioCommand {
    Load {
        episode: Box<Episode>,
        local_path: Option<PathBuf>,
        start_secs: f64,
    },
    Play,
    Pause,
    Toggle,

    Stop,

    Seek {
        target_secs: f64,
    },

    Shutdown,
}

pub struct AudioEngine {
    commands: Sender<AudioCommand>,
    thread: JoinHandle<()>,
}

impl AudioEngine {
    pub fn spawn(runtime: tokio::runtime::Handle, state: SharedState) -> Result<Self> {
        Self::spawn_with(runtime, state, Output::open_device)
    }

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
                "The audio thread ended before it started".into(),
            )),
        }
    }

    pub fn send(&self, command: AudioCommand) -> Result<()> {
        self.commands
            .send(command)
            .map_err(|_| Error::PlaybackFailed("The audio thread is gone".into()))
    }

    pub fn shutdown(self) {
        let _ = self.commands.send(AudioCommand::Shutdown);
        let _ = self.thread.join();
    }

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

struct Output {
    player: rodio::Player,

    effects: Option<Mixer>,

    _sink: Option<rodio::MixerDeviceSink>,
}

impl Output {
    fn open_device() -> Result<Self> {
        let builder = rodio::DeviceSinkBuilder::from_default_device()
            .map_err(|error| Error::PlaybackFailed(format!("No audio output device: {error}")))?;
        let mut sink = builder
            .open_stream()
            .map_err(|error| Error::PlaybackFailed(format!("No audio output stream: {error}")))?;

        sink.log_on_drop(false);
        let player = rodio::Player::connect_new(sink.mixer());
        Ok(Self {
            player,
            effects: Some(sink.mixer().clone()),
            _sink: Some(sink),
        })
    }

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
            effects: None,
            _sink: None,
        })
    }

    fn click(&self) {
        if let Some(effects) = &self.effects {
            effects.add(click());
        }
    }
}

fn click() -> impl rodio::Source + Send {
    SquareWave::new(CLICK_HZ)
        .amplify(CLICK_GAIN)
        .take_duration(CLICK_LENGTH)
}

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

    snapshot.spectrum = (snapshot.playback == PlaybackState::Playing).then_some(spectrum);
}

struct Loaded {
    episode: Arc<Episode>,
    local_path: Option<PathBuf>,

    kind: Option<Source>,
    seekable: bool,

    range_support: Option<bool>,
}

enum SeekOutcome {
    At(f64),

    EndOfEpisode,
}

enum Supersede {
    None,

    Target(f64),

    Abandon(AudioCommand),
}

struct Engine {
    runtime: tokio::runtime::Handle,
    state: SharedState,
    output: Output,
    loaded: Option<Loaded>,

    base_offset_secs: f64,

    want_paused: bool,
    playback: PlaybackState,
    seek_target_secs: Option<f64>,
    error: Option<ErrorObject>,
    stopping: bool,

    tap: SampleTap,

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
                if self.loaded.is_some() {
                    self.output.click();
                }
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

    fn resolve(&self, target_secs: f64) -> SeekOutcome {
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
        self.output.click();
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
        self.output.click();

        let SeekOutcome::At(target) = self.resolve(target_secs) else {
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

    fn seek_in_place(&mut self, target_secs: f64) -> bool {
        if seek::plan(self.position_secs(), target_secs) != SeekPlan::InPlaceThenRebuild {
            return false;
        }

        if self.output.player.empty() {
            return false;
        }
        let decoder_target = target_secs - self.base_offset_secs;

        let Ok(decoder_target) = seek::duration_from_secs(decoder_target) else {
            return false;
        };

        match self.output.player.try_seek(decoder_target) {
            Ok(()) => {
                self.seek_target_secs = None;

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

    fn drain_while_seeking(&mut self, requests: &Receiver<AudioCommand>) -> Supersede {
        let mut target = None;
        loop {
            match requests.try_recv() {
                Ok(AudioCommand::Seek { target_secs }) => target = Some(target_secs),
                Ok(AudioCommand::Play) => self.want_paused = false,
                Ok(AudioCommand::Pause) => self.want_paused = true,
                Ok(AudioCommand::Toggle) => self.want_paused = !self.want_paused,

                Ok(command) => return Supersede::Abandon(command),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
        match target {
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

    fn discard_chain(&mut self) {
        self.output.player.play();
        self.output.player.clear();
        self.base_offset_secs = 0.0;

        self.tap.clear();
    }

    fn set_paused(&mut self, paused: bool) {
        if self.loaded.is_none() {
            return;
        }
        if self.want_paused != paused {
            self.output.click();
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

    fn release(&mut self) {
        self.discard_chain();
        self.loaded = None;
        self.seek_target_secs = None;
        self.want_paused = false;
        self.error = None;
        self.playback = PlaybackState::Stopped;
    }

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

    fn position_secs(&self) -> f64 {
        if self.loaded.is_none() || self.playback == PlaybackState::Stopped {
            return 0.0;
        }
        reported_position(self.base_offset_secs, self.output.player.get_pos())
    }

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

        snapshot.duration_approximate = true;
        snapshot.seekable = self.loaded.as_ref().is_some_and(|loaded| loaded.seekable);
        snapshot.source = self.loaded.as_ref().and_then(|loaded| loaded.kind);
        snapshot.error = self.error.clone();

        if self.playback != PlaybackState::Playing {
            snapshot.spectrum = None;
        }
    }
}

fn reported_position(base_offset_secs: f64, player_position: Duration) -> f64 {
    base_offset_secs + player_position.as_secs_f64()
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use mfp_core::protocol::StateSnapshot;

    use super::*;
    use crate::audio::testing;

    const SETTLE: Duration = Duration::from_secs(5);

    struct Harness {
        engine: Option<AudioEngine>,
        state: SharedState,
        fixture: testing::Fixture,
        _runtime: tokio::runtime::Runtime,
    }

    impl Harness {
        fn new() -> Self {
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

        fn settle(&self, what: &str, settled: impl Fn(&StateSnapshot) -> bool) -> StateSnapshot {
            self.settle_within(SETTLE, what, settled)
        }

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
        assert_eq!(reported_position(3600.0, Duration::ZERO), 3600.0);
        assert_eq!(reported_position(3600.0, Duration::from_secs(60)), 3660.0);
        assert_eq!(reported_position(0.0, Duration::from_secs(12)), 12.0);
    }

    #[test]
    fn the_click_is_the_sites_twenty_millisecond_square_tick() {
        let tick = click();
        assert_eq!(tick.channels().get(), 1);

        let samples: Vec<_> = tick.collect();
        assert_eq!(samples.len(), 960);
        assert!(samples.iter().all(|sample| sample.abs() <= CLICK_GAIN + f32::EPSILON));
        assert!(samples.iter().any(|sample| *sample > 0.0));
        assert!(samples.iter().any(|sample| *sample < 0.0));
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

    #[test]
    fn a_forward_then_backward_then_forward_seek_all_succeed_with_honest_positions() {
        let harness = Harness::new();
        harness.load(0.0);
        harness.playing_at(0.0);

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

        harness.load(4.0);
        harness.playing_at(4.0);

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

    #[test]
    fn a_superseding_target_past_the_end_finishes_the_episode_rather_than_erroring() {
        let harness = Harness::new();

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

    #[test]
    fn loading_another_episode_during_a_seek_abandons_it_and_loads() {
        let harness = Harness::new();
        harness.load(4.0);
        harness.playing_at(4.0);

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

        harness.load(0.0);
        harness.playing_at(0.0);
    }

    #[test]
    #[ignore = "requires an audio output device"]
    fn the_end_of_the_audio_stops_playback_without_advancing_to_anything_else() {
        let harness = Harness::new();

        let start = f64::from(testing::DURATION_SECS) - 1.0;
        harness.load(start);

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

        assert!(failed.source.is_none());
        assert!(!failed.error.unwrap().message.is_empty());

        harness.load(0.0);
        let recovered = harness.playing_at(0.0);
        assert!(recovered.error.is_none());
    }

    #[test]
    fn a_seek_that_fails_leaves_a_later_seek_free_to_succeed() {
        let harness = Harness::new();
        harness.load(3.0);
        harness.playing_at(3.0);

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

        std::fs::write(&harness.fixture.path, &audio).unwrap();
        harness.send(AudioCommand::Seek { target_secs: 1.0 });
        let recovered = harness.playing_at(1.0);
        assert!(recovered.error.is_none());
    }

    const AUDIBLE_START: f64 = 1.0;

    fn has_signal(snapshot: &StateSnapshot) -> bool {
        snapshot
            .spectrum
            .as_ref()
            .is_some_and(|spectrum| spectrum.0.iter().any(|&bin| bin > 0))
    }

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

    const DROP_CHILD: &str = "MFP_TEST_DROP_CHILD";

    const DROP_TEST: &str = "audio::engine::tests::dropping_the_player_writes_nothing_to_stderr";

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
