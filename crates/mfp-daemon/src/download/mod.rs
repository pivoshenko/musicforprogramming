//! Resumable, atomic downloads that never block playback.
//!
//! A transfer writes to `<identifier>.mp3.part` and is renamed to `<identifier>.mp3` only
//! once its length matches what the feed declares, so a truncated transfer is never
//! observable under the final name.

pub mod cache;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mfp_core::error::{Error, Result};
use mfp_core::protocol::{DownloadProgress, DownloadState, ErrorObject, PlaybackState, Source};
use mfp_core::{Config, Episode};
use tokio::io::AsyncWriteExt;
use tokio::sync::Semaphore;

use crate::state::{SharedState, lock};
use cache::EpisodeCacheState;

/// How many bytes a transfer takes on before it republishes its progress.
///
/// The audio thread contends for the same lock, so progress is coarse on purpose.
const PROGRESS_STEP_BYTES: u64 = 64 * 1024;

/// How long [`DownloadManager::evict`] waits for a cancelled transfer to let go of its
/// part file before deleting it anyway.
const CANCEL_GRACE: Duration = Duration::from_secs(2);

/// The most transfers that may run at once, whatever the configuration asks for.
///
/// `max_concurrent_downloads` is a `usize` from the user's file and
/// [`tokio::sync::Semaphore::new`] panics above its own maximum, so an absurd setting would
/// otherwise end the daemon at startup naming neither the setting nor the file.
const MAX_CONCURRENT_DOWNLOADS: usize = 64;

/// Runs and tracks transfers, reporting their progress into the shared state so they
/// appear in ordinary snapshots rather than needing a channel of their own.
pub struct DownloadManager {
    state: SharedState,
    cache_dir: PathBuf,
    client: reqwest::Client,
    /// Bounds concurrent transfers; further requests queue rather than saturating the
    /// upstream host.
    permits: Arc<Semaphore>,
    /// How long [`Self::evict`] waits for a cancelled transfer to let go of its part file.
    cancel_grace: Duration,
    /// The cancellation flag of every transfer accepted and not yet finished.
    active: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
    /// Every episode evicted since it was last requested.
    ///
    /// An eviction removes the progress entry, but a publish in flight - the final one of a
    /// transfer that outlasted [`CANCEL_GRACE`], say - would put it back, leaving the
    /// interface a download describing a file that has gone, so every publish checks this set
    /// afterwards and withdraws itself. Bounded by the catalog: an identifier leaves the set
    /// the moment it is requested again.
    evicted: Arc<Mutex<HashSet<String>>>,
}

impl DownloadManager {
    pub fn new(state: SharedState, cache_dir: PathBuf, config: &Config) -> Self {
        Self::with_grace(state, cache_dir, config, CANCEL_GRACE)
    }

    fn with_grace(
        state: SharedState,
        cache_dir: PathBuf,
        config: &Config,
        cancel_grace: Duration,
    ) -> Self {
        Self {
            state,
            cache_dir,
            cancel_grace,
            client: reqwest::Client::new(),
            permits: Arc::new(Semaphore::new(
                config
                    .max_concurrent_downloads
                    .clamp(1, MAX_CONCURRENT_DOWNLOADS),
            )),
            active: Arc::new(Mutex::new(HashMap::new())),
            evicted: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Begins or resumes a transfer, returning as soon as it is accepted.
    ///
    /// Resumes from an existing part file with a range request. Refuses with
    /// [`mfp_core::ErrorCode::InsufficientSpace`] before writing anything when free space is
    /// short of the remaining bytes. A request for an episode already transferring or cached
    /// starts nothing and disturbs nothing; the wire protocol answers `download` with a bare
    /// `ok` either way, so which of the two it was shows only in the snapshot's progress entry.
    pub fn start(&self, episode: &Episode) -> Result<()> {
        let id = episode.id().into_owned();
        if !cache::names_one_entry(&id) {
            return Err(Error::Internal(format!("{id} does not name a cache entry")));
        }
        // a fresh request supersedes an eviction, so its progress is not withdrawn by one
        lock(&self.evicted).remove(&id);

        if cache::state_of(&self.cache_dir, &id).is_cached() {
            self.publish(
                &id,
                episode.byte_len,
                episode.byte_len,
                DownloadState::Completed,
                None,
            );
            return Ok(());
        }

        let cancel = Arc::new(AtomicBool::new(false));
        {
            // claimed under one lock, so two requests racing for the same episode cannot
            // both find the slot free
            let mut active = lock(&self.active);
            if active.contains_key(&id) {
                return Ok(());
            }
            active.insert(id.clone(), Arc::clone(&cancel));
        }

        let part = cache::part_path(&self.cache_dir, &id);
        let already = cache::file_len(&part).unwrap_or(0).min(episode.byte_len);
        let required = episode.byte_len - already;
        match cache::free_space(&self.cache_dir) {
            Ok(available) if available >= required => {}
            outcome => {
                self.release(&id);
                let available = outcome?;
                return Err(Error::InsufficientSpace {
                    required_bytes: required,
                    available_bytes: available,
                });
            }
        }

        self.publish(&id, already, episode.byte_len, DownloadState::Queued, None);

        let transfer = Transfer {
            id: id.clone(),
            url: episode.enclosure_url.clone(),
            total_bytes: episode.byte_len,
            part,
            complete: cache::audio_path(&self.cache_dir, &id),
            client: self.client.clone(),
            state: Arc::clone(&self.state),
            evicted: Arc::clone(&self.evicted),
            cancel,
        };
        let permits = Arc::clone(&self.permits);
        let active = Arc::clone(&self.active);

        tokio::spawn(async move {
            #[expect(
                clippy::expect_used,
                reason = "the Semaphore is owned by this DownloadManager and nothing calls close()"
            )]
            let _permit = permits
                .acquire_owned()
                .await
                .expect("the download semaphore is never closed");
            transfer.run().await;
            lock(&active).remove(&transfer.id);
        });

        Ok(())
    }

    /// Stops an in-progress transfer, leaving its bytes in the part file for a later resume.
    /// Fails with [`mfp_core::ErrorCode::DownloadNotActive`] when nothing is running for that
    /// identifier, which is what the `cancel_download` command reports to a client.
    pub fn cancel(&self, id: &str) -> Result<()> {
        if self.request_cancel(id) {
            Ok(())
        } else {
            Err(Error::DownloadNotActive(id.to_owned()))
        }
    }

    /// Cancels everything in flight, leaving no file that could be mistaken for complete.
    pub fn shutdown(&self) {
        let active = lock(&self.active);
        for cancel in active.values() {
            cancel.store(true, Ordering::SeqCst);
        }
    }

    /// Removes an episode from the cache, returning the bytes reclaimed.
    ///
    /// Cancels an in-progress transfer first and waits for it to let go of the part file.
    /// Refuses rather than deleting a file playback is currently sourcing from.
    pub async fn evict(&self, id: &str) -> Result<u64> {
        if self.is_playing_from_disk(id) {
            return Err(Error::Internal(format!(
                "{id} is playing from its local file and cannot be evicted"
            )));
        }

        if self.request_cancel(id) {
            let deadline = std::time::Instant::now() + self.cancel_grace;
            while self.is_active(id) && std::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }

        let reclaimed = cache::evict(&self.cache_dir, id)?;
        // recorded before the entry goes, so a publish racing this one withdraws itself
        // rather than describing a file that has just been removed
        lock(&self.evicted).insert(id.to_owned());
        withdraw(&self.state, id);
        Ok(reclaimed)
    }

    /// The cache's total size, counting complete files and part files.
    pub fn cache_size(&self) -> u64 {
        cache::total_size(&self.cache_dir)
    }

    pub fn cache_state(&self, id: &str) -> EpisodeCacheState {
        cache::state_of(&self.cache_dir, id)
    }

    /// Gives up a claimed slot without ever having started a transfer for it.
    fn release(&self, id: &str) {
        lock(&self.active).remove(id);
    }

    fn is_active(&self, id: &str) -> bool {
        lock(&self.active).contains_key(id)
    }

    /// Signals a transfer to stop, reporting whether there was one.
    fn request_cancel(&self, id: &str) -> bool {
        let active = lock(&self.active);
        match active.get(id) {
            Some(cancel) => {
                cancel.store(true, Ordering::SeqCst);
                true
            }
            None => false,
        }
    }

    fn is_playing_from_disk(&self, id: &str) -> bool {
        let snapshot = lock(&self.state);
        snapshot.source == Some(Source::Local)
            && snapshot.playback != PlaybackState::Stopped
            && snapshot
                .episode
                .as_ref()
                .is_some_and(|episode| episode.slug == id)
    }

    fn publish(
        &self,
        id: &str,
        downloaded_bytes: u64,
        total_bytes: u64,
        state: DownloadState,
        error: Option<ErrorObject>,
    ) {
        publish(
            &self.state,
            DownloadProgress {
                slug: id.to_owned(),
                downloaded_bytes,
                total_bytes,
                state,
                error,
            },
        );
        withdraw_if_evicted(&self.state, &self.evicted, id);
    }
}

/// Replaces this episode's progress entry, or adds one when it has none.
fn publish(state: &SharedState, entry: DownloadProgress) {
    let mut snapshot = lock(state);
    match snapshot
        .downloads
        .iter_mut()
        .find(|download| download.slug == entry.slug)
    {
        Some(existing) => *existing = entry,
        None => snapshot.downloads.push(entry),
    }
}

/// Removes this episode's progress entry.
fn withdraw(state: &SharedState, id: &str) {
    lock(state).downloads.retain(|download| download.slug != id);
}

/// Removes this episode's progress entry when the episode was evicted while the entry was
/// being written, so an eviction is never undone by a publish already in flight.
fn withdraw_if_evicted(state: &SharedState, evicted: &Mutex<HashSet<String>>, id: &str) {
    if lock(evicted).contains(id) {
        withdraw(state, id);
    }
}

/// How a transfer ended.
enum Outcome {
    Completed,
    /// Cancelled, with the bytes left in the part file for a later resume.
    Cancelled(u64),
    Failed(u64, Error),
}

/// Everything one transfer needs, owned so it can move onto its own task.
struct Transfer {
    id: String,
    url: String,
    /// The byte length the feed declares for the enclosure.
    total_bytes: u64,
    part: PathBuf,
    complete: PathBuf,
    client: reqwest::Client,
    state: SharedState,
    /// Shared with the manager; see [`DownloadManager::evicted`].
    evicted: Arc<Mutex<HashSet<String>>>,
    cancel: Arc<AtomicBool>,
}

impl Transfer {
    async fn run(&self) {
        let outcome = if self.cancelled() {
            // cancelled while queued, before a single byte was requested
            Outcome::Cancelled(cache::file_len(&self.part).unwrap_or(0))
        } else {
            self.transfer().await
        };

        let (downloaded, state, error) = match outcome {
            Outcome::Completed => (self.total_bytes, DownloadState::Completed, None),
            Outcome::Cancelled(bytes) => (bytes, DownloadState::Cancelled, None),
            Outcome::Failed(bytes, error) => {
                tracing::warn!(episode = %self.id, %error, "download failed");
                (
                    bytes,
                    DownloadState::Failed,
                    Some(ErrorObject::from(&error)),
                )
            }
        };
        publish(
            &self.state,
            DownloadProgress {
                slug: self.id.clone(),
                downloaded_bytes: downloaded,
                total_bytes: self.total_bytes,
                state,
                error,
            },
        );
        withdraw_if_evicted(&self.state, &self.evicted, &self.id);
    }

    async fn transfer(&self) -> Outcome {
        let mut existing = cache::file_len(&self.part).unwrap_or(0);
        if existing > self.total_bytes {
            // longer than the feed declares, so it cannot be a prefix of the real file
            let _ = tokio::fs::remove_file(&self.part).await;
            existing = 0;
        }
        if existing == self.total_bytes && existing > 0 {
            return self.verify_and_rename(existing).await;
        }

        self.publish_progress(existing, DownloadState::Running);

        let mut request = self
            .client
            .get(&self.url)
            .header(reqwest::header::USER_AGENT, mfp_core::USER_AGENT);
        if existing > 0 {
            request = request.header(reqwest::header::RANGE, format!("bytes={existing}-"));
        }

        let mut response = match request.send().await {
            Ok(response) => response,
            Err(error) => {
                return Outcome::Failed(existing, Error::DownloadFailed(error.to_string()));
            }
        };
        let status = response.status();
        if !status.is_success() {
            return Outcome::Failed(
                existing,
                Error::DownloadFailed(format!("{} answered {status}", self.url)),
            );
        }

        // a 200 to a ranged request means the server sent the whole resource, so the part
        // file is not a prefix of what is arriving and has to go
        let resuming = existing > 0 && status == reqwest::StatusCode::PARTIAL_CONTENT;
        if !resuming {
            existing = 0;
        }

        let mut file = match self.open_part(resuming).await {
            Ok(file) => file,
            Err(error) => return Outcome::Failed(existing, error),
        };

        let mut downloaded = existing;
        let mut published = existing;
        loop {
            if self.cancelled() {
                let _ = file.flush().await;
                return Outcome::Cancelled(downloaded);
            }
            let chunk = match response.chunk().await {
                Ok(Some(chunk)) => chunk,
                Ok(None) => break,
                Err(error) => {
                    let _ = file.flush().await;
                    return Outcome::Failed(downloaded, Error::DownloadFailed(error.to_string()));
                }
            };
            if let Err(error) = file.write_all(&chunk).await {
                return Outcome::Failed(downloaded, error.into());
            }
            downloaded += chunk.len() as u64;
            if downloaded - published >= PROGRESS_STEP_BYTES {
                published = downloaded;
                self.publish_progress(downloaded, DownloadState::Running);
            }
        }

        if let Err(error) = file.flush().await {
            return Outcome::Failed(downloaded, error.into());
        }
        if let Err(error) = file.sync_all().await {
            return Outcome::Failed(downloaded, error.into());
        }
        drop(file);

        self.verify_and_rename(downloaded).await
    }

    async fn open_part(&self, resuming: bool) -> Result<tokio::fs::File> {
        if let Some(parent) = self.part.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let file = if resuming {
            tokio::fs::OpenOptions::new()
                .append(true)
                .open(&self.part)
                .await?
        } else {
            // truncating, so a part file the server refused to resume starts over
            tokio::fs::File::create(&self.part).await?
        };
        Ok(file)
    }

    /// The only path to the final name: the part file is renamed only once its length
    /// matches what the feed declares, and is deleted outright when it does not.
    async fn verify_and_rename(&self, downloaded: u64) -> Outcome {
        let on_disk = cache::file_len(&self.part).unwrap_or(0);
        if on_disk != self.total_bytes {
            let _ = tokio::fs::remove_file(&self.part).await;
            return Outcome::Failed(
                downloaded,
                Error::DownloadFailed(format!(
                    "{} transferred {on_disk} bytes but the feed declares {}",
                    self.id, self.total_bytes
                )),
            );
        }
        if let Err(error) = tokio::fs::rename(&self.part, &self.complete).await {
            return Outcome::Failed(downloaded, error.into());
        }
        Outcome::Completed
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    fn publish_progress(&self, downloaded_bytes: u64, state: DownloadState) {
        publish(
            &self.state,
            DownloadProgress {
                slug: self.id.clone(),
                downloaded_bytes,
                total_bytes: self.total_bytes,
                state,
                error: None,
            },
        );
        withdraw_if_evicted(&self.state, &self.evicted, &self.id);
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::sync::atomic::AtomicUsize;

    use mfp_core::ErrorCode;
    use mfp_core::protocol::{EpisodeRef, StateSnapshot};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt as _, BufReader};
    use tokio::net::{TcpListener, TcpStream};

    use super::*;

    /// A deterministic stand-in for episode audio. Every test asserts against these exact
    /// bytes, so a resumed file being byte-identical is a claim about content, not just
    /// length.
    fn fixture(len: usize) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(len);
        let mut value: u32 = 0x1234_5678;
        while bytes.len() < len {
            value = value.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        bytes.truncate(len);
        bytes
    }

    /// A local HTTP server serving one fixture body. No test touches the network.
    struct Server {
        addr: SocketAddr,
        /// Every `Range` start the server was asked for, `None` for an unranged request.
        ranges: Arc<Mutex<Vec<Option<u64>>>>,
        requests: Arc<AtomicUsize>,
    }

    struct ServerOptions {
        /// When false the server answers a ranged request with the whole resource, which
        /// is the case the spec makes the client discard its part file for.
        honour_range: bool,
        chunk: usize,
        delay: Duration,
    }

    impl Default for ServerOptions {
        fn default() -> Self {
            Self {
                honour_range: true,
                chunk: usize::MAX,
                delay: Duration::ZERO,
            }
        }
    }

    impl Server {
        async fn start(body: Vec<u8>, options: ServerOptions) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let ranges = Arc::new(Mutex::new(Vec::new()));
            let requests = Arc::new(AtomicUsize::new(0));

            let body = Arc::new(body);
            let options = Arc::new(options);
            let seen = Arc::clone(&ranges);
            let counted = Arc::clone(&requests);
            tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    let body = Arc::clone(&body);
                    let options = Arc::clone(&options);
                    let seen = Arc::clone(&seen);
                    let counted = Arc::clone(&counted);
                    tokio::spawn(async move {
                        let _ = serve(stream, body, options, seen, counted).await;
                    });
                }
            });

            Self {
                addr,
                ranges,
                requests,
            }
        }

        fn url(&self) -> String {
            format!("http://{}/episode.mp3", self.addr)
        }

        fn ranges(&self) -> Vec<Option<u64>> {
            self.ranges.lock().unwrap().clone()
        }

        fn request_count(&self) -> usize {
            self.requests.load(Ordering::SeqCst)
        }
    }

    async fn serve(
        stream: TcpStream,
        body: Arc<Vec<u8>>,
        options: Arc<ServerOptions>,
        ranges: Arc<Mutex<Vec<Option<u64>>>>,
        requests: Arc<AtomicUsize>,
    ) -> std::io::Result<()> {
        let mut reader = BufReader::new(stream);
        let mut range_start = None;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).await? == 0 {
                return Ok(());
            }
            let trimmed = line.trim_end();
            if trimmed.is_empty() {
                break;
            }
            // hyper writes header names lowercased, so match without regard to case
            if let Some(value) = trimmed.to_ascii_lowercase().strip_prefix("range: bytes=") {
                range_start = value.trim_end_matches('-').parse::<u64>().ok();
            }
        }

        requests.fetch_add(1, Ordering::SeqCst);
        ranges.lock().unwrap().push(range_start);

        let total = body.len() as u64;
        let start = match range_start {
            Some(start) if options.honour_range => start.min(total),
            _ => 0,
        };
        let slice = &body[start as usize..];

        // one request per connection, announced so the client does not pool a socket this
        // handler is about to close under it
        let head = if range_start.is_some() && options.honour_range {
            format!(
                "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {start}-{}/{total}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                slice.len(),
                total.saturating_sub(1),
            )
        } else {
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                slice.len()
            )
        };

        let mut stream = reader.into_inner();
        stream.write_all(head.as_bytes()).await?;
        for chunk in slice.chunks(options.chunk.min(slice.len().max(1))) {
            stream.write_all(chunk).await?;
            stream.flush().await?;
            if !options.delay.is_zero() {
                tokio::time::sleep(options.delay).await;
            }
        }
        Ok(())
    }

    fn episode(url: &str, byte_len: u64) -> Episode {
        Episode {
            bundle_title: None,
            special: false,
            title: "Episode 79".into(),
            link: "https://musicforprogramming.net/seventynine".into(),
            enclosure_url: url.into(),
            byte_len,
            duration_secs: 14_400,
            published_at: 1_700_000_000,
            slug: Some("seventynine".into()),
            order: Some(79),
            tracklist: None,
            body: None,
            links: None,
        }
    }

    fn manager(state: &SharedState, dir: &std::path::Path) -> DownloadManager {
        DownloadManager::new(Arc::clone(state), dir.to_path_buf(), &Config::default())
    }

    fn shared() -> SharedState {
        Arc::new(Mutex::new(StateSnapshot::stopped()))
    }

    fn progress(state: &SharedState, id: &str) -> Option<DownloadProgress> {
        state
            .lock()
            .unwrap()
            .downloads
            .iter()
            .find(|download| download.slug == id)
            .cloned()
    }

    /// Waits for this episode's download to reach a terminal state.
    async fn settle(state: &SharedState, id: &str) -> DownloadProgress {
        for _ in 0..600 {
            if let Some(entry) = progress(state, id)
                && matches!(
                    entry.state,
                    DownloadState::Completed | DownloadState::Failed | DownloadState::Cancelled
                )
            {
                return entry;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the download never reached a terminal state");
    }

    #[tokio::test]
    async fn a_whole_download_is_verified_and_renamed_into_place() {
        let body = fixture(300_000);
        let server = Server::start(body.clone(), ServerOptions::default()).await;
        let root = tempfile::tempdir().unwrap();
        let state = shared();
        let manager = manager(&state, root.path());

        manager
            .start(&episode(&server.url(), body.len() as u64))
            .unwrap();
        let entry = settle(&state, "seventynine").await;

        assert_eq!(entry.state, DownloadState::Completed);
        assert_eq!(entry.downloaded_bytes, body.len() as u64);
        assert_eq!(entry.total_bytes, body.len() as u64);
        let complete = cache::audio_path(root.path(), "seventynine");
        assert_eq!(std::fs::read(&complete).unwrap(), body);
        assert!(
            !cache::part_path(root.path(), "seventynine").exists(),
            "a part file survived a completed download"
        );
        assert!(manager.cache_state("seventynine").is_cached());
    }

    #[tokio::test]
    async fn a_size_mismatch_discards_the_part_file_and_produces_no_final_file() {
        let body = fixture(120_000);
        let server = Server::start(body.clone(), ServerOptions::default()).await;
        let root = tempfile::tempdir().unwrap();
        let state = shared();
        let manager = manager(&state, root.path());

        // the feed declares more than the server actually serves
        manager
            .start(&episode(&server.url(), body.len() as u64 + 4_096))
            .unwrap();
        let entry = settle(&state, "seventynine").await;

        assert_eq!(entry.state, DownloadState::Failed);
        assert_eq!(
            entry.error.as_ref().map(|error| error.code),
            Some(ErrorCode::DownloadFailed)
        );
        assert!(
            !cache::audio_path(root.path(), "seventynine").exists(),
            "a truncated transfer produced a file under the complete name"
        );
        assert!(
            !cache::part_path(root.path(), "seventynine").exists(),
            "the part file was not discarded"
        );
        assert_eq!(
            manager.cache_state("seventynine"),
            EpisodeCacheState::NotCached
        );
    }

    #[tokio::test]
    async fn a_resumed_download_is_byte_identical_to_a_whole_one() {
        let body = fixture(400_000);
        let server = Server::start(body.clone(), ServerOptions::default()).await;
        let root = tempfile::tempdir().unwrap();

        // a whole download, for the reference bytes
        let whole_dir = root.path().join("whole");
        let whole_state = shared();
        let whole = manager(&whole_state, &whole_dir);
        whole
            .start(&episode(&server.url(), body.len() as u64))
            .unwrap();
        assert_eq!(
            settle(&whole_state, "seventynine").await.state,
            DownloadState::Completed
        );
        let reference = std::fs::read(cache::audio_path(&whole_dir, "seventynine")).unwrap();

        // and a resume from a part file holding the first 150 000 bytes
        let resumed_dir = root.path().join("resumed");
        std::fs::create_dir_all(&resumed_dir).unwrap();
        std::fs::write(
            cache::part_path(&resumed_dir, "seventynine"),
            &body[..150_000],
        )
        .unwrap();
        let resumed_state = shared();
        let resumed = manager(&resumed_state, &resumed_dir);
        resumed
            .start(&episode(&server.url(), body.len() as u64))
            .unwrap();
        assert_eq!(
            settle(&resumed_state, "seventynine").await.state,
            DownloadState::Completed
        );

        let resumed_bytes = std::fs::read(cache::audio_path(&resumed_dir, "seventynine")).unwrap();
        assert_eq!(resumed_bytes, reference);
        assert_eq!(resumed_bytes, body);
        // the second request asked only for the bytes that were missing
        assert_eq!(server.ranges(), vec![None, Some(150_000)]);
    }

    #[tokio::test]
    async fn a_server_that_ignores_the_range_restarts_from_zero_and_still_verifies() {
        let body = fixture(200_000);
        let server = Server::start(
            body.clone(),
            ServerOptions {
                honour_range: false,
                ..ServerOptions::default()
            },
        )
        .await;
        let root = tempfile::tempdir().unwrap();
        let state = shared();
        let manager = manager(&state, root.path());
        std::fs::write(
            cache::part_path(root.path(), "seventynine"),
            &body[..90_000],
        )
        .unwrap();

        manager
            .start(&episode(&server.url(), body.len() as u64))
            .unwrap();
        let entry = settle(&state, "seventynine").await;

        assert_eq!(entry.state, DownloadState::Completed);
        assert_eq!(server.ranges(), vec![Some(90_000)]);
        // the discarded part file was not appended to, so the result is still the fixture
        assert_eq!(
            std::fs::read(cache::audio_path(root.path(), "seventynine")).unwrap(),
            body
        );
    }

    #[tokio::test]
    async fn cancelling_keeps_the_part_file_and_a_later_start_resumes_from_it() {
        let body = fixture(500_000);
        let server = Server::start(
            body.clone(),
            ServerOptions {
                chunk: 16_384,
                delay: Duration::from_millis(20),
                ..ServerOptions::default()
            },
        )
        .await;
        let root = tempfile::tempdir().unwrap();
        let state = shared();
        let manager = manager(&state, root.path());

        manager
            .start(&episode(&server.url(), body.len() as u64))
            .unwrap();
        // let some bytes land before pulling the plug
        for _ in 0..200 {
            if progress(&state, "seventynine")
                .is_some_and(|entry| entry.downloaded_bytes >= PROGRESS_STEP_BYTES)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        manager.cancel("seventynine").unwrap();
        let entry = settle(&state, "seventynine").await;

        assert_eq!(entry.state, DownloadState::Cancelled);
        let part = cache::part_path(root.path(), "seventynine");
        let kept = std::fs::metadata(&part).unwrap().len();
        assert!(kept > 0 && kept < body.len() as u64, "kept {kept} bytes");
        assert!(
            !cache::audio_path(root.path(), "seventynine").exists(),
            "a cancelled download produced a complete file"
        );
        // the part file really is a prefix of the episode, so resuming is sound
        assert_eq!(std::fs::read(&part).unwrap(), body[..kept as usize]);

        // and starting again finishes it without re-fetching what is already there
        for _ in 0..200 {
            if !manager.is_active("seventynine") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        manager
            .start(&episode(&server.url(), body.len() as u64))
            .unwrap();
        assert_eq!(
            settle(&state, "seventynine").await.state,
            DownloadState::Completed
        );
        assert_eq!(
            std::fs::read(cache::audio_path(root.path(), "seventynine")).unwrap(),
            body
        );
        assert_eq!(server.ranges().last().copied().flatten(), Some(kept));
    }

    #[tokio::test]
    async fn cancelling_a_download_that_is_not_running_reports_download_not_active() {
        let root = tempfile::tempdir().unwrap();
        let state = shared();
        let manager = manager(&state, root.path());

        let error = manager.cancel("seventynine").unwrap_err();

        assert_eq!(error.code(), ErrorCode::DownloadNotActive);
        assert!(progress(&state, "seventynine").is_none());
    }

    #[tokio::test]
    async fn a_duplicate_request_starts_no_second_transfer() {
        let body = fixture(400_000);
        let server = Server::start(
            body.clone(),
            ServerOptions {
                chunk: 16_384,
                delay: Duration::from_millis(20),
                ..ServerOptions::default()
            },
        )
        .await;
        let root = tempfile::tempdir().unwrap();
        let state = shared();
        let manager = manager(&state, root.path());
        let episode = episode(&server.url(), body.len() as u64);

        manager.start(&episode).unwrap();
        for _ in 0..200 {
            if progress(&state, "seventynine")
                .is_some_and(|entry| entry.state == DownloadState::Running)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        manager.start(&episode).unwrap();
        manager.start(&episode).unwrap();

        assert_eq!(
            settle(&state, "seventynine").await.state,
            DownloadState::Completed
        );
        assert_eq!(server.request_count(), 1, "a second transfer was started");
        assert_eq!(
            std::fs::read(cache::audio_path(root.path(), "seventynine")).unwrap(),
            body
        );
    }

    #[tokio::test]
    async fn a_request_for_an_already_cached_episode_transfers_nothing() {
        let body = fixture(50_000);
        let server = Server::start(body.clone(), ServerOptions::default()).await;
        let root = tempfile::tempdir().unwrap();
        std::fs::write(cache::audio_path(root.path(), "seventynine"), &body).unwrap();
        let state = shared();
        let manager = manager(&state, root.path());

        manager
            .start(&episode(&server.url(), body.len() as u64))
            .unwrap();

        assert_eq!(server.request_count(), 0);
        assert_eq!(
            progress(&state, "seventynine").unwrap().state,
            DownloadState::Completed
        );
    }

    #[tokio::test]
    async fn an_insufficient_free_space_refusal_writes_nothing() {
        let root = tempfile::tempdir().unwrap();
        let state = shared();
        let manager = manager(&state, root.path());
        // larger than any filesystem this test could be run on
        let enormous = u64::MAX / 2;

        let error = manager
            .start(&episode("http://127.0.0.1:1/episode.mp3", enormous))
            .unwrap_err();

        assert_eq!(error.code(), ErrorCode::InsufficientSpace);
        let Error::InsufficientSpace {
            required_bytes,
            available_bytes,
        } = &error
        else {
            panic!("expected an insufficient-space error, got {error:?}");
        };
        assert_eq!(*required_bytes, enormous);
        assert!(available_bytes < required_bytes);
        assert!(!cache::part_path(root.path(), "seventynine").exists());
        assert!(progress(&state, "seventynine").is_none());
    }

    #[tokio::test]
    async fn free_space_is_measured_against_only_the_bytes_still_missing() {
        let root = tempfile::tempdir().unwrap();
        let body = fixture(80_000);
        let server = Server::start(body.clone(), ServerOptions::default()).await;
        let state = shared();
        let manager = manager(&state, root.path());
        // a part file holding all but the last 1 000 bytes: only those are required, and
        // the free-space check must not refuse on the full declared length
        std::fs::write(
            cache::part_path(root.path(), "seventynine"),
            &body[..body.len() - 1_000],
        )
        .unwrap();

        manager
            .start(&episode(&server.url(), body.len() as u64))
            .unwrap();

        assert_eq!(
            settle(&state, "seventynine").await.state,
            DownloadState::Completed
        );
        assert_eq!(
            std::fs::read(cache::audio_path(root.path(), "seventynine")).unwrap(),
            body
        );
    }

    #[tokio::test]
    async fn progress_advances_while_playback_keeps_its_own_snapshot_fields() {
        let body = fixture(600_000);
        let server = Server::start(
            body.clone(),
            ServerOptions {
                chunk: 16_384,
                delay: Duration::from_millis(10),
                ..ServerOptions::default()
            },
        )
        .await;
        let root = tempfile::tempdir().unwrap();
        let state = shared();
        {
            // an episode is playing from the network while the download runs
            let mut snapshot = state.lock().unwrap();
            snapshot.playback = PlaybackState::Playing;
            snapshot.source = Some(Source::Stream);
            snapshot.position_secs = 1_234.0;
            snapshot.episode = Some(EpisodeRef {
                slug: "seventynine".into(),
                title: "Episode 79".into(),
                duration_secs: 14_400.0,
            });
        }
        let manager = manager(&state, root.path());

        manager
            .start(&episode(&server.url(), body.len() as u64))
            .unwrap();

        let mut readings = Vec::new();
        for _ in 0..400 {
            if let Some(entry) = progress(&state, "seventynine") {
                readings.push(entry.downloaded_bytes);
                if entry.state == DownloadState::Completed {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        let observed: Vec<u64> = {
            let mut unique = readings.clone();
            unique.dedup();
            unique
        };
        assert!(observed.len() >= 3, "progress never advanced: {observed:?}");
        assert!(
            observed.windows(2).all(|pair| pair[0] <= pair[1]),
            "progress went backwards: {observed:?}"
        );
        assert_eq!(*observed.last().unwrap(), body.len() as u64);

        // playback was never touched by the transfer
        let snapshot = state.lock().unwrap();
        assert_eq!(snapshot.playback, PlaybackState::Playing);
        assert_eq!(snapshot.source, Some(Source::Stream));
        assert_eq!(snapshot.position_secs, 1_234.0);
    }

    /// `Semaphore::new` panics above its own maximum, so an absurd setting in the user's
    /// configuration file would otherwise end the daemon before it served anything.
    #[tokio::test]
    async fn an_absurd_concurrency_setting_is_clamped_rather_than_ending_the_daemon() {
        let root = tempfile::tempdir().unwrap();
        let state = shared();

        for requested in [0, 1, usize::MAX] {
            let config = Config {
                max_concurrent_downloads: requested,
                ..Config::default()
            };
            let manager =
                DownloadManager::new(Arc::clone(&state), root.path().to_path_buf(), &config);
            assert!(manager.permits.available_permits() >= 1);
            assert!(manager.permits.available_permits() <= MAX_CONCURRENT_DOWNLOADS);
        }
    }

    #[tokio::test]
    async fn more_requests_than_the_limit_queue_rather_than_running_at_once() {
        let body = fixture(300_000);
        let server = Server::start(
            body.clone(),
            ServerOptions {
                chunk: 16_384,
                delay: Duration::from_millis(10),
                ..ServerOptions::default()
            },
        )
        .await;
        let root = tempfile::tempdir().unwrap();
        let state = shared();
        let config = Config {
            max_concurrent_downloads: 1,
            ..Config::default()
        };
        let manager = DownloadManager::new(Arc::clone(&state), root.path().to_path_buf(), &config);

        let mut ids = Vec::new();
        for index in 0..3 {
            let mut episode = episode(&server.url(), body.len() as u64);
            episode.slug = Some(format!("episode-{index}"));
            ids.push(episode.id().into_owned());
            manager.start(&episode).unwrap();
        }

        // with a limit of one, at most one transfer is ever running
        let mut saw_a_queued_entry = false;
        for _ in 0..400 {
            let running = state
                .lock()
                .unwrap()
                .downloads
                .iter()
                .filter(|download| download.state == DownloadState::Running)
                .count();
            assert!(running <= 1, "{running} transfers ran at once");
            saw_a_queued_entry |= state
                .lock()
                .unwrap()
                .downloads
                .iter()
                .any(|download| download.state == DownloadState::Queued);
            if state
                .lock()
                .unwrap()
                .downloads
                .iter()
                .all(|download| download.state == DownloadState::Completed)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        assert!(saw_a_queued_entry, "nothing ever queued");
        for id in ids {
            assert_eq!(settle(&state, &id).await.state, DownloadState::Completed);
            assert_eq!(
                std::fs::read(cache::audio_path(root.path(), &id)).unwrap(),
                body
            );
        }
    }

    #[tokio::test]
    async fn eviction_cancels_an_in_progress_download_and_removes_both_files() {
        let body = fixture(500_000);
        let server = Server::start(
            body.clone(),
            ServerOptions {
                chunk: 16_384,
                delay: Duration::from_millis(20),
                ..ServerOptions::default()
            },
        )
        .await;
        let root = tempfile::tempdir().unwrap();
        let state = shared();
        let manager = manager(&state, root.path());

        manager
            .start(&episode(&server.url(), body.len() as u64))
            .unwrap();
        for _ in 0..200 {
            if progress(&state, "seventynine")
                .is_some_and(|entry| entry.downloaded_bytes >= PROGRESS_STEP_BYTES)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let reclaimed = manager.evict("seventynine").await.unwrap();

        assert!(reclaimed > 0);
        assert!(!cache::part_path(root.path(), "seventynine").exists());
        assert!(!cache::audio_path(root.path(), "seventynine").exists());
        assert!(progress(&state, "seventynine").is_none());
        assert_eq!(manager.cache_size(), 0);
    }

    /// The grace period bounds how long an eviction waits, not whether the transfer is over.
    /// One that outlasts it still publishes its outcome, and without the withdrawal that
    /// publish would put back the entry the eviction just removed, leaving the interface a
    /// download describing a file that has gone.
    #[tokio::test]
    async fn a_transfer_outlasting_the_grace_period_does_not_resurrect_its_progress_entry() {
        let body = fixture(500_000);
        // the body stalls after its first chunk, so the transfer is parked inside the read
        // rather than at the top of the loop where it would see the cancellation at once
        let grace = Duration::from_millis(100);
        let stall = Duration::from_secs(1);
        let server = Server::start(
            body.clone(),
            ServerOptions {
                chunk: 1_024,
                delay: stall,
                ..ServerOptions::default()
            },
        )
        .await;
        let root = tempfile::tempdir().unwrap();
        let state = shared();
        // the grace is injected rather than waited out, so the test turns on the transfer
        // outlasting it and not on how loaded the machine running it is
        let manager = DownloadManager::with_grace(
            Arc::clone(&state),
            root.path().to_path_buf(),
            &Config::default(),
            grace,
        );

        manager
            .start(&episode(&server.url(), body.len() as u64))
            .unwrap();
        for _ in 0..200 {
            if cache::file_len(&cache::part_path(root.path(), "seventynine")).is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        manager.evict("seventynine").await.unwrap();
        assert!(progress(&state, "seventynine").is_none());
        assert!(
            manager.is_active("seventynine"),
            "the transfer finished inside the grace period, so this proves nothing"
        );

        // and it is still gone once the transfer has published whatever it made of itself
        // Polled to a deadline rather than slept for `stall`, for the same reason the grace
        // is injected: a loaded machine can take longer than the stall to wake the transfer,
        // publish and release the slot
        let deadline = std::time::Instant::now() + stall * 10;
        while manager.is_active("seventynine") && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !manager.is_active("seventynine"),
            "the transfer never finished"
        );
        assert!(
            progress(&state, "seventynine").is_none(),
            "an eviction was undone by a publish already in flight: {:?}",
            progress(&state, "seventynine")
        );
    }

    /// An evicted episode's progress stays gone, and the withdrawal is what makes that
    /// hold, whatever the transfer was about to say.
    #[tokio::test]
    async fn a_publish_for_an_evicted_episode_withdraws_itself() {
        let root = tempfile::tempdir().unwrap();
        let state = shared();
        let manager = manager(&state, root.path());
        manager.publish("seventynine", 10, 20, DownloadState::Running, None);
        assert!(progress(&state, "seventynine").is_some());

        manager.evict("seventynine").await.unwrap();
        // exactly what a transfer that had not noticed the eviction would publish
        manager.publish("seventynine", 20, 20, DownloadState::Completed, None);

        assert!(progress(&state, "seventynine").is_none());

        // and a fresh request supersedes the eviction rather than being withdrawn by it
        manager.start(&episode("http://127.0.0.1:1/x", 20)).unwrap();
        assert!(progress(&state, "seventynine").is_some());
    }

    #[tokio::test]
    async fn an_identifier_that_could_name_a_file_outside_the_cache_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let state = shared();
        let manager = manager(&state, root.path());
        let mut episode = episode("http://127.0.0.1:1/x", 20);
        episode.slug = Some("../escape".into());

        assert_eq!(
            manager.start(&episode).unwrap_err().code(),
            ErrorCode::Internal
        );
        assert!(!root.path().join("..").join("escape.mp3.part").exists());
    }

    #[tokio::test]
    async fn evicting_the_episode_playing_from_disk_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let body = fixture(1_000);
        std::fs::write(cache::audio_path(root.path(), "seventynine"), &body).unwrap();
        let state = shared();
        {
            let mut snapshot = state.lock().unwrap();
            snapshot.playback = PlaybackState::Playing;
            snapshot.source = Some(Source::Local);
            snapshot.episode = Some(EpisodeRef {
                slug: "seventynine".into(),
                title: "Episode 79".into(),
                duration_secs: 14_400.0,
            });
        }
        let manager = manager(&state, root.path());

        let error = manager.evict("seventynine").await.unwrap_err();

        assert_eq!(error.code(), ErrorCode::Internal);
        assert!(cache::audio_path(root.path(), "seventynine").exists());
        assert_eq!(manager.cache_size(), body.len() as u64);
    }

    #[tokio::test]
    async fn shutdown_cancels_every_transfer_and_leaves_no_complete_file() {
        let body = fixture(500_000);
        let server = Server::start(
            body.clone(),
            ServerOptions {
                chunk: 16_384,
                delay: Duration::from_millis(20),
                ..ServerOptions::default()
            },
        )
        .await;
        let root = tempfile::tempdir().unwrap();
        let state = shared();
        let manager = manager(&state, root.path());

        manager
            .start(&episode(&server.url(), body.len() as u64))
            .unwrap();
        for _ in 0..200 {
            if progress(&state, "seventynine")
                .is_some_and(|entry| entry.state == DownloadState::Running)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        manager.shutdown();

        assert_eq!(
            settle(&state, "seventynine").await.state,
            DownloadState::Cancelled
        );
        assert!(!cache::audio_path(root.path(), "seventynine").exists());
    }

    #[tokio::test]
    async fn a_part_file_that_is_already_the_declared_length_is_renamed_without_a_request() {
        let body = fixture(40_000);
        let server = Server::start(body.clone(), ServerOptions::default()).await;
        let root = tempfile::tempdir().unwrap();
        std::fs::write(cache::part_path(root.path(), "seventynine"), &body).unwrap();
        let state = shared();
        let manager = manager(&state, root.path());

        manager
            .start(&episode(&server.url(), body.len() as u64))
            .unwrap();

        assert_eq!(
            settle(&state, "seventynine").await.state,
            DownloadState::Completed
        );
        assert_eq!(server.request_count(), 0);
        assert_eq!(
            std::fs::read(cache::audio_path(root.path(), "seventynine")).unwrap(),
            body
        );
    }

    #[tokio::test]
    async fn a_network_failure_produces_no_complete_file() {
        let root = tempfile::tempdir().unwrap();
        let state = shared();
        let manager = manager(&state, root.path());

        // nothing is listening on this port
        manager
            .start(&episode("http://127.0.0.1:1/episode.mp3", 441_000_000))
            .unwrap();
        let entry = settle(&state, "seventynine").await;

        assert_eq!(entry.state, DownloadState::Failed);
        assert_eq!(
            entry.error.as_ref().map(|error| error.code),
            Some(ErrorCode::DownloadFailed)
        );
        assert!(!cache::audio_path(root.path(), "seventynine").exists());
    }
}
