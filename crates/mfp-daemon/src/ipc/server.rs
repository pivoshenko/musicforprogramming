//! Serving clients over the socket.

use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mfp_core::error::{Error, Result};
use mfp_core::protocol::{
    Command, DownloadState, Event, EventFrame, PlaybackState, Preferences, Request, Response,
    ResultBody, StateSnapshot, parse_request,
};
use mfp_core::{Catalog, Config, Episode};
use serde::Serialize;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::watch;
use tokio::time::MissedTickBehavior;

use super::{EVENT_TICK, PUSH_POLL, SPECTRUM_PUSH_INTERVAL};
use crate::audio::engine::{AudioCommand, AudioEngine};
use crate::audio::seek;
use crate::download::DownloadManager;
use crate::download::cache::{self, EpisodeCacheState};
use crate::state::{SharedState, StateStore, lock};

/// What the socket server needs from the rest of the daemon.
///
/// Dispatch is defined against this rather than against the engine and the download manager,
/// so the wire behaviour - framing, ordering, error codes, the event tick, the lifecycle -
/// is exercised over a real socket without an audio device.
///
/// Every method returns as soon as the command is *accepted*, never waiting for audio to
/// resume or a transfer to finish; clients observe that through snapshots.
pub trait Player: Send + Sync + 'static {
    /// The complete current state. Never a delta.
    fn snapshot(&self) -> StateSnapshot;
    /// Starts the named episode, or resumes the loaded one when no identifier is given.
    fn play(&self, id: Option<&str>) -> Result<()>;
    fn pause(&self) -> Result<()>;
    fn toggle(&self) -> Result<()>;
    fn stop(&self) -> Result<()>;
    /// Exactly one of the two arguments is set; any other combination was already rejected
    /// by the protocol layer as `invalid_params`.
    fn seek(&self, position_secs: Option<f64>, delta_secs: Option<f64>) -> Result<()>;
    fn next(&self) -> Result<()>;
    fn previous(&self) -> Result<()>;
    fn download(&self, id: &str) -> Result<()>;
    fn cancel_download(&self, id: &str) -> Result<()>;
    /// Removes an episode's local copy. Not merely accepted: it answers once the files are
    /// gone, because a client's next cache scan must not still see them, and it awaits a
    /// cancelled transfer letting go of its part file - hence the only boxed future here.
    fn delete_download<'a>(&'a self, id: &'a str) -> BoxedFuture<'a, Result<()>>;
    /// The catalog the daemon holds, so clients never read its cache file themselves.
    fn list_catalog(&self) -> Result<Catalog>;
    /// Marks an episode a favourite, or unmarks it. Repeating either is not an error, so a
    /// client never has to read the set before writing to it.
    fn set_favourite(&self, id: &str, favourite: bool) -> Result<()>;
    /// The current set of favourites, by episode identifier.
    fn list_favourites(&self) -> Vec<String>;
    /// Applies the palette preference if given and returns the resulting set; given nothing,
    /// this is a read. Preferences are the interface's own: none touches playback, the
    /// catalog, or the daemon's lifetime.
    fn preferences(&self, inverted_palette: Option<bool>) -> Preferences;
    /// What has moved in the snapshot since `previous`.
    ///
    /// Answered without cloning the snapshot, so the poll that finds nothing owed - at
    /// [`PUSH_POLL`], the overwhelming majority - allocates nothing. The default is the
    /// honest answer for an implementation with no cheaper route.
    fn moved(&self, previous: &StateSnapshot) -> Moved {
        let state = self.snapshot();
        Moved::between(&state, previous)
    }

    /// Offers the position playback has reached for persistence and lets a debounced write
    /// happen. Called on the event tick, whether or not any client is connected.
    fn tick(&self);
    /// Stops audio, cancels transfers, and flushes persisted state. Called once, after the
    /// `shutdown` response is on the wire.
    fn shutdown(&self);
}

/// The most one request line may hold before it is refused.
///
/// A request is a JSON object carrying at most an episode identifier, so a mebibyte is
/// orders of magnitude more than one needs and far too little to trouble the daemon.
/// Uncapped, a client that never sends a newline grows the read buffer without bound and
/// kills the daemon - which outlives every client so that closing one cannot stop the audio.
pub const MAX_REQUEST_BYTES: usize = 1 << 20;

/// What one poll found had changed in the shared state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Moved {
    /// Any field but the spectrum differs, which is pushed on the poll that sees it.
    pub beyond_the_spectrum: bool,
    /// The spectrum differs, which is pushed no faster than [`SPECTRUM_PUSH_INTERVAL`].
    pub spectrum: bool,
}

impl Moved {
    fn between(state: &StateSnapshot, previous: &StateSnapshot) -> Self {
        Self {
            beyond_the_spectrum: differs_beyond_the_spectrum(state, previous),
            spectrum: state.spectrum != previous.spectrum,
        }
    }
}

/// A future returned from behind a `dyn` reference, which `async fn` in a trait cannot be.
pub type BoxedFuture<'a, T> = std::pin::Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// What reading one request line off a connection produced.
enum Incoming {
    /// A line within [`MAX_REQUEST_BYTES`].
    Line(String),
    /// A line over the cap. Its bytes are discarded as they arrive and the rest read and
    /// dropped, so the connection resynchronises on the next request rather than closing.
    TooLong,
    /// The client has nothing further to send.
    Closed,
}

/// One connection's request line so far, held by the caller rather than by the future that
/// fills it.
///
/// [`read_request`] is one arm of a `select!`, so a push falling due mid-request drops that
/// future where it stands. The bytes it already took are gone from the reader, so holding
/// them anywhere but here would lose them and leave the rest of the line parsed as a request
/// of its own.
#[derive(Default)]
struct Partial {
    line: Vec<u8>,
    /// Whether this line has already passed the cap, so the rest of it is being discarded.
    over: bool,
}

impl Partial {
    /// Takes what has been read, leaving the next line to start empty.
    fn take(&mut self) -> std::io::Result<Incoming> {
        let taken = std::mem::take(self);
        if taken.over {
            return Ok(Incoming::TooLong);
        }
        String::from_utf8(taken.line)
            .map(Incoming::Line)
            .map_err(std::io::Error::other)
    }
}

/// Reads one newline-terminated request, refusing one longer than [`MAX_REQUEST_BYTES`].
///
/// Spelled out rather than left to [`AsyncBufReadExt::lines`], whose buffer has no cap.
/// Cancellation safe, which is what `partial` is for.
async fn read_request(
    reader: &mut (impl AsyncBufRead + Unpin),
    partial: &mut Partial,
) -> std::io::Result<Incoming> {
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            // a client that shut its write half without a final newline still meant those bytes
            if partial.over || partial.line.is_empty() {
                *partial = Partial::default();
                return Ok(Incoming::Closed);
            }
            return partial.take();
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let taken = newline.map_or(available.len(), |at| at + 1);
        if !partial.over {
            partial
                .line
                .extend_from_slice(&available[..newline.unwrap_or(taken)]);
            if partial.line.len() > MAX_REQUEST_BYTES {
                // released at the cap rather than at the newline, so a line that never ends
                // never holds more than one read of itself
                partial.line = Vec::new();
                partial.over = true;
            }
        }
        reader.consume(taken);
        if newline.is_some() {
            return partial.take();
        }
    }
}

/// Runs the daemon until an explicit `shutdown` or a configured idle timeout.
///
/// Creates the tokio runtime, resolves and binds the socket, restores persisted state,
/// starts the audio thread, and serves clients. Returns [`ExitCode::SUCCESS`] without
/// disturbing anything when a live daemon already owns the endpoint.
pub fn run() -> anyhow::Result<ExitCode> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let config = Config::load()?;
    let socket_path = mfp_core::paths::socket_path();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    let listener = {
        let _guard = runtime.enter();
        match bind(&socket_path)? {
            Some(listener) => listener,
            // a live daemon owns the endpoint; leave it and its socket file alone
            None => return Ok(ExitCode::SUCCESS),
        }
    };

    let player = Arc::new(DaemonPlayer::start(&runtime, &config)?);
    let idle_timeout = config.idle_timeout_secs.map(Duration::from_secs);
    runtime.block_on(serve(listener, player, idle_timeout));

    if let Err(error) = std::fs::remove_file(&socket_path) {
        tracing::warn!(%error, path = %socket_path.display(), "could not remove the socket");
    }
    Ok(ExitCode::SUCCESS)
}

/// Binds the socket, treating the path as the single-instance lock.
///
/// `Ok(None)` means a connection to the existing socket succeeded, so a live daemon owns the
/// endpoint and this process should exit 0 leaving the socket file alone. A socket that exists
/// but refuses a connection is stale: unlinked and rebound automatically, with no flag and no
/// user intervention. A regular file at the path is an error and is not deleted.
///
/// Must be called inside the tokio runtime.
pub fn bind(socket_path: &Path) -> Result<Option<UnixListener>> {
    mfp_core::paths::ensure_socket_dir(socket_path)?;

    match UnixListener::bind(socket_path) {
        Ok(listener) => Ok(Some(listener)),
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
            if std::os::unix::net::UnixStream::connect(socket_path).is_ok() {
                return Ok(None);
            }
            // nothing answers, so the path is stale - but only unlink a socket, never a
            // file somebody else put there
            if !std::fs::symlink_metadata(socket_path)?
                .file_type()
                .is_socket()
            {
                return Err(Error::Internal(format!(
                    "{} exists and is not a socket",
                    socket_path.display()
                )));
            }
            std::fs::remove_file(socket_path)?;
            Ok(Some(UnixListener::bind(socket_path)?))
        }
        Err(error) => Err(error.into()),
    }
}

/// Accepts clients until an explicit `shutdown` or the idle timeout, then tears the player
/// down.
///
/// Serving outlives every client on purpose: a closed pane must not stop the audio.
pub async fn serve(
    listener: UnixListener,
    player: Arc<dyn Player>,
    idle_timeout: Option<Duration>,
) {
    let (shutdown, mut stopping) = watch::channel(false);
    let clients = Arc::new(AtomicUsize::new(0));

    // one poller decides for the whole daemon, so the spectrum's rate limit is the daemon's
    // property rather than each connection's
    let (pushes, watching) = watch::channel(Push::new(player.snapshot(), 0));
    tokio::spawn(broadcast(Arc::clone(&player), pushes, shutdown.subscribe()));

    if let Some(timeout) = idle_timeout {
        tokio::spawn(idle_monitor(
            Arc::clone(&player),
            Arc::clone(&clients),
            shutdown.clone(),
            timeout,
        ));
    }

    let mut ticker = tokio::time::interval(EVENT_TICK);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = stopping.changed() => break,
            // persistence rides the daemon's own tick and not a client's, so a position
            // goes on being recorded with nobody connected
            _ = ticker.tick() => player.tick(),
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    clients.fetch_add(1, Ordering::SeqCst);
                    tokio::spawn(serve_connection(
                        stream,
                        Arc::clone(&player),
                        shutdown.clone(),
                        Arc::clone(&clients),
                        watching.clone(),
                    ));
                }
                Err(error) => tracing::warn!(%error, "could not accept a client"),
            },
        }
    }

    player.shutdown();
}

/// Serves one client: reads request lines, answers them in order, and pushes events once
/// the connection has subscribed.
///
/// One task owns the write half, so a pushed event can never interleave into the middle of
/// a response line.
async fn serve_connection(
    stream: UnixStream,
    player: Arc<dyn Player>,
    shutdown: watch::Sender<bool>,
    clients: Arc<AtomicUsize>,
    mut pushes: watch::Receiver<Push>,
) {
    let mut stopping = shutdown.subscribe();
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut partial = Partial::default();
    let mut subscribed = false;
    let mut wants_spectrum = false;
    // everything the daemon has pushed so far predates this connection
    let mut sent_changes = pushes.borrow_and_update().changes;

    loop {
        tokio::select! {
            _ = stopping.changed() => break,
            changed = pushes.changed(), if subscribed => {
                if changed.is_err() {
                    break;
                }
                let push = pushes.borrow_and_update().clone();
                // a connection that opted out is owed nothing for a spectrum-only push, and
                // the count stays right even when the channel coalesced it with one it is owed
                if !wants_spectrum && push.changes == sent_changes {
                    continue;
                }
                sent_changes = push.changes;
                let state = (**push.state(wants_spectrum)).clone();
                let event = EventFrame { event: Event::State { state } };
                if write_line(&mut writer, &event).await.is_err() {
                    break;
                }
            }
            incoming = read_request(&mut reader, &mut partial) => {
                let line = match incoming {
                    Ok(Incoming::Line(line)) => line,
                    Ok(Incoming::TooLong) => {
                        // no identifier to answer under: the request was never parsed
                        let refusal = Response::failure(
                            None,
                            &Error::InvalidParams(format!(
                                "A request line may hold at most {MAX_REQUEST_BYTES} bytes"
                            )),
                        );
                        if write_line(&mut writer, &refusal).await.is_err() {
                            break;
                        }
                        continue;
                    }
                    Ok(Incoming::Closed) => break,
                    Err(error) => {
                        // distinguishes a client that died mid-frame from one that closed
                        // politely, which is the difference a dropped subscription turns on
                        tracing::debug!(%error, "could not read from a client");
                        break;
                    }
                };
                if line.trim().is_empty() {
                    continue;
                }
                let (response, ends_the_process) = match parse_request(&line) {
                    Ok(request) => {
                        // flags rather than a second ticker, so subscribing twice cannot
                        // double the event rate
                        if let Command::Subscribe { spectrum } = request.cmd {
                            subscribed = true;
                            wants_spectrum |= spectrum;
                        }
                        let ends = matches!(request.cmd, Command::Shutdown);
                        (dispatch(player.as_ref(), &request).await, ends)
                    }
                    Err(rejected) => (rejected.response(), false),
                };
                if write_line(&mut writer, &response).await.is_err() {
                    break;
                }
                if ends_the_process {
                    let _ = shutdown.send(true);
                }
            }
        }
    }

    clients.fetch_sub(1, Ordering::SeqCst);
}

/// One snapshot the daemon has decided every subscribed connection should see.
///
/// Both forms are built once for the whole daemon rather than per connection: a connection
/// that opted out of spectrum updates must be handed a snapshot without one, and the deep
/// copy that strips it belongs here rather than in every writer.
#[derive(Clone)]
struct Push {
    state: Arc<StateSnapshot>,
    /// The same snapshot with no spectrum, which is the same `Arc` when it had none.
    without_spectrum: Arc<StateSnapshot>,
    /// How many pushes so far were owed to something other than the spectrum. A connection
    /// that opted out writes only when this moves, which stays correct even when a slow
    /// connection sees two pushes coalesced into one.
    changes: u64,
}

impl Push {
    fn new(state: StateSnapshot, changes: u64) -> Self {
        let state = Arc::new(state);
        let without_spectrum = if state.spectrum.is_none() {
            Arc::clone(&state)
        } else {
            let mut stripped = (*state).clone();
            stripped.spectrum = None;
            Arc::new(stripped)
        };
        Self {
            state,
            without_spectrum,
            changes,
        }
    }

    fn state(&self, wants_spectrum: bool) -> &Arc<StateSnapshot> {
        if wants_spectrum {
            &self.state
        } else {
            &self.without_spectrum
        }
    }
}

/// Decides, for the whole daemon, when a subscribed connection is owed a snapshot.
///
/// Nothing writing the shared state notifies anyone, so this polls it at [`PUSH_POLL`]. A
/// change to any field but the spectrum is pushed on the poll that sees it; the fixed
/// [`EVENT_TICK`] pushes whatever the state is doing, so a client that missed a frame is
/// never left rendering from nothing; a spectrum that moved on its own waits out
/// [`SPECTRUM_PUSH_INTERVAL`] since the last push of any kind.
///
/// The spectrum's gap is measured from every push and the tick's only from pushes that
/// counted as a change. Sharing one instant is the bug this avoids: a spectrum at its cap
/// would keep resetting the tick's gap, and a subscriber that opted out would then be pushed
/// nothing at all.
async fn broadcast(
    player: Arc<dyn Player>,
    pushes: watch::Sender<Push>,
    mut stopping: watch::Receiver<bool>,
) {
    let mut ticker = tokio::time::interval(PUSH_POLL);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut last = Arc::new(player.snapshot());
    // two gaps, deliberately not one: the spectrum's is measured from the last push of any
    // kind, so it can never lift the rate above twenty a second; the tick's only from the
    // last push that counted as a change, so a continuously moving spectrum cannot postpone it
    let mut last_push = Instant::now();
    let mut last_change = Instant::now();
    let mut changes = 0u64;

    loop {
        tokio::select! {
            _ = stopping.changed() => break,
            _ = ticker.tick() => {}
        }

        // asked rather than taken: the poll that finds nothing owed is the common one, and
        // it must not deep-copy the snapshot under the lock the audio thread is writing
        let moved = player.moved(&last);
        // a real change and the fixed tick both count, and both are decided before the
        // spectrum, so a spectrum push falling due at the same moment cannot swallow the tick
        let owed = if moved.beyond_the_spectrum || last_change.elapsed() >= EVENT_TICK {
            Some(true)
        } else if moved.spectrum && last_push.elapsed() >= SPECTRUM_PUSH_INTERVAL {
            Some(false)
        } else {
            None
        };
        let Some(counts_as_a_change) = owed else {
            continue;
        };

        let now = Instant::now();
        if counts_as_a_change {
            changes += 1;
            last_change = now;
        }
        let push = Push::new(player.snapshot(), changes);
        last = Arc::clone(&push.state);
        last_push = now;
        pushes.send_replace(push);
    }
}

/// Whether these two snapshots differ in anything but the spectrum.
///
/// Spelled out field by field rather than derived. The snapshot is `#[non_exhaustive]`, so
/// the binding below takes `..` and a new field no longer breaks this build: it must be added
/// here by hand or it silently stops being pushed promptly.
fn differs_beyond_the_spectrum(state: &StateSnapshot, previous: &StateSnapshot) -> bool {
    let StateSnapshot {
        playback,
        episode,
        position_secs,
        seek_target_secs,
        duration_secs,
        duration_approximate,
        seekable,
        source,
        downloads,
        favourites,
        spectrum: _,
        error,
        ..
    } = state;

    // Exact equality is the question: whether the field moved at all, not how far. A margin
    // would swallow a real position change
    #[expect(
        clippy::float_cmp,
        reason = "a change test, not an approximate comparison"
    )]
    {
        *playback != previous.playback
            || *episode != previous.episode
            || *position_secs != previous.position_secs
            || *seek_target_secs != previous.seek_target_secs
            || *duration_secs != previous.duration_secs
            || *duration_approximate != previous.duration_approximate
            || *seekable != previous.seekable
            || *source != previous.source
            || *downloads != previous.downloads
            || *favourites != previous.favourites
            || *error != previous.error
    }
}

async fn dispatch(player: &dyn Player, request: &Request) -> Response {
    let accepted = match &request.cmd {
        Command::Play { slug } => player.play(slug.as_deref()),
        Command::Pause => player.pause(),
        Command::Toggle => player.toggle(),
        Command::Stop => player.stop(),
        Command::Seek {
            position_secs,
            delta_secs,
        } => player.seek(*position_secs, *delta_secs),
        Command::Next => player.next(),
        Command::Previous => player.previous(),
        Command::Download { slug } => player.download(slug),
        Command::CancelDownload { slug } => player.cancel_download(slug),
        Command::DeleteDownload { slug } => player.delete_download(slug).await,
        Command::Favourite { slug } => player.set_favourite(slug, true),
        Command::Unfavourite { slug } => player.set_favourite(slug, false),
        Command::ListFavourites => {
            return Response {
                id: Some(request.id),
                result: Some(ResultBody::Favourites {
                    favourites: player.list_favourites(),
                }),
                error: None,
            };
        }
        Command::Preferences { inverted_palette } => {
            return Response {
                id: Some(request.id),
                result: Some(ResultBody::Preferences {
                    preferences: player.preferences(*inverted_palette),
                }),
                error: None,
            };
        }
        Command::Status => return Response::state(request.id, player.snapshot()),
        Command::ListCatalog => {
            return match player.list_catalog() {
                Ok(catalog) => Response::catalog(request.id, catalog),
                Err(error) => Response::failure(Some(request.id), &error),
            };
        }
        // subscription is a property of the connection, and shutdown is answered before
        // anything is torn down
        Command::Subscribe { .. } | Command::Shutdown => Ok(()),
    };

    match accepted {
        Ok(()) => Response::ok(request.id),
        Err(error) => Response::failure(Some(request.id), &error),
    }
}

/// Writes one JSON value followed by a single newline.
async fn write_line<W, T>(writer: &mut W, value: &T) -> std::io::Result<()>
where
    W: AsyncWriteExt + Unpin,
    T: Serialize + ?Sized,
{
    let mut line = serde_json::to_string(value).map_err(std::io::Error::other)?;
    line.push('\n');
    writer.write_all(line.as_bytes()).await
}

/// Ends the process once nothing has been playing, downloading, or connected for the
/// configured duration. Only spawned when one is configured; unconfigured, the default,
/// idles indefinitely.
async fn idle_monitor(
    player: Arc<dyn Player>,
    clients: Arc<AtomicUsize>,
    shutdown: watch::Sender<bool>,
    timeout: Duration,
) {
    let mut ticker = tokio::time::interval(EVENT_TICK);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut idle_since: Option<Instant> = None;

    loop {
        ticker.tick().await;
        if clients.load(Ordering::SeqCst) == 0 && is_idle(&player.snapshot()) {
            if idle_since.get_or_insert_with(Instant::now).elapsed() >= timeout {
                let _ = shutdown.send(true);
                return;
            }
        } else {
            idle_since = None;
        }
    }
}

fn is_idle(state: &StateSnapshot) -> bool {
    let playing = matches!(
        state.playback,
        PlaybackState::Loading | PlaybackState::Playing | PlaybackState::Seeking
    );
    let downloading = state.downloads.iter().any(|download| {
        matches!(
            download.state,
            DownloadState::Queued | DownloadState::Running
        )
    });
    !playing && !downloading
}

/// The real player: the audio thread, the downloads, the catalog, and the durable state
/// behind one dispatch surface.
pub struct DaemonPlayer {
    state: SharedState,
    audio: AudioEngine,
    downloads: DownloadManager,
    store: StateStore,
    /// `None` when the catalog could not be served from cache or network, which every
    /// command needing it reports as `catalog_unavailable`.
    catalog: Option<Catalog>,
    audio_dir: PathBuf,
}

impl DaemonPlayer {
    /// Restores persisted state, loads the catalog, and starts the audio thread.
    ///
    /// A catalog that cannot be loaded is not fatal: the daemon still serves, reporting
    /// `catalog_unavailable` for the commands that need one.
    pub fn start(runtime: &tokio::runtime::Runtime, config: &Config) -> Result<Self> {
        let store = StateStore::load(mfp_core::paths::state_file()?);
        let mut restored = StateSnapshot::stopped();
        restored.favourites = store.favourites();
        let state: SharedState = Arc::new(Mutex::new(restored));

        let client = reqwest::Client::builder()
            .user_agent(mfp_core::USER_AGENT)
            .build()
            .map_err(|error| Error::Internal(error.to_string()))?;
        let catalog = match runtime.block_on(mfp_core::catalog::cache::load(&client, false)) {
            Ok(catalog) => Some(catalog),
            Err(error) => {
                tracing::warn!(%error, "starting without a catalog");
                None
            }
        };

        let audio_dir = mfp_core::paths::audio_cache_dir()?;
        let audio = AudioEngine::spawn(runtime.handle().clone(), Arc::clone(&state))?;
        let downloads = DownloadManager::new(Arc::clone(&state), audio_dir.clone(), config);

        Ok(Self {
            state,
            audio,
            downloads,
            store,
            catalog,
            audio_dir,
        })
    }

    fn catalog(&self) -> Result<&Catalog> {
        self.catalog.as_ref().ok_or_else(|| {
            Error::CatalogUnavailable("No catalog is available from cache or network".into())
        })
    }

    fn episode(&self, id: &str) -> Result<&Episode> {
        self.catalog()?
            .get(id)
            .ok_or_else(|| Error::UnknownEpisode(id.to_owned()))
    }

    fn loaded(&self) -> Option<String> {
        lock(&self.state)
            .episode
            .as_ref()
            .map(|episode| episode.slug.clone())
    }

    /// Offers the position the audio thread last published for the loaded episode.
    ///
    /// The snapshot holds that position: the one the engine is actually producing audio at.
    /// Only a settled transport is offered, because a position read while an episode is
    /// loading or a seek is rebuilding is not yet the episode's own, and the store reads a
    /// position near the start as a reason to forget the one it has.
    fn record_position(&self) {
        let published = {
            let state = lock(&self.state);
            match state.playback {
                PlaybackState::Playing | PlaybackState::Paused => {
                    state.episode.as_ref().map(|episode| {
                        (
                            episode.slug.clone(),
                            state.position_secs,
                            state.duration_secs,
                        )
                    })
                }
                _ => None,
            }
        };
        let Some((id, position_secs, duration_secs)) = published else {
            return;
        };
        self.store.record_position(
            &id,
            position_secs as u64,
            duration_secs.map(|duration| duration as u64),
        );
    }

    /// Records the position and writes it out without waiting for the debounce, for the
    /// transitions that end playback: pause, stop, and an episode change.
    fn persist_now(&self) {
        self.record_position();
        self.store.flush();
    }

    /// Hands the episode to the audio thread, preferring a complete local copy and
    /// resuming from the persisted position.
    fn load(&self, episode: &Episode) -> Result<()> {
        // the outgoing episode's position, while the snapshot still describes it
        self.persist_now();
        let id = episode.id().into_owned();
        let local_path = match cache::state_of(&self.audio_dir, &id) {
            EpisodeCacheState::Cached { .. } => Some(cache::audio_path(&self.audio_dir, &id)),
            _ => None,
        };
        self.store.set_last_played(&id);
        let start_secs = self.store.position(&id).unwrap_or(0);

        self.audio.send(AudioCommand::Load {
            episode: Box::new(episode.clone()),
            local_path,
            start_secs: start_secs as f64,
        })
    }

    /// Moves through the catalog from the loaded episode, wrapping at either end.
    fn step(&self, forward: bool) -> Result<()> {
        let catalog = self.catalog()?;
        let current = self.loaded().ok_or(Error::NotPlaying)?;
        let index = catalog
            .position(&current)
            .ok_or_else(|| Error::UnknownEpisode(current.clone()))?;
        let count = catalog.episodes.len();
        let next = if forward {
            (index + 1) % count
        } else {
            (index + count - 1) % count
        };
        self.load(&catalog.episodes[next])
    }
}

impl Player for DaemonPlayer {
    fn snapshot(&self) -> StateSnapshot {
        lock(&self.state).clone()
    }

    /// Compared under the lock and never cloned, so the poller's usual answer - nothing
    /// moved - costs one comparison rather than a deep copy of the snapshot, its favourites
    /// and its download entries.
    fn moved(&self, previous: &StateSnapshot) -> Moved {
        Moved::between(&lock(&self.state), previous)
    }

    fn play(&self, id: Option<&str>) -> Result<()> {
        if let Some(id) = id {
            return self.load(self.episode(id)?);
        }
        if self.loaded().is_some() {
            return self.audio.send(AudioCommand::Play);
        }

        let last_played = self.store.last_played();
        let catalog = self.catalog()?;
        let episode = match &last_played {
            Some(id) => catalog
                .get(id)
                .ok_or_else(|| Error::UnknownEpisode(id.clone()))?,
            None => catalog
                .episodes
                .first()
                .ok_or_else(|| Error::CatalogUnavailable("The catalog is empty".into()))?,
        };
        self.load(episode)
    }

    fn pause(&self) -> Result<()> {
        self.persist_now();
        self.audio.send(AudioCommand::Pause)
    }

    fn toggle(&self) -> Result<()> {
        self.persist_now();
        self.audio.send(AudioCommand::Toggle)
    }

    /// The position is read before the engine is told to stop, because a stopped engine
    /// publishes no position to read.
    fn stop(&self) -> Result<()> {
        self.persist_now();
        self.audio.send(AudioCommand::Stop)
    }

    fn seek(&self, position_secs: Option<f64>, delta_secs: Option<f64>) -> Result<()> {
        let (current_secs, loaded, seekable) = {
            let state = lock(&self.state);
            (state.position_secs, state.episode.is_some(), state.seekable)
        };
        if !loaded {
            return Err(Error::NotPlaying);
        }
        if !seekable {
            return Err(Error::SeekUnsupported);
        }

        // accepted, not awaited: a streamed rebuild can take seconds, and the client learns
        // that it landed from the snapshots rather than from this response
        let target_secs = seek::resolve_target(current_secs, position_secs, delta_secs);
        self.audio.send(AudioCommand::Seek { target_secs })
    }

    fn next(&self) -> Result<()> {
        self.step(true)
    }

    fn previous(&self) -> Result<()> {
        self.step(false)
    }

    fn download(&self, id: &str) -> Result<()> {
        self.downloads.start(self.episode(id)?)
    }

    fn cancel_download(&self, id: &str) -> Result<()> {
        self.downloads.cancel(id)
    }

    /// Evicting the episode playing from its local file is refused rather than pulling the
    /// file out from under the decoder; evicting one that is merely streaming is allowed,
    /// since the stream owes nothing to the cache.
    fn delete_download<'a>(&'a self, id: &'a str) -> BoxedFuture<'a, Result<()>> {
        Box::pin(async move {
            self.episode(id)?;
            self.downloads.evict(id).await.map(|_| ())
        })
    }

    /// Keyed by [`Episode::id`] rather than by whatever the client named the episode, so a
    /// favourite lands under the same identifier as that episode's position and is still
    /// found after a catalog refresh. Mirrored into the snapshot as well as written to disk,
    /// so a second subscribed client learns of the change from its next push without polling.
    fn set_favourite(&self, id: &str, favourite: bool) -> Result<()> {
        let id = self.episode(id)?.id().into_owned();
        self.store.set_favourite(&id, favourite);
        let favourites = self.store.favourites();
        lock(&self.state).favourites = favourites;
        Ok(())
    }

    fn list_favourites(&self) -> Vec<String> {
        self.store.favourites()
    }

    /// Deliberately not mirrored into the snapshot the way favourites are: a preference
    /// changes when a user presses a key, and nothing subscribed needs it pushed.
    fn preferences(&self, inverted_palette: Option<bool>) -> Preferences {
        if let Some(inverted) = inverted_palette {
            self.store.set_inverted_palette(inverted);
        }
        Preferences {
            inverted_palette: self.store.preferences().inverted_palette,
        }
    }

    fn list_catalog(&self) -> Result<Catalog> {
        self.catalog().cloned()
    }

    fn tick(&self) {
        self.record_position();
        self.store.tick();
    }

    fn shutdown(&self) {
        self.downloads.shutdown();
        // before the audio thread is told to stop, for the same reason as `stop`
        self.record_position();
        if let Err(error) = self.audio.send(AudioCommand::Shutdown) {
            tracing::warn!(%error, "could not stop the audio thread");
        }
        // the one place that waits for the disk: the process is about to end, so a write
        // still on the writer's queue would otherwise be lost
        if let Err(error) = self.store.sync() {
            tracing::error!(%error, "could not persist state before exit");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::AtomicBool;

    use mfp_core::protocol::{DownloadProgress, EpisodeRef, SPECTRUM_BINS, Source, Spectrum};

    use crate::state::SessionState;

    /// The debounce the test daemon runs with. Long enough that a state file observed to
    /// change at all proves the write was immediate rather than debounced.
    const TEST_DEBOUNCE: Duration = Duration::from_secs(3600);
    use serde_json::{Value, json};
    use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

    use super::*;

    /// Long enough that a hang fails the test rather than stalling the suite.
    const PATIENCE: Duration = Duration::from_secs(5);

    /// A player that records what dispatch asked of it and answers immediately.
    struct FakePlayer {
        calls: Mutex<Vec<String>>,
        state: Mutex<StateSnapshot>,
        /// `None` stands for a daemon that could not load one, which is
        /// `catalog_unavailable`.
        catalog: Mutex<Option<Catalog>>,
        favourites: Mutex<std::collections::BTreeSet<String>>,
        preferences: Mutex<Preferences>,
        stopped: AtomicBool,
    }

    impl FakePlayer {
        fn new() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                state: Mutex::new(StateSnapshot::stopped()),
                catalog: Mutex::new(Some(catalog())),
                favourites: Mutex::new(std::collections::BTreeSet::new()),
                preferences: Mutex::new(Preferences::default()),
                stopped: AtomicBool::new(false),
            }
        }

        fn drop_catalog(&self) {
            *lock(&self.catalog) = None;
        }

        fn record(&self, call: String) -> Result<()> {
            lock(&self.calls).push(call);
            Ok(())
        }

        fn calls(&self) -> Vec<String> {
            lock(&self.calls).clone()
        }

        fn set_state(&self, state: StateSnapshot) {
            *lock(&self.state) = state;
        }

        /// Moves the spectrum and nothing else, the only thing that may be rate limited.
        fn set_spectrum(&self, spectrum: Spectrum) {
            lock(&self.state).spectrum = Some(spectrum);
        }

        fn was_shut_down(&self) -> bool {
            self.stopped.load(Ordering::SeqCst)
        }
    }

    impl Player for FakePlayer {
        fn snapshot(&self) -> StateSnapshot {
            lock(&self.state).clone()
        }

        fn play(&self, id: Option<&str>) -> Result<()> {
            if id == Some("nope") {
                return Err(Error::UnknownEpisode("nope".into()));
            }
            self.record(format!("play:{id:?}"))
        }

        fn pause(&self) -> Result<()> {
            self.record("pause".into())
        }

        fn toggle(&self) -> Result<()> {
            self.record("toggle".into())
        }

        fn stop(&self) -> Result<()> {
            self.record("stop".into())
        }

        fn seek(&self, position_secs: Option<f64>, delta_secs: Option<f64>) -> Result<()> {
            self.record(format!("seek:{position_secs:?}:{delta_secs:?}"))
        }

        fn next(&self) -> Result<()> {
            self.record("next".into())
        }

        fn previous(&self) -> Result<()> {
            self.record("previous".into())
        }

        fn download(&self, id: &str) -> Result<()> {
            self.record(format!("download:{id}"))
        }

        fn cancel_download(&self, id: &str) -> Result<()> {
            if id == "quiet" {
                return Err(Error::DownloadNotActive(id.into()));
            }
            self.record(format!("cancel_download:{id}"))
        }

        fn delete_download<'a>(&'a self, id: &'a str) -> BoxedFuture<'a, Result<()>> {
            Box::pin(async move {
                if id == "playing" {
                    return Err(Error::Internal(format!(
                        "{id} is playing from its local file and cannot be evicted"
                    )));
                }
                self.record(format!("delete_download:{id}"))
            })
        }

        fn set_favourite(&self, id: &str, favourite: bool) -> Result<()> {
            if id == "nope" {
                return Err(Error::UnknownEpisode("nope".into()));
            }
            if favourite {
                lock(&self.favourites).insert(id.to_owned());
            } else {
                lock(&self.favourites).remove(id);
            }
            self.record(format!("set_favourite:{id}:{favourite}"))
        }

        fn list_favourites(&self) -> Vec<String> {
            lock(&self.favourites).iter().cloned().collect()
        }

        fn preferences(&self, inverted_palette: Option<bool>) -> Preferences {
            let mut held = lock(&self.preferences);
            if let Some(inverted) = inverted_palette {
                held.inverted_palette = inverted;
            }
            *held
        }

        fn list_catalog(&self) -> Result<Catalog> {
            let catalog = lock(&self.catalog)
                .clone()
                .ok_or_else(|| Error::CatalogUnavailable("no catalog".into()))?;
            self.record("list_catalog".into())?;
            Ok(catalog)
        }

        fn tick(&self) {}

        fn shutdown(&self) {
            self.stopped.store(true, Ordering::SeqCst);
        }
    }

    /// A one-episode catalog, enough to assert the wire shape of a `list_catalog` result.
    fn catalog() -> Catalog {
        Catalog {
            info: Vec::new(),
            episodes: vec![Episode {
                bundle_title: None,
                special: false,
                title: "Episode 79".into(),
                link: "https://musicforprogramming.net/seventynine".into(),
                enclosure_url: "https://datashat.net/music_for_programming_79.mp3".into(),
                byte_len: 441_000_000,
                duration_secs: 14_400,
                published_at: 1_700_000_000,
                slug: Some("seventynine".into()),
                order: Some(79),
                tracklist: None,
                body: None,
                links: None,
            }],
            fetched_at: 1_700_000_000,
            enriched: true,
        }
    }

    /// A snapshot with everything populated, so completeness assertions are meaningful.
    fn playing() -> StateSnapshot {
        let mut snapshot = StateSnapshot::stopped();
        snapshot.playback = PlaybackState::Playing;
        snapshot.episode = Some(EpisodeRef {
            slug: "seventynine".into(),
            title: "Episode 79".into(),
            duration_secs: 14_400.0,
        });
        snapshot.position_secs = 1800.0;
        snapshot.duration_secs = Some(14_400.0);
        snapshot.duration_approximate = true;
        snapshot.seekable = true;
        snapshot.source = Some(Source::Stream);
        snapshot.downloads = vec![DownloadProgress {
            slug: "seventyeight".into(),
            downloaded_bytes: 1024,
            total_bytes: 4096,
            state: DownloadState::Running,
            error: None,
        }];
        snapshot
    }

    struct TestDaemon {
        path: PathBuf,
        player: Arc<FakePlayer>,
        serving: tokio::task::JoinHandle<()>,
        _dir: tempfile::TempDir,
    }

    impl TestDaemon {
        async fn start() -> Self {
            Self::start_with(None).await
        }

        async fn start_with(idle_timeout: Option<Duration>) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("mfp").join("daemon.sock");
            let listener = bind(&path).unwrap().expect("a fresh path binds");
            let player = Arc::new(FakePlayer::new());
            let serving = tokio::spawn(serve(
                listener,
                Arc::clone(&player) as Arc<dyn Player>,
                idle_timeout,
            ));
            Self {
                path,
                player,
                serving,
                _dir: dir,
            }
        }

        async fn client(&self) -> TestClient {
            TestClient::connect(&self.path).await
        }
    }

    struct TestClient {
        lines: tokio::io::Lines<BufReader<OwnedReadHalf>>,
        writer: OwnedWriteHalf,
    }

    impl TestClient {
        async fn connect(path: &Path) -> Self {
            let (reader, writer) = UnixStream::connect(path).await.unwrap().into_split();
            Self {
                lines: BufReader::new(reader).lines(),
                writer,
            }
        }

        async fn send(&mut self, line: &str) {
            self.writer.write_all(line.as_bytes()).await.unwrap();
            self.writer.write_all(b"\n").await.unwrap();
        }

        /// The next line, parsed. Fails rather than hanging when nothing arrives.
        async fn recv(&mut self) -> Value {
            let line = tokio::time::timeout(PATIENCE, self.lines.next_line())
                .await
                .expect("the daemon answered nothing")
                .unwrap()
                .expect("the daemon closed the connection");
            serde_json::from_str(&line).unwrap()
        }

        async fn request(&mut self, line: &str) -> Value {
            self.send(line).await;
            self.recv().await
        }
    }

    /// The state on disk once the daemon's writer has landed something satisfying `settled`.
    ///
    /// Persistence runs on a thread of its own so an `fsync` never blocks the runtime, so a
    /// test asserting that a command wrote *now* waits for the write rather than racing it.
    /// [`TEST_DEBOUNCE`] is set far beyond this deadline, so a write that waited for the
    /// debounce still fails the test however loaded the machine.
    fn settles(path: &Path, settled: impl Fn(&SessionState) -> bool) -> SessionState {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let session = SessionState::read(path);
            if settled(&session) {
                return session;
            }
            assert!(
                Instant::now() < deadline,
                "the daemon never wrote {}",
                path.display()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn assert_complete(state: &Value) {
        let object = state.as_object().expect("a snapshot is an object");
        for field in [
            "playback",
            "episode",
            "position_secs",
            "seek_target_secs",
            "duration_secs",
            "duration_approximate",
            "seekable",
            "source",
            "downloads",
            "favourites",
            "error",
        ] {
            assert!(object.contains_key(field), "snapshot is missing {field}");
        }
    }

    #[tokio::test]
    async fn a_request_gets_exactly_the_response_line_the_spec_names() {
        let daemon = TestDaemon::start().await;
        let mut client = daemon.client().await;

        let response = client.request(r#"{"id":1,"cmd":{"type":"pause"}}"#).await;

        assert_eq!(response, json!({"id": 1, "result": {"type": "ok"}}));
        assert_eq!(daemon.player.calls(), vec!["pause"]);
    }

    #[tokio::test]
    async fn pipelined_requests_are_answered_in_order_with_their_own_ids() {
        let daemon = TestDaemon::start().await;
        let mut client = daemon.client().await;

        client.send(r#"{"id":42,"cmd":{"type":"pause"}}"#).await;
        client.send(r#"{"id":7,"cmd":{"type":"next"}}"#).await;
        client.send(r#"{"id":9,"cmd":{"type":"previous"}}"#).await;

        let ids: Vec<i64> = vec![
            client.recv().await["id"].as_i64().unwrap(),
            client.recv().await["id"].as_i64().unwrap(),
            client.recv().await["id"].as_i64().unwrap(),
        ];
        assert_eq!(ids, vec![42, 7, 9]);
        assert_eq!(daemon.player.calls(), vec!["pause", "next", "previous"]);
    }

    #[tokio::test]
    async fn an_unparseable_line_is_rejected_without_closing_the_connection() {
        let daemon = TestDaemon::start().await;
        let mut client = daemon.client().await;

        let rejected = client.request("not json").await;

        assert_eq!(rejected["id"], Value::Null);
        assert_eq!(rejected["error"]["code"], "invalid_request");

        let response = client.request(r#"{"id":1,"cmd":{"type":"pause"}}"#).await;
        assert_eq!(response["result"]["type"], "ok");
    }

    /// Without a cap the read buffer grows with whatever the client sends, so a client that
    /// never writes a newline takes the daemon down, and the audio it keeps playing with it.
    #[tokio::test]
    async fn a_request_line_over_the_cap_is_refused_and_the_daemon_survives() {
        let daemon = TestDaemon::start().await;
        let mut client = daemon.client().await;

        // a megabyte and more with no newline anywhere in it
        let flood = vec![b'x'; MAX_REQUEST_BYTES + 4_096];
        client.writer.write_all(&flood).await.unwrap();
        client.writer.write_all(b"\n").await.unwrap();
        let refused = client.recv().await;

        assert_eq!(refused["error"]["code"], "invalid_params");
        assert_eq!(refused["id"], Value::Null);

        // and the connection resynchronised rather than closing, so the next request is
        // answered on the same socket
        let answered = client.request(r#"{"id":7,"cmd":{"type":"status"}}"#).await;
        assert_eq!(answered["id"], 7);
        assert_eq!(answered["result"]["type"], "state");

        // as is one on a socket opened afterwards, which is the daemon still being alive
        let mut fresh = daemon.client().await;
        let served = fresh.request(r#"{"id":8,"cmd":{"type":"status"}}"#).await;
        assert_eq!(served["id"], 8);
    }

    /// The bytes before an oversized line are released as they arrive, so the buffer never
    /// holds more than one read of a client that never stops writing.
    #[tokio::test]
    async fn an_oversized_line_is_refused_once_however_much_follows_it() {
        let daemon = TestDaemon::start().await;
        let mut client = daemon.client().await;

        for _ in 0..4 {
            client
                .writer
                .write_all(&vec![b'x'; MAX_REQUEST_BYTES])
                .await
                .unwrap();
        }
        client.writer.write_all(b"\n").await.unwrap();

        let refused = client.recv().await;
        assert_eq!(refused["error"]["code"], "invalid_params");
        let answered = client.request(r#"{"id":9,"cmd":{"type":"status"}}"#).await;
        assert_eq!(answered["id"], 9);
    }

    /// The read is one arm of a `select!`, so every push falling due while a request is
    /// half-written drops the future reading it. The bytes it had already taken are out of
    /// the reader for good, so a partial line kept inside that future would be lost and the
    /// rest of the request parsed as one of its own.
    #[tokio::test]
    async fn a_request_split_across_the_pushes_that_interrupt_it_is_still_answered() {
        let daemon = TestDaemon::start().await;
        let mut client = daemon.client().await;
        // subscribed, so the push arm fires on the daemon's own tick throughout
        client
            .request(r#"{"id":1,"cmd":{"type":"subscribe"}}"#)
            .await;

        for fragment in [r#"{"id":2,"cmd"#, r#"":{"type":"#, r#""status"}}"#] {
            client.writer.write_all(fragment.as_bytes()).await.unwrap();
            client.writer.flush().await.unwrap();
            tokio::time::sleep(EVENT_TICK * 2).await;
        }
        client.writer.write_all(b"\n").await.unwrap();

        // the events the subscription is owed arrive in between; the response is the line
        // carrying the identifier split across them, and the loop is bounded so a request
        // that never arrives fails rather than reading events for ever
        let mut answered = None;
        for _ in 0..50 {
            let line = client.recv().await;
            if line["id"] == 2 {
                answered = Some(line);
                break;
            }
        }
        let answered = answered.expect("the split request was never answered");
        assert_eq!(answered["result"]["type"], "state");
        assert!(answered.get("error").is_none() || answered["error"].is_null());
    }

    #[tokio::test]
    async fn an_unrecognised_command_is_unknown_command() {
        let daemon = TestDaemon::start().await;
        let mut client = daemon.client().await;

        let rejected = client
            .request(r#"{"id":7,"cmd":{"type":"teleport"}}"#)
            .await;

        assert_eq!(rejected["id"], 7);
        assert_eq!(rejected["error"]["code"], "unknown_command");
        assert!(daemon.player.calls().is_empty());
    }

    #[tokio::test]
    async fn arguments_outside_their_domain_are_invalid_params() {
        let daemon = TestDaemon::start().await;
        let mut client = daemon.client().await;

        for line in [
            r#"{"id":1,"cmd":{"type":"seek"}}"#,
            r#"{"id":2,"cmd":{"type":"seek","position_secs":1,"delta_secs":2}}"#,
            r#"{"id":5,"cmd":{"type":"download"}}"#,
        ] {
            let rejected = client.request(line).await;
            assert_eq!(rejected["error"]["code"], "invalid_params", "for {line}");
        }
        assert!(daemon.player.calls().is_empty());
    }

    #[tokio::test]
    async fn a_failure_from_the_player_carries_its_stable_code() {
        let daemon = TestDaemon::start().await;
        let mut client = daemon.client().await;

        let unknown = client
            .request(r#"{"id":1,"cmd":{"type":"play","slug":"nope"}}"#)
            .await;
        assert_eq!(unknown["id"], 1);
        assert_eq!(unknown["error"]["code"], "unknown_episode");
        assert!(unknown["error"]["message"].is_string());
        assert!(unknown.get("result").is_none());

        let inactive = client
            .request(r#"{"id":2,"cmd":{"type":"cancel_download","slug":"quiet"}}"#)
            .await;
        assert_eq!(inactive["error"]["code"], "download_not_active");
    }

    #[tokio::test]
    async fn every_command_reaches_the_player_over_the_socket() {
        let daemon = TestDaemon::start().await;
        let mut client = daemon.client().await;

        let cases = [
            (
                r#"{"id":1,"cmd":{"type":"play","slug":"seventynine"}}"#,
                r#"play:Some("seventynine")"#,
            ),
            (r#"{"id":2,"cmd":{"type":"play"}}"#, "play:None"),
            (r#"{"id":3,"cmd":{"type":"pause"}}"#, "pause"),
            (r#"{"id":4,"cmd":{"type":"toggle"}}"#, "toggle"),
            (r#"{"id":5,"cmd":{"type":"stop"}}"#, "stop"),
            (
                r#"{"id":6,"cmd":{"type":"seek","position_secs":5400}}"#,
                "seek:Some(5400.0):None",
            ),
            (
                r#"{"id":7,"cmd":{"type":"seek","delta_secs":-30}}"#,
                "seek:None:Some(-30.0)",
            ),
            (r#"{"id":8,"cmd":{"type":"next"}}"#, "next"),
            (r#"{"id":9,"cmd":{"type":"previous"}}"#, "previous"),
            (
                r#"{"id":13,"cmd":{"type":"download","slug":"seventynine"}}"#,
                "download:seventynine",
            ),
            (
                r#"{"id":14,"cmd":{"type":"cancel_download","slug":"seventynine"}}"#,
                "cancel_download:seventynine",
            ),
            (
                r#"{"id":15,"cmd":{"type":"delete_download","slug":"seventynine"}}"#,
                "delete_download:seventynine",
            ),
        ];

        let mut expected = Vec::new();
        for (line, call) in cases {
            let response = client.request(line).await;
            assert_eq!(response["result"]["type"], "ok", "for {line}");
            expected.push(call.to_owned());
        }
        assert_eq!(daemon.player.calls(), expected);

        // the commands that are not transport still answer over the same connection
        let status = client.request(r#"{"id":16,"cmd":{"type":"status"}}"#).await;
        assert_eq!(status["result"]["type"], "state");
        let listed = client
            .request(r#"{"id":17,"cmd":{"type":"list_catalog"}}"#)
            .await;
        assert_eq!(listed["result"]["type"], "catalog");
        let subscribed = client
            .request(r#"{"id":18,"cmd":{"type":"subscribe"}}"#)
            .await;
        assert_eq!(subscribed["result"]["type"], "ok");
    }

    #[tokio::test]
    async fn status_returns_a_complete_snapshot_without_a_prior_subscribe() {
        let daemon = TestDaemon::start().await;
        let mut client = daemon.client().await;

        let idle = client.request(r#"{"id":1,"cmd":{"type":"status"}}"#).await;
        assert_eq!(idle["result"]["type"], "state");
        assert_eq!(idle["result"]["state"]["playback"], "stopped");
        assert_eq!(idle["result"]["state"]["episode"], Value::Null);
        assert_complete(&idle["result"]["state"]);

        daemon.player.set_state(playing());
        let loaded = client.request(r#"{"id":2,"cmd":{"type":"status"}}"#).await;
        assert_eq!(loaded["result"]["state"]["playback"], "playing");
        assert_eq!(loaded["result"]["state"]["episode"]["slug"], "seventynine");
        assert_eq!(loaded["result"]["state"]["position_secs"], 1800.0);
        assert_complete(&loaded["result"]["state"]);
    }

    #[tokio::test]
    async fn evicting_the_episode_playing_from_its_local_file_is_refused_over_the_wire() {
        let daemon = TestDaemon::start().await;
        let mut client = daemon.client().await;

        let refused = client
            .request(r#"{"id":1,"cmd":{"type":"delete_download","slug":"playing"}}"#)
            .await;

        assert_eq!(refused["id"], 1);
        assert_eq!(refused["error"]["code"], "internal");
        assert!(refused["error"]["message"].is_string());
        assert!(
            daemon.player.calls().is_empty(),
            "a refused eviction must not reach the cache"
        );
    }

    #[tokio::test]
    async fn list_catalog_returns_the_catalog_the_daemon_holds() {
        let daemon = TestDaemon::start().await;
        let mut client = daemon.client().await;

        let listed = client
            .request(r#"{"id":1,"cmd":{"type":"list_catalog"}}"#)
            .await;

        assert_eq!(listed["id"], 1);
        assert_eq!(listed["result"]["type"], "catalog");
        let episodes = listed["result"]["catalog"]["episodes"]
            .as_array()
            .expect("a catalog carries an episode array");
        assert_eq!(episodes.len(), 1);
        assert_eq!(episodes[0]["slug"], "seventynine");
        assert_eq!(episodes[0]["title"], "Episode 79");
        assert_eq!(daemon.player.calls(), vec!["list_catalog"]);
    }

    #[tokio::test]
    async fn list_catalog_without_a_catalog_is_catalog_unavailable() {
        let daemon = TestDaemon::start().await;
        daemon.player.drop_catalog();
        let mut client = daemon.client().await;

        let refused = client
            .request(r#"{"id":1,"cmd":{"type":"list_catalog"}}"#)
            .await;

        assert_eq!(refused["error"]["code"], "catalog_unavailable");
        assert!(refused.get("result").is_none());
    }

    #[tokio::test]
    async fn subscribing_pushes_complete_snapshots_and_never_deltas() {
        let daemon = TestDaemon::start().await;
        daemon.player.set_state(playing());
        let mut client = daemon.client().await;

        assert_eq!(
            client
                .request(r#"{"id":8,"cmd":{"type":"subscribe"}}"#)
                .await["result"]["type"],
            "ok"
        );

        for _ in 0..3 {
            let event = client.recv().await;
            assert!(event.get("id").is_none(), "an event must carry no id");
            assert_eq!(event["event"]["type"], "state");
            let state = &event["event"]["state"];
            assert_complete(state);
            // one event alone renders the whole state
            assert_eq!(state["playback"], "playing");
            assert_eq!(state["episode"]["title"], "Episode 79");
            assert_eq!(state["downloads"][0]["slug"], "seventyeight");
        }
    }

    #[tokio::test]
    async fn events_keep_arriving_while_nothing_is_playing() {
        let daemon = TestDaemon::start().await;
        let mut client = daemon.client().await;

        client
            .request(r#"{"id":1,"cmd":{"type":"subscribe"}}"#)
            .await;

        for _ in 0..2 {
            let event = client.recv().await;
            assert_eq!(event["event"]["state"]["playback"], "stopped");
        }
    }

    #[tokio::test]
    async fn subscribing_twice_does_not_multiply_the_event_rate() {
        let daemon = TestDaemon::start().await;
        let mut client = daemon.client().await;

        client
            .request(r#"{"id":1,"cmd":{"type":"subscribe"}}"#)
            .await;
        client
            .request(r#"{"id":2,"cmd":{"type":"subscribe"}}"#)
            .await;

        // four ticks' worth of time yields about four events, not eight
        let deadline = tokio::time::Instant::now() + EVENT_TICK * 4;
        let mut events = 0;
        while let Ok(Ok(Some(_))) =
            tokio::time::timeout_at(deadline, client.lines.next_line()).await
        {
            events += 1;
        }
        assert!((3..=6).contains(&events), "{events} events in four ticks");
    }

    #[tokio::test]
    async fn responses_and_events_interleave_without_confusing_either() {
        let daemon = TestDaemon::start().await;
        let mut client = daemon.client().await;

        client
            .request(r#"{"id":1,"cmd":{"type":"subscribe"}}"#)
            .await;
        client.send(r#"{"id":99,"cmd":{"type":"pause"}}"#).await;

        let mut events = 0;
        let response = loop {
            let frame = client.recv().await;
            if frame.get("event").is_some() {
                events += 1;
                assert!(events < 20, "the response never arrived");
                continue;
            }
            break frame;
        };
        assert_eq!(response, json!({"id": 99, "result": {"type": "ok"}}));
        assert_eq!(daemon.player.calls(), vec!["pause"]);
    }

    #[tokio::test]
    async fn the_socket_directory_is_created_private() {
        let daemon = TestDaemon::start().await;

        let mode = std::fs::metadata(daemon.path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode();

        assert_eq!(mode & 0o777, 0o700);
    }

    #[tokio::test]
    async fn a_live_daemon_keeps_its_endpoint_and_the_second_process_exits() {
        let daemon = TestDaemon::start().await;

        // what a second daemon process does at startup
        assert!(bind(&daemon.path).unwrap().is_none());

        assert!(daemon.path.exists(), "the socket file was disturbed");
        let mut client = daemon.client().await;
        assert_eq!(
            client.request(r#"{"id":1,"cmd":{"type":"pause"}}"#).await["result"]["type"],
            "ok"
        );
    }

    #[tokio::test]
    async fn a_stale_socket_is_unlinked_and_rebound() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mfp").join("daemon.sock");

        // a killed daemon leaves the file behind with nothing listening, because a Unix
        // socket is never unlinked by the process that bound it going away
        drop(bind(&path).unwrap().unwrap());
        assert!(path.exists());
        assert!(std::os::unix::net::UnixStream::connect(&path).is_err());

        let listener = bind(&path).unwrap().expect("a stale socket is recovered");
        let player = Arc::new(FakePlayer::new());
        tokio::spawn(serve(
            listener,
            Arc::clone(&player) as Arc<dyn Player>,
            None,
        ));

        let mut client = TestClient::connect(&path).await;
        assert_eq!(
            client.request(r#"{"id":1,"cmd":{"type":"pause"}}"#).await["result"]["type"],
            "ok"
        );
    }

    #[tokio::test]
    async fn a_regular_file_at_the_socket_path_is_refused_and_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.sock");
        std::fs::write(&path, b"not a socket").unwrap();

        let error = bind(&path).unwrap_err();

        assert_eq!(error.code(), mfp_core::ErrorCode::Internal);
        assert!(error.to_string().contains("daemon.sock"));
        assert_eq!(std::fs::read(&path).unwrap(), b"not a socket");
    }

    #[tokio::test]
    async fn two_clients_are_served_and_one_leaving_leaves_the_daemon_alive() {
        let daemon = TestDaemon::start().await;
        let mut watcher = daemon.client().await;
        let mut short_lived = daemon.client().await;

        watcher
            .request(r#"{"id":1,"cmd":{"type":"subscribe"}}"#)
            .await;
        assert_eq!(
            short_lived
                .request(r#"{"id":2,"cmd":{"type":"next"}}"#)
                .await["result"]["type"],
            "ok"
        );

        // the second client goes away abruptly
        drop(short_lived);

        // the first keeps receiving events and the command took effect for both
        let event = watcher.recv().await;
        assert_eq!(event["event"]["type"], "state");
        assert_eq!(
            watcher.request(r#"{"id":3,"cmd":{"type":"status"}}"#).await["result"]["type"],
            "state"
        );
        assert_eq!(daemon.player.calls(), vec!["next"]);
        assert!(!daemon.serving.is_finished());
    }

    #[tokio::test]
    async fn the_daemon_outlives_its_last_client() {
        let daemon = TestDaemon::start().await;

        {
            let mut only = daemon.client().await;
            only.request(r#"{"id":1,"cmd":{"type":"play","slug":"seventynine"}}"#)
                .await;
        }
        daemon.player.set_state(playing());

        let mut later = daemon.client().await;
        let status = later.request(r#"{"id":2,"cmd":{"type":"status"}}"#).await;

        assert_eq!(status["result"]["state"]["playback"], "playing");
        assert!(!daemon.serving.is_finished());
        assert!(!daemon.player.was_shut_down());
    }

    #[tokio::test]
    async fn shutdown_is_answered_before_the_daemon_stops() {
        let daemon = TestDaemon::start().await;
        let mut client = daemon.client().await;

        let response = client
            .request(r#"{"id":9,"cmd":{"type":"shutdown"}}"#)
            .await;

        assert_eq!(response, json!({"id": 9, "result": {"type": "ok"}}));
        tokio::time::timeout(PATIENCE, daemon.serving)
            .await
            .expect("the daemon kept serving after shutdown")
            .unwrap();
        assert!(daemon.player.was_shut_down());
    }

    #[tokio::test]
    async fn an_unconfigured_daemon_never_idles_out() {
        let daemon = TestDaemon::start_with(None).await;

        tokio::time::sleep(EVENT_TICK * 6).await;

        assert!(!daemon.serving.is_finished());
        let mut client = daemon.client().await;
        assert_eq!(
            client.request(r#"{"id":1,"cmd":{"type":"status"}}"#).await["result"]["type"],
            "state"
        );
    }

    #[tokio::test]
    async fn a_configured_idle_timeout_ends_the_daemon() {
        let daemon = TestDaemon::start_with(Some(EVENT_TICK)).await;

        tokio::time::timeout(PATIENCE, daemon.serving)
            .await
            .expect("the idle timeout did not fire")
            .unwrap();
        assert!(daemon.player.was_shut_down());
    }

    #[tokio::test]
    async fn a_connected_client_and_a_download_each_hold_off_the_idle_timeout() {
        let daemon = TestDaemon::start_with(Some(EVENT_TICK * 2)).await;
        let mut client = daemon.client().await;

        client.request(r#"{"id":1,"cmd":{"type":"status"}}"#).await;
        tokio::time::sleep(EVENT_TICK * 6).await;
        assert!(
            !daemon.serving.is_finished(),
            "a client did not hold it off"
        );

        // still busy after the client leaves, so the countdown must not start yet
        daemon.player.set_state(playing());
        drop(client);
        tokio::time::sleep(EVENT_TICK * 6).await;
        assert!(
            !daemon.serving.is_finished(),
            "playback and a download did not hold it off"
        );

        daemon.player.set_state(StateSnapshot::stopped());
        tokio::time::timeout(PATIENCE, daemon.serving)
            .await
            .expect("the countdown did not restart once idle")
            .unwrap();
    }

    /// The real player over a recording audio engine, a catalog of its own, and a state file
    /// in a temporary directory.
    ///
    /// The engine is the only thing faked: the store, the snapshot, and every persistence
    /// decision are the daemon's own, which lets a test see whether a position it publishes
    /// ever reaches the disk.
    fn daemon_player(
        state_path: &Path,
        audio_dir: &Path,
    ) -> (Arc<DaemonPlayer>, std::sync::mpsc::Receiver<AudioCommand>) {
        daemon_player_with(catalog(), state_path, audio_dir)
    }

    fn daemon_player_with(
        catalog: Catalog,
        state_path: &Path,
        audio_dir: &Path,
    ) -> (Arc<DaemonPlayer>, std::sync::mpsc::Receiver<AudioCommand>) {
        // A debounce far longer than any deadline here, so `settles` returning at all proves
        // the write did not wait for it. With the real five-second debounce the two windows
        // are close enough that a loaded machine decides the outcome
        let store = StateStore::with_debounce(state_path.to_path_buf(), TEST_DEBOUNCE);
        let mut restored = StateSnapshot::stopped();
        restored.favourites = store.favourites();
        let state: SharedState = Arc::new(Mutex::new(restored));
        let (audio, commands) = AudioEngine::recording();
        let downloads = DownloadManager::new(
            Arc::clone(&state),
            audio_dir.to_path_buf(),
            &Config::default(),
        );
        let player = DaemonPlayer {
            state,
            audio,
            downloads,
            store,
            catalog: Some(catalog),
            audio_dir: audio_dir.to_path_buf(),
        };
        (Arc::new(player), commands)
    }

    /// A real daemon serving over a real socket, so a test drives the same path a client
    /// does.
    struct RealDaemon {
        socket: PathBuf,
        player: Arc<DaemonPlayer>,
        serving: tokio::task::JoinHandle<()>,
        /// Held rather than read: the daemon's commands are only accepted while it lives
        _commands: std::sync::mpsc::Receiver<AudioCommand>,
    }

    impl RealDaemon {
        fn start(directory: &Path) -> Self {
            let socket = directory.join("mfp").join("daemon.sock");
            let listener = bind(&socket).unwrap().expect("a fresh path binds");
            let (player, commands) =
                daemon_player(&directory.join("state.json"), &directory.join("audio"));
            let serving = tokio::spawn(serve(
                listener,
                Arc::clone(&player) as Arc<dyn Player>,
                None,
            ));
            Self {
                socket,
                player,
                serving,
                _commands: commands,
            }
        }

        /// What the audio thread would publish as it plays.
        fn publish(&self, state: StateSnapshot) {
            *lock(&self.player.state) = state;
        }

        /// Shuts the daemon down over the wire and waits for it to finish tearing down,
        /// which is where the final write happens.
        async fn shutdown(self) {
            let mut client = TestClient::connect(&self.socket).await;
            client
                .request(r#"{"id":99,"cmd":{"type":"shutdown"}}"#)
                .await;
            tokio::time::timeout(PATIENCE, self.serving)
                .await
                .expect("the daemon did not stop")
                .unwrap();
        }
    }

    /// Catches a daemon that never feeds its own store: nothing here touches `StateStore`
    /// until the assertions, and the position is published only while the daemon runs and is
    /// then withdrawn, so only its periodic recording can have put it on disk.
    #[tokio::test]
    async fn a_position_published_while_playing_is_written_and_resumed_after_a_restart() {
        let directory = tempfile::tempdir().unwrap();
        let state_path = directory.path().join("state.json");
        let daemon = RealDaemon::start(directory.path());

        daemon.publish(playing());
        tokio::time::sleep(EVENT_TICK * 3).await;
        // the episode is over and the engine publishes nothing further, so a daemon that
        // only looked at its snapshot on the way out would record nothing at all
        daemon.publish(StateSnapshot::stopped());
        daemon.shutdown().await;

        let session = SessionState::read(&state_path);
        assert_eq!(
            session.positions.get("seventynine").copied(),
            Some(1_800),
            "the daemon never recorded the position it was publishing: {:?}",
            session.positions
        );

        // and a daemon started afresh over that file resumes the episode there
        let (restarted, commands) = daemon_player(&state_path, &directory.path().join("audio"));
        restarted.play(Some("seventynine")).unwrap();
        let loaded = commands.recv_timeout(PATIENCE).unwrap();
        match loaded {
            AudioCommand::Load { start_secs, .. } => assert_eq!(start_secs, 1_800.0),
            other => panic!("the episode was not loaded: {other:?}"),
        }
    }

    /// The two halves have to agree on the key: a position is recorded under the identifier
    /// the snapshot carries and looked up under `Episode::id()`. An episode enrichment never
    /// reached has no slug, so it is where they would part company, leaving a position
    /// written but never found again.
    #[tokio::test]
    async fn a_position_for_an_episode_without_a_slug_is_recorded_and_found_again() {
        let directory = tempfile::tempdir().unwrap();
        let state_path = directory.path().join("state.json");
        let audio_dir = directory.path().join("audio");
        let mut episode = catalog().episodes.remove(0);
        episode.slug = None;
        let id = episode.id().into_owned();
        assert!(id.starts_with("ep-"), "the episode still has a slug: {id}");
        let catalog = Catalog {
            info: Vec::new(),
            episodes: vec![episode.clone()],
            fetched_at: 1_700_000_000,
            enriched: false,
        };

        let (player, _commands) = daemon_player_with(catalog.clone(), &state_path, &audio_dir);
        // published exactly as the audio thread publishes it, by `Episode::id()`
        let mut state = playing();
        state.episode = Some(EpisodeRef {
            slug: episode.id().into_owned(),
            title: episode.title.clone(),
            duration_secs: episode.duration_secs as f64,
        });
        *lock(&player.state) = state;
        player.tick();
        player.shutdown();

        assert_eq!(
            SessionState::read(&state_path).positions.get(&id).copied(),
            Some(1_800)
        );

        let (restarted, commands) = daemon_player_with(catalog, &state_path, &audio_dir);
        restarted.play(Some(&id)).unwrap();
        match commands.recv_timeout(PATIENCE).unwrap() {
            AudioCommand::Load { start_secs, .. } => assert_eq!(start_secs, 1_800.0),
            other => panic!("the episode was not loaded: {other:?}"),
        }
    }

    /// The poller asks what moved rather than taking a snapshot, so the poll that finds
    /// nothing owed - at [`PUSH_POLL`] almost all of them - neither allocates nor holds the
    /// lock the audio thread is writing. An override that drifted from the snapshot would
    /// stop pushing a field, silently, which is exactly what the default answer catches.
    #[tokio::test]
    async fn what_the_poller_is_told_moved_is_what_the_snapshot_says_moved() {
        let directory = tempfile::tempdir().unwrap();
        let (player, _commands) = daemon_player(
            &directory.path().join("state.json"),
            &directory.path().join("audio"),
        );
        let stopped = StateSnapshot::stopped();

        for published in [stopped.clone(), playing(), spectrum_only(&playing())] {
            *lock(&player.state) = published;
            for previous in [&stopped, &playing()] {
                let taken = player.snapshot();
                assert_eq!(
                    player.moved(previous),
                    Moved::between(&taken, previous),
                    "the poller was told something other than the snapshot says"
                );
            }
        }
    }

    /// A snapshot differing from `state` in the spectrum and in nothing else.
    fn spectrum_only(state: &StateSnapshot) -> StateSnapshot {
        let mut moved = state.clone();
        moved.spectrum = Some(Spectrum(vec![128; SPECTRUM_BINS]));
        moved
    }

    #[tokio::test]
    async fn pausing_writes_the_position_without_waiting_for_the_debounce() {
        let directory = tempfile::tempdir().unwrap();
        let state_path = directory.path().join("state.json");
        let daemon = RealDaemon::start(directory.path());
        let mut client = TestClient::connect(&daemon.socket).await;

        daemon.publish(playing());
        client.request(r#"{"id":1,"cmd":{"type":"pause"}}"#).await;

        // on disk already, rather than five seconds from now
        let session = settles(&state_path, |session| {
            session.positions.contains_key("seventynine")
        });
        assert_eq!(session.positions.get("seventynine").copied(), Some(1_800));
        drop(client);
        daemon.shutdown().await;
    }

    // == Favourites ==

    #[tokio::test]
    async fn list_favourites_answers_with_the_set_the_player_holds() {
        let daemon = TestDaemon::start().await;
        let mut client = daemon.client().await;

        let empty = client
            .request(r#"{"id":1,"cmd":{"type":"list_favourites"}}"#)
            .await;
        assert_eq!(empty["result"]["type"], "favourites");
        assert_eq!(empty["result"]["favourites"], json!([]));

        client
            .request(r#"{"id":2,"cmd":{"type":"favourite","slug":"seventynine"}}"#)
            .await;
        let listed = client
            .request(r#"{"id":3,"cmd":{"type":"list_favourites"}}"#)
            .await;

        assert_eq!(listed["result"]["favourites"], json!(["seventynine"]));
        assert_eq!(
            daemon.player.calls(),
            vec!["set_favourite:seventynine:true"]
        );
    }

    #[tokio::test]
    async fn favourite_and_unfavourite_both_reach_the_player_over_the_socket() {
        let daemon = TestDaemon::start().await;
        let mut client = daemon.client().await;

        for line in [
            r#"{"id":1,"cmd":{"type":"favourite","slug":"seventynine"}}"#,
            r#"{"id":2,"cmd":{"type":"unfavourite","slug":"seventynine"}}"#,
        ] {
            assert_eq!(client.request(line).await["result"]["type"], "ok", "{line}");
        }

        assert_eq!(
            daemon.player.calls(),
            vec![
                "set_favourite:seventynine:true",
                "set_favourite:seventynine:false"
            ]
        );
    }

    #[tokio::test]
    async fn favouriting_an_episode_reports_it_in_the_next_snapshot() {
        let directory = tempfile::tempdir().unwrap();
        let daemon = RealDaemon::start(directory.path());
        let mut client = TestClient::connect(&daemon.socket).await;

        let marked = client
            .request(r#"{"id":1,"cmd":{"type":"favourite","slug":"seventynine"}}"#)
            .await;

        assert_eq!(marked["result"]["type"], "ok");
        let status = client.request(r#"{"id":2,"cmd":{"type":"status"}}"#).await;
        assert_eq!(
            status["result"]["state"]["favourites"],
            json!(["seventynine"])
        );
        let listed = client
            .request(r#"{"id":3,"cmd":{"type":"list_favourites"}}"#)
            .await;
        assert_eq!(listed["result"]["favourites"], json!(["seventynine"]));
        drop(client);
        daemon.shutdown().await;
    }

    #[tokio::test]
    async fn marking_an_episode_already_marked_succeeds_and_changes_nothing() {
        let directory = tempfile::tempdir().unwrap();
        let daemon = RealDaemon::start(directory.path());
        let mut client = TestClient::connect(&daemon.socket).await;

        client
            .request(r#"{"id":1,"cmd":{"type":"favourite","slug":"seventynine"}}"#)
            .await;
        let again = client
            .request(r#"{"id":2,"cmd":{"type":"favourite","slug":"seventynine"}}"#)
            .await;

        assert_eq!(again["result"]["type"], "ok");
        assert!(again.get("error").is_none());
        let listed = client
            .request(r#"{"id":3,"cmd":{"type":"list_favourites"}}"#)
            .await;
        assert_eq!(listed["result"]["favourites"], json!(["seventynine"]));
        drop(client);
        daemon.shutdown().await;
    }

    #[tokio::test]
    async fn unmarking_an_episode_that_is_not_a_favourite_succeeds() {
        let directory = tempfile::tempdir().unwrap();
        let daemon = RealDaemon::start(directory.path());
        let mut client = TestClient::connect(&daemon.socket).await;

        let unmarked = client
            .request(r#"{"id":1,"cmd":{"type":"unfavourite","slug":"seventynine"}}"#)
            .await;

        assert_eq!(unmarked["result"]["type"], "ok");
        let listed = client
            .request(r#"{"id":2,"cmd":{"type":"list_favourites"}}"#)
            .await;
        assert_eq!(listed["result"]["favourites"], json!([]));
        drop(client);
        daemon.shutdown().await;
    }

    #[tokio::test]
    async fn favouriting_an_episode_not_in_the_catalog_is_unknown_episode() {
        let directory = tempfile::tempdir().unwrap();
        let daemon = RealDaemon::start(directory.path());
        let mut client = TestClient::connect(&daemon.socket).await;

        let refused = client
            .request(r#"{"id":1,"cmd":{"type":"favourite","slug":"nothing-like-it"}}"#)
            .await;

        assert_eq!(refused["error"]["code"], "unknown_episode");
        assert!(refused.get("result").is_none());
        let listed = client
            .request(r#"{"id":2,"cmd":{"type":"list_favourites"}}"#)
            .await;
        assert_eq!(listed["result"]["favourites"], json!([]));
        drop(client);
        daemon.shutdown().await;
    }

    #[tokio::test]
    async fn a_second_subscribed_client_is_pushed_the_new_favourites() {
        let directory = tempfile::tempdir().unwrap();
        let daemon = RealDaemon::start(directory.path());
        let mut watching = TestClient::connect(&daemon.socket).await;
        let mut marking = TestClient::connect(&daemon.socket).await;
        watching
            .request(r#"{"id":1,"cmd":{"type":"subscribe"}}"#)
            .await;

        marking
            .request(r#"{"id":2,"cmd":{"type":"favourite","slug":"seventynine"}}"#)
            .await;

        // never asked for it: the change arrives on the subscription alone
        let mut seen = None;
        for _ in 0..20 {
            let event = watching.recv().await;
            if event["event"]["state"]["favourites"] == json!(["seventynine"]) {
                seen = Some(event);
                break;
            }
        }
        assert!(
            seen.is_some(),
            "the other client's favourite was never pushed"
        );
        drop(watching);
        drop(marking);
        daemon.shutdown().await;
    }

    #[tokio::test]
    async fn a_favourite_is_on_disk_and_restored_by_a_restarted_daemon() {
        let directory = tempfile::tempdir().unwrap();
        let state_path = directory.path().join("state.json");
        let daemon = RealDaemon::start(directory.path());
        let mut client = TestClient::connect(&daemon.socket).await;

        client
            .request(r#"{"id":1,"cmd":{"type":"favourite","slug":"seventynine"}}"#)
            .await;

        // on disk already, rather than at shutdown
        let session = settles(&state_path, |session| {
            session.favourites.contains("seventynine")
        });
        assert!(session.favourites.contains("seventynine"));
        drop(client);
        daemon.shutdown().await;

        let (restarted, _commands) = daemon_player(&state_path, &directory.path().join("audio"));
        assert_eq!(restarted.list_favourites(), vec!["seventynine".to_owned()]);
        assert_eq!(
            restarted.snapshot().favourites,
            vec!["seventynine".to_owned()]
        );
    }

    #[tokio::test]
    async fn a_favourite_survives_a_catalog_refresh() {
        let directory = tempfile::tempdir().unwrap();
        let state_path = directory.path().join("state.json");
        let audio_dir = directory.path().join("audio");
        let (player, _commands) = daemon_player(&state_path, &audio_dir);
        player.set_favourite("seventynine", true).unwrap();
        // dropped rather than left running, so the file is complete before it is reread
        drop(player);

        // what a refresh from upstream leaves: the same episode, fetched and retitled
        let mut refreshed = catalog();
        refreshed.fetched_at = 1_800_000_000;
        refreshed.episodes[0].title = "79: Corticyte".into();
        let (refetched, _commands) = daemon_player_with(refreshed, &state_path, &audio_dir);

        assert_eq!(refetched.list_favourites(), vec!["seventynine".to_owned()]);
        assert_eq!(
            refetched.snapshot().favourites,
            vec!["seventynine".to_owned()]
        );
    }

    // == Interface Preferences ==

    #[tokio::test]
    async fn preferences_default_to_the_default_palette() {
        let directory = tempfile::tempdir().unwrap();
        let daemon = RealDaemon::start(directory.path());
        let mut client = TestClient::connect(&daemon.socket).await;

        let read = client
            .request(r#"{"id":1,"cmd":{"type":"preferences"}}"#)
            .await;

        assert_eq!(read["result"]["type"], "preferences");
        assert_eq!(read["result"]["preferences"]["inverted_palette"], false);
        drop(client);
        daemon.shutdown().await;
    }

    /// The cookie notice is gone and so is the preference recording its dismissal. A client
    /// built before that still names the field, and must be answered rather than refused.
    #[tokio::test]
    async fn a_client_still_sending_the_dismissed_notice_is_answered_rather_than_refused() {
        let directory = tempfile::tempdir().unwrap();
        let daemon = RealDaemon::start(directory.path());
        let mut client = TestClient::connect(&daemon.socket).await;

        let answered = client
            .request(
                r#"{"id":1,"cmd":{"type":"preferences","inverted_palette":true,"notice_dismissed":true}}"#,
            )
            .await;

        assert_eq!(answered["error"], Value::Null, "{answered}");
        assert_eq!(answered["result"]["type"], "preferences");
        // the preference it did name was still applied
        assert_eq!(answered["result"]["preferences"]["inverted_palette"], true);
        // and the one it named that no longer exists is answered with nothing at all
        assert_eq!(
            answered["result"]["preferences"]["notice_dismissed"],
            Value::Null
        );
        drop(client);
        daemon.shutdown().await;
    }

    #[tokio::test]
    async fn a_preference_is_on_disk_and_restored_by_a_restarted_daemon() {
        let directory = tempfile::tempdir().unwrap();
        let state_path = directory.path().join("state.json");
        let daemon = RealDaemon::start(directory.path());
        let mut client = TestClient::connect(&daemon.socket).await;

        let set = client
            .request(r#"{"id":1,"cmd":{"type":"preferences","inverted_palette":true}}"#)
            .await;
        assert_eq!(set["result"]["preferences"]["inverted_palette"], true);

        // on disk already, rather than at shutdown
        let session = settles(&state_path, |session| session.preferences.inverted_palette);
        assert!(session.preferences.inverted_palette);
        drop(client);
        daemon.shutdown().await;

        let (restarted, _commands) = daemon_player(&state_path, &directory.path().join("audio"));
        assert_eq!(
            restarted.preferences(None),
            Preferences {
                inverted_palette: true,
            }
        );
    }

    /// They are the interface's own: setting one must not disturb what is playing, and must
    /// not appear in the snapshot every subscriber is pushed.
    #[tokio::test]
    async fn a_preference_touches_neither_playback_nor_the_snapshot() {
        let directory = tempfile::tempdir().unwrap();
        let daemon = RealDaemon::start(directory.path());
        daemon.publish(playing());
        let mut client = TestClient::connect(&daemon.socket).await;

        let before = client.request(r#"{"id":1,"cmd":{"type":"status"}}"#).await;
        client
            .request(r#"{"id":2,"cmd":{"type":"preferences","inverted_palette":true}}"#)
            .await;
        let after = client.request(r#"{"id":3,"cmd":{"type":"status"}}"#).await;

        assert_eq!(before["result"]["state"], after["result"]["state"]);
        assert!(
            after["result"]["state"].get("inverted_palette").is_none(),
            "a preference leaked into the snapshot: {after}"
        );
        assert!(!daemon.serving.is_finished(), "the daemon stopped");
        drop(client);
        daemon.shutdown().await;
    }

    // == The spectrum push ==

    /// Everything one subscribed client is pushed until `deadline`.
    async fn events_until(client: &mut TestClient, deadline: tokio::time::Instant) -> Vec<Value> {
        let mut events = Vec::new();
        while let Ok(Ok(Some(line))) =
            tokio::time::timeout_at(deadline, client.lines.next_line()).await
        {
            events.push(serde_json::from_str(&line).unwrap());
        }
        events
    }

    /// Moves the spectrum far faster than the cap and changes nothing else at all.
    fn animate(player: Arc<FakePlayer>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            for frame in 0..1_000u32 {
                player.set_spectrum(Spectrum(vec![(frame % 256) as u8; SPECTRUM_BINS]));
                tokio::time::sleep(Duration::from_millis(4)).await;
            }
        })
    }

    #[tokio::test]
    async fn snapshots_caused_by_the_spectrum_alone_are_capped_at_twenty_a_second() {
        let daemon = TestDaemon::start().await;
        daemon.player.set_state(playing());
        let mut drawing = daemon.client().await;
        let mut ticking = daemon.client().await;
        drawing
            .request(r#"{"id":1,"cmd":{"type":"subscribe","spectrum":true}}"#)
            .await;
        ticking
            .request(r#"{"id":2,"cmd":{"type":"subscribe"}}"#)
            .await;

        let moving = animate(Arc::clone(&daemon.player));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        let (drawn, ticked) = tokio::join!(
            events_until(&mut drawing, deadline),
            events_until(&mut ticking, deadline)
        );
        moving.abort();

        // the second client, which opted out, is pushed the fixed tick and nothing else, so
        // everything the first one got beyond that is exactly what the spectrum caused
        let caused_by_the_spectrum = drawn.len().saturating_sub(ticked.len());
        // one second's worth at the cap, plus the frame that lands on the boundary
        assert!(
            caused_by_the_spectrum <= 21,
            "{caused_by_the_spectrum} spectrum snapshots in one second ({} and {})",
            drawn.len(),
            ticked.len(),
        );
        assert!(
            caused_by_the_spectrum >= 10,
            "the analyser was barely fed at all: {caused_by_the_spectrum}",
        );
        assert!(
            drawn
                .iter()
                .any(|event| event["event"]["state"]["spectrum"].is_string()),
            "no snapshot carried a spectrum",
        );
    }

    #[tokio::test]
    async fn the_cap_does_not_rise_with_a_second_connected_client() {
        let daemon = TestDaemon::start().await;
        daemon.player.set_state(playing());
        let mut first = daemon.client().await;
        let mut second = daemon.client().await;
        for client in [&mut first, &mut second] {
            client
                .request(r#"{"id":1,"cmd":{"type":"subscribe","spectrum":true}}"#)
                .await;
        }

        let moving = animate(Arc::clone(&daemon.player));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        let (one, two) = tokio::join!(
            events_until(&mut first, deadline),
            events_until(&mut second, deadline)
        );
        moving.abort();

        for events in [&one, &two] {
            // a second's worth at the cap, the fixed tick alongside it, and the frame that
            // lands on the boundary
            assert!(
                events.len() <= 25,
                "{} snapshots in one second with two clients attached",
                events.len()
            );
        }
        assert!(
            one.len() >= 10 && two.len() >= 10,
            "{} and {}",
            one.len(),
            two.len()
        );
    }

    /// The fixed tick is unconditional, and a spectrum moving at its cap is what would starve
    /// it if the two shared one gap: a connection that opted out counts none of those pushes,
    /// so it would be pushed nothing at all.
    #[tokio::test]
    async fn the_fixed_tick_survives_a_spectrum_that_never_stops_moving() {
        let daemon = TestDaemon::start().await;
        // every other field held completely still, so nothing but the tick can push
        daemon.player.set_state(playing());
        let mut ticking = daemon.client().await;
        ticking
            .request(r#"{"id":1,"cmd":{"type":"subscribe"}}"#)
            .await;

        let moving = animate(Arc::clone(&daemon.player));
        let events = events_until(
            &mut ticking,
            tokio::time::Instant::now() + Duration::from_secs(1),
        )
        .await;
        moving.abort();

        assert!(
            (3..=6).contains(&events.len()),
            "{} events in one second with the spectrum at its cap",
            events.len()
        );
        for event in &events {
            assert_eq!(event["event"]["type"], "state");
            assert_eq!(event["event"]["state"]["playback"], "playing");
            assert!(
                event["event"]["state"].get("spectrum").is_none(),
                "a client that opted out was sent spectrum data: {event}",
            );
        }
    }

    #[tokio::test]
    async fn a_pause_is_pushed_promptly_and_not_held_to_the_spectrum_cap() {
        let daemon = TestDaemon::start().await;
        daemon.player.set_state(playing());
        let mut client = daemon.client().await;
        client
            .request(r#"{"id":1,"cmd":{"type":"subscribe","spectrum":true}}"#)
            .await;

        // spend the spectrum's slot immediately before pausing, so a gate that also held
        // other changes back would be at its least forgiving here
        daemon.player.set_spectrum(Spectrum::silent());
        client.recv().await;
        let mut paused = playing();
        paused.playback = PlaybackState::Paused;
        let at = Instant::now();
        daemon.player.set_state(paused);

        loop {
            let event = client.recv().await;
            if event["event"]["state"]["playback"] == "paused" {
                break;
            }
        }

        let waited = at.elapsed();
        assert!(
            waited < SPECTRUM_PUSH_INTERVAL,
            "the pause waited {waited:?} for the spectrum's gate",
        );
    }

    #[tokio::test]
    async fn a_client_that_opted_out_of_the_spectrum_receives_no_spectrum_only_push() {
        let daemon = TestDaemon::start().await;
        daemon.player.set_state(playing());
        let mut animated = daemon.client().await;
        let mut indifferent = daemon.client().await;
        animated
            .request(r#"{"id":1,"cmd":{"type":"subscribe","spectrum":true}}"#)
            .await;
        indifferent
            .request(r#"{"id":2,"cmd":{"type":"subscribe"}}"#)
            .await;

        let moving = animate(Arc::clone(&daemon.player));
        let deadline = tokio::time::Instant::now() + EVENT_TICK * 4;
        let (drawn, ignored) = tokio::join!(
            events_until(&mut animated, deadline),
            events_until(&mut indifferent, deadline)
        );
        moving.abort();

        assert!(
            drawn.len() >= 10,
            "the analyser was not fed: {}",
            drawn.len()
        );
        // the fixed tick over four of them and nothing on top: every spectrum push withheld
        assert!(
            (3..=6).contains(&ignored.len()),
            "a client that opted out was pushed {} snapshots",
            ignored.len()
        );
        assert!(
            drawn.len() > ignored.len() * 2,
            "the two subscriptions were pushed alike: {} and {}",
            drawn.len(),
            ignored.len(),
        );
        for event in &ignored {
            assert!(
                event["event"]["state"].get("spectrum").is_none(),
                "a client that opted out was sent spectrum data: {event}",
            );
        }

        // and it is still pushed everything else
        let mut stopped = playing();
        stopped.playback = PlaybackState::Paused;
        daemon.player.set_state(stopped);
        loop {
            let event = indifferent.recv().await;
            if event["event"]["state"]["playback"] == "paused" {
                assert!(event["event"]["state"].get("spectrum").is_none());
                break;
            }
        }
    }
}
