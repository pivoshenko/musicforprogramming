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

pub trait Player: Send + Sync + 'static {
    fn snapshot(&self) -> StateSnapshot;

    fn play(&self, id: Option<&str>) -> Result<()>;
    fn pause(&self) -> Result<()>;
    fn toggle(&self) -> Result<()>;
    fn stop(&self) -> Result<()>;

    fn seek(&self, position_secs: Option<f64>, delta_secs: Option<f64>) -> Result<()>;
    fn next(&self) -> Result<()>;
    fn previous(&self) -> Result<()>;
    fn download(&self, id: &str) -> Result<()>;
    fn cancel_download(&self, id: &str) -> Result<()>;

    fn delete_download<'a>(&'a self, id: &'a str) -> BoxedFuture<'a, Result<()>>;

    fn list_catalog(&self) -> Result<Catalog>;

    fn set_favourite(&self, id: &str, favourite: bool) -> Result<()>;

    fn list_favourites(&self) -> Vec<String>;

    fn preferences(&self, inverted_palette: Option<bool>) -> Preferences;

    fn moved(&self, previous: &StateSnapshot) -> Moved {
        let state = self.snapshot();
        Moved::between(&state, previous)
    }

    fn tick(&self);

    fn shutdown(&self);
}

pub const MAX_REQUEST_BYTES: usize = 1 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Moved {
    pub beyond_the_spectrum: bool,

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

pub type BoxedFuture<'a, T> = std::pin::Pin<Box<dyn Future<Output = T> + Send + 'a>>;

enum Incoming {
    Line(String),

    TooLong,

    Closed,
}

#[derive(Default)]
struct Partial {
    line: Vec<u8>,

    over: bool,
}

impl Partial {
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

async fn read_request(
    reader: &mut (impl AsyncBufRead + Unpin),
    partial: &mut Partial,
) -> std::io::Result<Incoming> {
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
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

pub fn bind(socket_path: &Path) -> Result<Option<UnixListener>> {
    mfp_core::paths::ensure_socket_dir(socket_path)?;

    match UnixListener::bind(socket_path) {
        Ok(listener) => Ok(Some(listener)),
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
            if std::os::unix::net::UnixStream::connect(socket_path).is_ok() {
                return Ok(None);
            }

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

pub async fn serve(
    listener: UnixListener,
    player: Arc<dyn Player>,
    idle_timeout: Option<Duration>,
) {
    let (shutdown, mut stopping) = watch::channel(false);
    let clients = Arc::new(AtomicUsize::new(0));

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

    let mut sent_changes = pushes.borrow_and_update().changes;

    loop {
        tokio::select! {
            _ = stopping.changed() => break,
            changed = pushes.changed(), if subscribed => {
                if changed.is_err() {
                    break;
                }
                let push = pushes.borrow_and_update().clone();

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

                        tracing::debug!(%error, "could not read from a client");
                        break;
                    }
                };
                if line.trim().is_empty() {
                    continue;
                }
                let (response, ends_the_process) = match parse_request(&line) {
                    Ok(request) => {

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

#[derive(Clone)]
struct Push {
    state: Arc<StateSnapshot>,

    without_spectrum: Arc<StateSnapshot>,

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

async fn broadcast(
    player: Arc<dyn Player>,
    pushes: watch::Sender<Push>,
    mut stopping: watch::Receiver<bool>,
) {
    let mut ticker = tokio::time::interval(PUSH_POLL);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut last = Arc::new(player.snapshot());

    let mut last_push = Instant::now();
    let mut last_change = Instant::now();
    let mut changes = 0u64;

    loop {
        tokio::select! {
            _ = stopping.changed() => break,
            _ = ticker.tick() => {}
        }

        let moved = player.moved(&last);

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

        Command::Subscribe { .. } | Command::Shutdown => Ok(()),
    };

    match accepted {
        Ok(()) => Response::ok(request.id),
        Err(error) => Response::failure(Some(request.id), &error),
    }
}

async fn write_line<W, T>(writer: &mut W, value: &T) -> std::io::Result<()>
where
    W: AsyncWriteExt + Unpin,
    T: Serialize + ?Sized,
{
    let mut line = serde_json::to_string(value).map_err(std::io::Error::other)?;
    line.push('\n');
    writer.write_all(line.as_bytes()).await
}

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

pub struct DaemonPlayer {
    state: SharedState,
    audio: AudioEngine,
    downloads: DownloadManager,
    store: StateStore,

    catalog: Option<Catalog>,
    audio_dir: PathBuf,
}

impl DaemonPlayer {
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

    fn persist_now(&self) {
        self.record_position();
        self.store.flush();
    }

    fn load(&self, episode: &Episode) -> Result<()> {
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

    fn delete_download<'a>(&'a self, id: &'a str) -> BoxedFuture<'a, Result<()>> {
        Box::pin(async move {
            self.episode(id)?;
            self.downloads.evict(id).await.map(|_| ())
        })
    }

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

        self.record_position();
        if let Err(error) = self.audio.send(AudioCommand::Shutdown) {
            tracing::warn!(%error, "could not stop the audio thread");
        }

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

    const TEST_DEBOUNCE: Duration = Duration::from_secs(3600);
    use serde_json::{Value, json};
    use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

    use super::*;

    const PATIENCE: Duration = Duration::from_secs(5);

    struct FakePlayer {
        calls: Mutex<Vec<String>>,
        state: Mutex<StateSnapshot>,

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

    #[tokio::test]
    async fn a_request_line_over_the_cap_is_refused_and_the_daemon_survives() {
        let daemon = TestDaemon::start().await;
        let mut client = daemon.client().await;

        let flood = vec![b'x'; MAX_REQUEST_BYTES + 4_096];
        client.writer.write_all(&flood).await.unwrap();
        client.writer.write_all(b"\n").await.unwrap();
        let refused = client.recv().await;

        assert_eq!(refused["error"]["code"], "invalid_params");
        assert_eq!(refused["id"], Value::Null);

        let answered = client.request(r#"{"id":7,"cmd":{"type":"status"}}"#).await;
        assert_eq!(answered["id"], 7);
        assert_eq!(answered["result"]["type"], "state");

        let mut fresh = daemon.client().await;
        let served = fresh.request(r#"{"id":8,"cmd":{"type":"status"}}"#).await;
        assert_eq!(served["id"], 8);
    }

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

    #[tokio::test]
    async fn a_request_split_across_the_pushes_that_interrupt_it_is_still_answered() {
        let daemon = TestDaemon::start().await;
        let mut client = daemon.client().await;

        client
            .request(r#"{"id":1,"cmd":{"type":"subscribe"}}"#)
            .await;

        for fragment in [r#"{"id":2,"cmd"#, r#"":{"type":"#, r#""status"}}"#] {
            client.writer.write_all(fragment.as_bytes()).await.unwrap();
            client.writer.flush().await.unwrap();
            tokio::time::sleep(EVENT_TICK * 2).await;
        }
        client.writer.write_all(b"\n").await.unwrap();

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

        drop(short_lived);

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

    struct RealDaemon {
        socket: PathBuf,
        player: Arc<DaemonPlayer>,
        serving: tokio::task::JoinHandle<()>,

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

        fn publish(&self, state: StateSnapshot) {
            *lock(&self.player.state) = state;
        }

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

    #[tokio::test]
    async fn a_position_published_while_playing_is_written_and_resumed_after_a_restart() {
        let directory = tempfile::tempdir().unwrap();
        let state_path = directory.path().join("state.json");
        let daemon = RealDaemon::start(directory.path());

        daemon.publish(playing());
        tokio::time::sleep(EVENT_TICK * 3).await;

        daemon.publish(StateSnapshot::stopped());
        daemon.shutdown().await;

        let session = SessionState::read(&state_path);
        assert_eq!(
            session.positions.get("seventynine").copied(),
            Some(1_800),
            "the daemon never recorded the position it was publishing: {:?}",
            session.positions
        );

        let (restarted, commands) = daemon_player(&state_path, &directory.path().join("audio"));
        restarted.play(Some("seventynine")).unwrap();
        let loaded = commands.recv_timeout(PATIENCE).unwrap();
        match loaded {
            AudioCommand::Load { start_secs, .. } => assert_eq!(start_secs, 1_800.0),
            other => panic!("the episode was not loaded: {other:?}"),
        }
    }

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

        let session = settles(&state_path, |session| {
            session.positions.contains_key("seventynine")
        });
        assert_eq!(session.positions.get("seventynine").copied(), Some(1_800));
        drop(client);
        daemon.shutdown().await;
    }

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

        drop(player);

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

        assert_eq!(answered["result"]["preferences"]["inverted_palette"], true);

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

    async fn events_until(client: &mut TestClient, deadline: tokio::time::Instant) -> Vec<Value> {
        let mut events = Vec::new();
        while let Ok(Ok(Some(line))) =
            tokio::time::timeout_at(deadline, client.lines.next_line()).await
        {
            events.push(serde_json::from_str(&line).unwrap());
        }
        events
    }

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

        let caused_by_the_spectrum = drawn.len().saturating_sub(ticked.len());

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

    #[tokio::test]
    async fn the_fixed_tick_survives_a_spectrum_that_never_stops_moving() {
        let daemon = TestDaemon::start().await;

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
