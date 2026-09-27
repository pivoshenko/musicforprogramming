//! The daemon's live state and the durable listening state behind it.
//!
//! [`SharedState`] is what the audio thread and the download tasks publish into and the
//! socket server reads. [`StateStore`] is the `state.json` beneath it, which survives
//! restarts.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use mfp_core::error::{Error, Result};
use mfp_core::protocol::StateSnapshot;
use serde::{Deserialize, Serialize};

/// The live snapshot every part of the daemon publishes into.
///
/// Clients hold no authoritative state of their own, so several simultaneous clients are
/// correct by construction rather than by convention.
pub type SharedState = Arc<Mutex<StateSnapshot>>;

/// Takes a lock, recovering rather than propagating a panic from another thread.
///
/// A poisoned player state is still a truthful snapshot, and refusing to answer clients
/// because an unrelated thread panicked would be worse than serving it. It lives beside the
/// lock rather than beside the socket server or the download tasks because both take this
/// view of it.
pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// How long a position may sit unwritten while playback advances.
pub const WRITE_DEBOUNCE: Duration = Duration::from_secs(5);

/// How often the writer thread looks at a state the debounce held back.
///
/// The debounce is otherwise only re-examined when something new arrives, so a state held
/// back would sit there until the next change prompted a look. It also keeps a debounce of
/// zero from spinning the thread.
const WRITER_WAKEUP: Duration = Duration::from_millis(50);

/// A position within this many seconds of the start is not recorded.
pub const START_MARGIN_SECS: u64 = 30;

/// A position within this many seconds of the end is not recorded.
pub const END_MARGIN_SECS: u64 = 60;

/// The contents of `state.json`, kept human-readable so a user can inspect or delete it
/// without special tooling.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionState {
    /// Whole-second positions keyed by episode identifier. An entry whose episode has
    /// left the catalog is retained rather than discarded.
    pub positions: BTreeMap<String, u64>,
    pub last_played: Option<String>,
    /// Every episode marked a favourite, keyed as [`Self::positions`] is, so an entry stays
    /// attached to its episode across catalog refreshes.
    pub favourites: BTreeSet<String>,
    pub preferences: Preferences,
}

impl SessionState {
    /// The state the file at `path` holds.
    ///
    /// A missing, empty, or unparseable file yields an empty state rather than an error.
    /// An unparseable one is moved aside before it is replaced, and the condition logged.
    pub fn read(path: &Path) -> Self {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Self::default();
            }
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "could not read state; starting empty");
                return Self::default();
            }
        };

        if text.trim().is_empty() {
            return Self::default();
        }

        match serde_json::from_str(&text) {
            Ok(session) => session,
            Err(error) => {
                let aside = path.with_extension("json.corrupt");
                tracing::warn!(
                    path = %path.display(),
                    preserved_at = %aside.display(),
                    %error,
                    "state is unparseable; starting empty",
                );
                if let Err(error) = std::fs::rename(path, &aside) {
                    tracing::warn!(%error, "could not preserve the unparseable state file");
                }
                Self::default()
            }
        }
    }
}

/// The interface's own display preferences.
///
/// They are properties of the interface rather than of playback, and ride in the same file
/// only to avoid a second store and a second recovery path for one boolean.
///
/// A file written by an earlier version also carries a dismissed-notice flag, which `serde`
/// drops as a field the struct no longer has, leaving its positions and its palette intact.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Preferences {
    pub inverted_palette: bool,
}

/// Owns `state.json`: loads it at startup, debounces writes while playback advances, and
/// writes through a temporary file and a rename so an abrupt kill can never leave a
/// truncated file that fails to load next time.
///
/// The in-memory state is on this side of the channel and every mutator is cheap. The file
/// is written on a thread of its own, so creating the temporary file, the `fsync` and the
/// rename never run on the tokio runtime serving clients and driving downloads. Every method
/// here returns as soon as the change is recorded; [`Self::sync`] waits for the disk.
pub struct StateStore {
    inner: Mutex<Inner>,
    /// `Option` only so that [`Drop`] can close the channel before it joins the writer.
    writes: Option<mpsc::Sender<Write>>,
    writer: Option<std::thread::JoinHandle<()>>,
}

struct Inner {
    session: SessionState,
    /// Whether [`Self::session`] holds a change the writer has not been handed.
    dirty: bool,
}

/// What the writer thread is asked to do.
enum Write {
    /// Write this state once the debounce has elapsed.
    Debounced(SessionState),
    /// Write this state at once.
    Now(SessionState),
    /// Answer once everything sent before this has been dealt with, reporting what the
    /// last attempted write made of it.
    ///
    /// `force` writes whatever the debounce is still holding back first, which is what a
    /// shutdown needs and what a test asserting the debounce held must not ask for.
    Drain {
        force: bool,
        ack: mpsc::Sender<Result<()>>,
    },
}

impl StateStore {
    /// Loads the state file and starts the thread that will write it.
    pub fn load(path: PathBuf) -> Self {
        Self::with_debounce(path, WRITE_DEBOUNCE)
    }

    /// Visible to the crate so a test can make the debounce long enough that a write
    /// observed at all is provably a write that did not wait for it.
    pub(crate) fn with_debounce(path: PathBuf, debounce: Duration) -> Self {
        let session = SessionState::read(&path);
        let (writes, requests) = mpsc::channel();
        let writer = std::thread::Builder::new()
            .name("mfp-state".to_owned())
            .spawn(move || Writer::new(path, debounce).run(&requests));
        #[expect(
            clippy::expect_used,
            reason = "a store that cannot start its writer cannot persist anything, and \
                      the daemon has nothing useful to do without one"
        )]
        let writer = writer.expect("the state writer thread starts");

        Self {
            inner: Mutex::new(Inner {
                session,
                dirty: false,
            }),
            writes: Some(writes),
            writer: Some(writer),
        }
    }

    /// The state as it currently stands, restored values included.
    pub fn session(&self) -> SessionState {
        lock(&self.inner).session.clone()
    }

    pub fn position(&self, id: &str) -> Option<u64> {
        lock(&self.inner).session.positions.get(id).copied()
    }

    /// The episode that was playing when the daemon last recorded one.
    pub fn last_played(&self) -> Option<String> {
        lock(&self.inner).session.last_played.clone()
    }

    /// Every episode marked a favourite, in the order the wire protocol reports them.
    pub fn favourites(&self) -> Vec<String> {
        lock(&self.inner)
            .session
            .favourites
            .iter()
            .cloned()
            .collect()
    }

    pub fn preferences(&self) -> Preferences {
        lock(&self.inner).session.preferences
    }

    /// Offers a position for the episode.
    ///
    /// A position inside [`START_MARGIN_SECS`] of the start or [`END_MARGIN_SECS`] of the end
    /// removes any previously recorded position rather than storing a new one, so the episode
    /// starts from the beginning next time.
    pub fn record_position(&self, id: &str, position_secs: u64, duration_secs: Option<u64>) {
        if is_near_an_edge(position_secs, duration_secs) {
            self.clear_position(id);
            return;
        }
        let mut inner = lock(&self.inner);
        if inner.session.positions.insert(id.to_owned(), position_secs) != Some(position_secs) {
            inner.dirty = true;
        }
    }

    /// Forgets one episode's position. Succeeds when it had none.
    pub fn clear_position(&self, id: &str) {
        let mut inner = lock(&self.inner);
        if inner.session.positions.remove(id).is_some() {
            inner.dirty = true;
        }
    }

    pub fn clear_all_positions(&self) {
        let mut inner = lock(&self.inner);
        if !inner.session.positions.is_empty() {
            inner.session.positions.clear();
            inner.dirty = true;
        }
    }

    /// Records the episode that has just begun playing.
    pub fn set_last_played(&self, id: &str) {
        let mut inner = lock(&self.inner);
        if inner.session.last_played.as_deref() != Some(id) {
            inner.session.last_played = Some(id.to_owned());
            inner.dirty = true;
        }
    }

    /// Marks an episode a favourite, or unmarks it.
    ///
    /// Marking one already marked and unmarking one not marked both change nothing and are
    /// not errors, so a caller never has to read the set before writing to it.
    pub fn set_favourite(&self, id: &str, favourite: bool) {
        let changed = {
            let mut inner = lock(&self.inner);
            let changed = if favourite {
                inner.session.favourites.insert(id.to_owned())
            } else {
                inner.session.favourites.remove(id)
            };
            inner.dirty |= changed;
            changed
        };
        if changed {
            self.flush();
        }
    }

    /// Records whether the interface draws the inverted palette.
    pub fn set_inverted_palette(&self, inverted: bool) {
        let changed = {
            let mut inner = lock(&self.inner);
            let changed = inner.session.preferences.inverted_palette != inverted;
            inner.session.preferences.inverted_palette = inverted;
            inner.dirty |= changed;
            changed
        };
        if changed {
            self.flush();
        }
    }

    /// Offers the current state to the writer, which writes it once [`WRITE_DEBOUNCE`] has
    /// elapsed since its last write. Called on the tick.
    pub fn tick(&self) {
        if let Some(session) = self.take_dirty() {
            self.send(Write::Debounced(session));
        }
    }

    /// Hands any pending change to the writer to be written at once, for pause, stop, an
    /// episode change, a favourite, and a preference.
    ///
    /// A favourite and a preference are each a single user action, not a position that
    /// advances continuously, so each is worth its own write: a daemon killed a moment later
    /// must not lose one. Resetting the debounce the tick measures is correct rather than
    /// incidental - the write carries the pending position along with the favourite, so a
    /// debounced write moments later would rewrite a file that already holds everything.
    ///
    /// Returns as soon as the state is handed over; the file is written on the writer's
    /// thread, and [`Self::sync`] is what waits for it.
    pub fn flush(&self) {
        if let Some(session) = self.take_dirty() {
            self.send(Write::Now(session));
        }
    }

    /// Waits for every change handed over so far to reach the file, reporting what the last
    /// attempted write made of it.
    ///
    /// The one method here that blocks on the disk, so it belongs on the way out and not on
    /// the runtime.
    pub fn sync(&self) -> Result<()> {
        self.flush();
        self.drain(true)
    }

    /// The current state when it holds a change the writer has not been handed, marking it
    /// handed over.
    fn take_dirty(&self) -> Option<SessionState> {
        let mut inner = lock(&self.inner);
        if !inner.dirty {
            return None;
        }
        inner.dirty = false;
        Some(inner.session.clone())
    }

    /// Waits for the writer to have dealt with everything sent so far.
    fn drain(&self, force: bool) -> Result<()> {
        let (ack, answered) = mpsc::channel();
        self.send(Write::Drain { force, ack });
        // an answer is owed unless the writer is already gone, which is only possible once
        // this store is being dropped
        answered.recv().unwrap_or(Ok(()))
    }

    fn send(&self, write: Write) {
        if let Some(writes) = &self.writes {
            let _ = writes.send(write);
        }
    }
}

impl Drop for StateStore {
    /// Hands over anything still unwritten, then closes the channel and waits for the writer,
    /// so a store dropped without a [`StateStore::sync`] still leaves the file complete.
    fn drop(&mut self) {
        self.flush();
        self.writes = None;
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
    }
}

// == The writer thread ==

/// Writes the state file, and nothing else, for as long as the store lives.
///
/// The debounce lives here rather than with the caller because a state held back has to be
/// written when its interval elapses, with nothing further arriving to prompt it.
struct Writer {
    path: PathBuf,
    debounce: Duration,
    /// A state handed over and not yet on disk. Kept across a failure so the next attempt
    /// retries it rather than losing the change.
    pending: Option<SessionState>,
    last_write: Instant,
    /// What the last attempted write made of it, reported to the next drain and cleared by
    /// it.
    failure: Option<Error>,
}

impl Writer {
    fn new(path: PathBuf, debounce: Duration) -> Self {
        Self {
            path,
            debounce,
            pending: None,
            last_write: Instant::now(),
            failure: None,
        }
    }

    fn run(mut self, writes: &mpsc::Receiver<Write>) {
        let wakeup = self.debounce.max(WRITER_WAKEUP);
        loop {
            match writes.recv_timeout(wakeup) {
                Ok(Write::Debounced(session)) => {
                    self.pending = Some(session);
                    self.attempt_if_due();
                }
                Ok(Write::Now(session)) => {
                    self.pending = Some(session);
                    self.attempt();
                }
                Ok(Write::Drain { force, ack }) => {
                    if force {
                        self.attempt();
                    }
                    let _ = ack.send(self.failure.take().map_or(Ok(()), Err));
                }
                Err(RecvTimeoutError::Timeout) => self.attempt_if_due(),
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }

        // the store is gone: whatever it handed over last still belongs on disk
        self.attempt();
    }

    fn attempt_if_due(&mut self) {
        if self.last_write.elapsed() >= self.debounce {
            self.attempt();
        }
    }

    /// Writes whatever is pending, keeping it on failure so the next attempt retries it and
    /// the previously persisted file is left intact.
    fn attempt(&mut self) {
        let Some(session) = self.pending.take() else {
            return;
        };
        // stamped before the attempt so a failing write retries on the next interval rather
        // than on every wakeup
        self.last_write = Instant::now();
        match write_atomically(&self.path, &session) {
            Ok(()) => self.failure = None,
            Err(error) => {
                tracing::warn!(path = %self.path.display(), %error, "could not persist state");
                self.pending = Some(session);
                self.failure = Some(error);
            }
        }
    }
}

/// Whether this position sits inside either margin and so must not be recorded.
///
/// Both margins are inclusive: exactly 30 seconds in is within 30 seconds of the start.
/// An unknown duration leaves only the start margin to apply.
fn is_near_an_edge(position_secs: u64, duration_secs: Option<u64>) -> bool {
    if position_secs <= START_MARGIN_SECS {
        return true;
    }
    match duration_secs {
        // saturating, so a nonsensical position cannot wrap the sum into looking early
        Some(duration) => position_secs.saturating_add(END_MARGIN_SECS) >= duration,
        None => false,
    }
}

/// Writes through a temporary file and a rename, so a reader sees either the whole
/// previous contents or the whole new ones and never a truncated file.
fn write_atomically(path: &Path, session: &SessionState) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::Internal(format!("{} has no parent directory", path.display())))?;
    std::fs::create_dir_all(parent)?;

    let json = serde_json::to_vec_pretty(session)
        .map_err(|error| Error::Internal(format!("Could not serialise state: {error}")))?;

    // deliberately not `state.json`, so a half-written one is never loaded as the state
    let temp = path.with_extension("json.tmp");
    if let Err(error) = write_all_and_sync(&temp, &json) {
        let _ = std::fs::remove_file(&temp);
        return Err(error);
    }
    if let Err(error) = std::fs::rename(&temp, path) {
        let _ = std::fs::remove_file(&temp);
        return Err(error.into());
    }
    Ok(())
}

fn write_all_and_sync(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = std::fs::File::create(path)?;
    file.write_all(bytes)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOURS_TWO: u64 = 7_200;

    fn store(root: &Path) -> StateStore {
        StateStore::load(root.join("state.json"))
    }

    fn reload(root: &Path) -> SessionState {
        SessionState::read(&root.join("state.json"))
    }

    /// The state on disk once the writer has landed something satisfying `settled`.
    ///
    /// Persistence runs on its own thread, so a test asserting a change reached the file waits
    /// for it rather than racing it. The deadline is far shorter than any debounce, so a test
    /// still fails rather than passing on a write that waited.
    fn settles(root: &Path, settled: impl Fn(&SessionState) -> bool) -> SessionState {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let session = reload(root);
            if settled(&session) {
                return session;
            }
            assert!(Instant::now() < deadline, "the state was never written");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn a_state_file_from_before_volume_was_removed_still_loads() {
        // losing every position a listener has accumulated, over two fields that no longer
        // mean anything, would be far worse than ignoring them
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("state.json"),
            r#"{
                "positions": {"seventynine": 1800},
                "last_played": "seventynine",
                "volume": 0.3,
                "muted": true
            }"#,
        )
        .unwrap();

        let session = reload(root.path());

        assert_eq!(session.positions["seventynine"], 1_800);
        assert_eq!(session.last_played.as_deref(), Some("seventynine"));
    }

    #[test]
    fn nothing_about_loudness_is_written() {
        let root = tempfile::tempdir().unwrap();
        let store = store(root.path());

        store.record_position("seventynine", 1_800, Some(HOURS_TWO));
        store.sync().unwrap();

        let written = std::fs::read_to_string(root.path().join("state.json")).unwrap();
        assert!(!written.contains("volume"), "{written}");
        assert!(!written.contains("muted"), "{written}");
    }

    #[test]
    fn positions_for_two_episodes_are_independent() {
        let root = tempfile::tempdir().unwrap();
        let store = store(root.path());

        store.record_position("a", 1_800, Some(HOURS_TWO));
        store.record_position("b", 240, Some(HOURS_TWO));

        assert_eq!(store.session().positions["a"], 1_800);
        assert_eq!(store.session().positions["b"], 240);
    }

    #[test]
    fn a_position_just_inside_the_start_margin_is_not_recorded() {
        let root = tempfile::tempdir().unwrap();
        let store = store(root.path());

        store.record_position("a", 12, Some(HOURS_TWO));
        assert!(!store.session().positions.contains_key("a"));

        // exactly at the margin is still within it
        store.record_position("a", START_MARGIN_SECS, Some(HOURS_TWO));
        assert!(!store.session().positions.contains_key("a"));

        // one second past it is recorded
        store.record_position("a", START_MARGIN_SECS + 1, Some(HOURS_TWO));
        assert_eq!(store.session().positions["a"], START_MARGIN_SECS + 1);
    }

    #[test]
    fn a_position_just_inside_the_end_margin_is_not_recorded() {
        let root = tempfile::tempdir().unwrap();
        let store = store(root.path());

        store.record_position("a", 7_180, Some(HOURS_TWO));
        assert!(!store.session().positions.contains_key("a"));

        // exactly at the margin is still within it
        store.record_position("a", HOURS_TWO - END_MARGIN_SECS, Some(HOURS_TWO));
        assert!(!store.session().positions.contains_key("a"));

        // one second before it is recorded
        store.record_position("a", HOURS_TWO - END_MARGIN_SECS - 1, Some(HOURS_TWO));
        assert_eq!(
            store.session().positions["a"],
            HOURS_TWO - END_MARGIN_SECS - 1
        );
    }

    /// The sum is against the duration, and a position nonsensical enough to wrap it would
    /// otherwise read as early in a long episode rather than as the nonsense it is.
    #[test]
    fn a_position_that_would_overflow_the_end_margin_is_not_recorded() {
        let root = tempfile::tempdir().unwrap();
        let store = store(root.path());

        store.record_position("a", u64::MAX, Some(HOURS_TWO));
        store.record_position("b", u64::MAX - 1, None);

        assert!(!store.session().positions.contains_key("a"));
        // an unknown duration has no end margin to apply, so it is recorded as given
        assert_eq!(store.session().positions["b"], u64::MAX - 1);
    }

    #[test]
    fn a_position_outside_both_margins_is_recorded() {
        let root = tempfile::tempdir().unwrap();
        let store = store(root.path());

        store.record_position("a", 45, Some(HOURS_TWO));

        assert_eq!(store.session().positions["a"], 45);
    }

    #[test]
    fn an_edge_position_supersedes_an_older_recorded_position() {
        let root = tempfile::tempdir().unwrap();
        let store = store(root.path());
        store.record_position("a", 3_000, Some(HOURS_TWO));

        store.record_position("a", 15, Some(HOURS_TWO));

        assert!(!store.session().positions.contains_key("a"));
    }

    #[test]
    fn an_unknown_duration_still_applies_the_start_margin() {
        let root = tempfile::tempdir().unwrap();
        let store = store(root.path());

        store.record_position("a", 12, None);
        assert!(!store.session().positions.contains_key("a"));

        store.record_position("a", 1_800, None);
        assert_eq!(store.session().positions["a"], 1_800);
    }

    #[test]
    fn clearing_one_position_leaves_the_others_alone() {
        let root = tempfile::tempdir().unwrap();
        let store = store(root.path());
        store.record_position("a", 1_800, Some(HOURS_TWO));
        store.record_position("b", 240, Some(HOURS_TWO));

        store.clear_position("a");

        assert!(!store.session().positions.contains_key("a"));
        assert_eq!(store.session().positions["b"], 240);
    }

    #[test]
    fn clearing_a_position_that_does_not_exist_succeeds() {
        let root = tempfile::tempdir().unwrap();
        let store = store(root.path());

        store.clear_position("nothing");

        assert!(store.session().positions.is_empty());
    }

    #[test]
    fn clearing_every_position_empties_the_map() {
        let root = tempfile::tempdir().unwrap();
        let store = store(root.path());
        store.record_position("a", 1_800, Some(HOURS_TWO));
        store.record_position("b", 240, Some(HOURS_TWO));

        store.clear_all_positions();

        assert!(store.session().positions.is_empty());
    }

    #[test]
    fn an_empty_state_file_loads_as_an_empty_state() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("state.json"), "").unwrap();

        assert_eq!(reload(root.path()), SessionState::default());
    }

    #[test]
    fn a_truncated_or_unparseable_state_file_is_preserved_and_recovered_from() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("state.json");
        // a write cut off mid-document, which is exactly what a naive writer leaves
        std::fs::write(&path, r#"{"positions":{"a":18"#).unwrap();

        let session = reload(root.path());

        assert_eq!(session, SessionState::default());
        let preserved = root.path().join("state.json.corrupt");
        assert!(preserved.exists(), "the unparseable file was not preserved");
        assert_eq!(
            std::fs::read_to_string(&preserved).unwrap(),
            r#"{"positions":{"a":18"#
        );
        assert!(!path.exists(), "the unparseable file was left in place");
    }

    #[test]
    fn an_entry_for_an_episode_no_longer_in_the_catalog_is_retained() {
        let root = tempfile::tempdir().unwrap();
        let store = store(root.path());
        store.record_position("retired-episode", 2_400, Some(HOURS_TWO));
        store.sync().unwrap();

        let session = reload(root.path());

        assert_eq!(session.positions["retired-episode"], 2_400);
    }

    #[test]
    fn the_written_file_is_human_readable_json() {
        let root = tempfile::tempdir().unwrap();
        let store = store(root.path());
        store.record_position("a", 2_400, Some(HOURS_TWO));
        store.sync().unwrap();

        let text = std::fs::read_to_string(root.path().join("state.json")).unwrap();

        assert!(text.contains("\"positions\""), "{text}");
        assert!(text.contains('\n'), "the document was not pretty-printed");
        serde_json::from_str::<serde_json::Value>(&text).unwrap();
    }

    #[test]
    fn the_state_directory_is_created_on_first_write() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("mfp");
        let store = StateStore::load(dir.join("state.json"));
        store.record_position("a", 2_400, Some(HOURS_TWO));

        store.sync().unwrap();

        assert!(dir.join("state.json").is_file());
    }

    #[test]
    fn no_temporary_file_is_left_behind_by_a_successful_write() {
        let root = tempfile::tempdir().unwrap();
        let store = store(root.path());
        store.record_position("a", 2_400, Some(HOURS_TWO));

        store.sync().unwrap();

        assert!(!root.path().join("state.json.tmp").exists());
    }

    #[test]
    fn a_failed_write_leaves_the_previous_file_intact_and_loadable() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("state");
        std::fs::create_dir(&dir).unwrap();
        let store = StateStore::load(dir.join("state.json"));
        store.record_position("a", 2_400, Some(HOURS_TWO));
        store.sync().unwrap();
        let before = std::fs::read_to_string(dir.join("state.json")).unwrap();

        // the next write cannot create its temporary file
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        store.record_position("a", 3_000, Some(HOURS_TWO));
        let error = store.sync().unwrap_err();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();

        assert_eq!(error.code(), mfp_core::ErrorCode::Internal);
        assert_eq!(
            std::fs::read_to_string(dir.join("state.json")).unwrap(),
            before
        );
        assert_eq!(
            SessionState::read(&dir.join("state.json")).positions["a"],
            2_400
        );
        assert!(!dir.join("state.json.tmp").exists());
        // the change was kept rather than lost, so the writer retries it once it can
        store.sync().unwrap();
        assert_eq!(
            SessionState::read(&dir.join("state.json")).positions["a"],
            3_000
        );
    }

    #[test]
    fn a_write_is_debounced_while_playing_but_flushed_on_demand() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("state.json");
        let store = StateStore::load(path.clone());

        // steady playback: the debounce has not elapsed, so the tick writes nothing
        for position in [40, 41, 42, 43, 44] {
            store.record_position("a", position, Some(HOURS_TWO));
            store.tick();
        }
        // drained rather than synced, so the writer is caught up without being told to
        // write what the debounce is holding back
        store.drain(false).unwrap();
        assert!(
            !path.exists(),
            "the tick wrote inside the debounce interval"
        );

        // pause flushes immediately rather than waiting for the interval
        store.record_position("a", 1_000, Some(HOURS_TWO));
        store.sync().unwrap();

        assert_eq!(reload(root.path()).positions["a"], 1_000);
    }

    #[test]
    fn the_tick_writes_once_the_debounce_has_elapsed() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("state.json");
        // an elapsed interval rather than a wait, which is the same thing the writer sees
        let store = StateStore::with_debounce(path.clone(), Duration::ZERO);
        store.record_position("a", 1_000, Some(HOURS_TWO));

        store.tick();
        store.drain(false).unwrap();

        assert_eq!(reload(root.path()).positions["a"], 1_000);
        // and nothing further is written while the state is unchanged
        let written_at = std::fs::metadata(&path).unwrap().modified().unwrap();
        store.tick();
        store.drain(false).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            written_at
        );
    }

    #[test]
    fn flushing_an_unchanged_store_writes_nothing() {
        let root = tempfile::tempdir().unwrap();
        let store = store(root.path());

        store.sync().unwrap();

        assert!(!root.path().join("state.json").exists());
    }

    // == Favourites ==

    #[test]
    fn favourites_survive_a_restart() {
        let root = tempfile::tempdir().unwrap();
        let store = store(root.path());

        store.set_favourite("a", true);
        store.set_favourite("b", true);
        store.sync().unwrap();

        let session = reload(root.path());
        assert_eq!(
            session.favourites.iter().cloned().collect::<Vec<_>>(),
            vec!["a".to_owned(), "b".to_owned()]
        );
    }

    #[test]
    fn a_favourite_is_durable_without_a_clean_shutdown() {
        let root = tempfile::tempdir().unwrap();
        let store = store(root.path());

        store.set_favourite("a", true);
        // killed where it stands: nothing flushes, nothing drops cleanly
        std::mem::forget(store);

        // persistence is off the caller's thread, so the claim is that the write happens
        // on its own and not that it has already happened when the call returns
        assert!(
            settles(root.path(), |session| session.favourites.contains("a"))
                .favourites
                .contains("a")
        );
    }

    #[test]
    fn unfavouriting_is_durable_without_a_clean_shutdown() {
        let root = tempfile::tempdir().unwrap();
        let store = store(root.path());
        store.set_favourite("a", true);
        store.set_favourite("b", true);

        store.set_favourite("a", false);
        std::mem::forget(store);

        let session = settles(root.path(), |session| {
            session.favourites.contains("b") && !session.favourites.contains("a")
        });
        assert!(!session.favourites.contains("a"));
        assert!(session.favourites.contains("b"));
    }

    #[test]
    fn repeating_a_favourite_or_an_unfavourite_changes_nothing() {
        let root = tempfile::tempdir().unwrap();
        let store = store(root.path());

        store.set_favourite("a", true);
        store.set_favourite("a", true);
        assert_eq!(store.session().favourites.len(), 1);

        store.set_favourite("a", false);
        store.set_favourite("a", false);
        store.set_favourite("never-marked", false);
        assert!(store.session().favourites.is_empty());
    }

    #[test]
    fn a_favourite_is_keyed_by_the_same_identifier_as_a_position() {
        let root = tempfile::tempdir().unwrap();
        let store = store(root.path());

        store.record_position("ep-1700000000", 1_800, Some(HOURS_TWO));
        store.set_favourite("ep-1700000000", true);
        store.sync().unwrap();

        let session = reload(root.path());
        assert!(session.positions.contains_key("ep-1700000000"));
        assert!(session.favourites.contains("ep-1700000000"));
    }

    #[test]
    fn a_missing_or_corrupt_state_file_yields_no_favourites_rather_than_an_error() {
        let missing = tempfile::tempdir().unwrap();
        assert!(reload(missing.path()).favourites.is_empty());

        let corrupt = tempfile::tempdir().unwrap();
        std::fs::write(
            corrupt.path().join("state.json"),
            r#"{"favourites":["a","b"#,
        )
        .unwrap();
        assert!(reload(corrupt.path()).favourites.is_empty());
    }

    #[test]
    fn a_favourite_is_written_through_the_atomic_rename() {
        let root = tempfile::tempdir().unwrap();
        let store = store(root.path());

        store.set_favourite("a", true);
        store.drain(false).unwrap();

        assert!(
            !root.path().join("state.json.tmp").exists(),
            "the temporary file outlived the write"
        );
        let text = std::fs::read_to_string(root.path().join("state.json")).unwrap();
        assert!(text.contains("\"favourites\""), "{text}");
        serde_json::from_str::<serde_json::Value>(&text).unwrap();
    }

    // == Preferences ==

    #[test]
    fn absent_preferences_yield_the_defaults() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("state.json"),
            r#"{"positions":{"a":1800},"volume":0.3}"#,
        )
        .unwrap();

        let preferences = reload(root.path()).preferences;

        assert!(!preferences.inverted_palette);
        assert_eq!(preferences, Preferences::default());
    }

    #[test]
    fn an_unreadable_state_file_yields_the_default_preferences() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("state.json"), r#"{"preferences":{"inv"#).unwrap();

        assert_eq!(reload(root.path()).preferences, Preferences::default());
    }

    #[test]
    fn a_set_preference_round_trips() {
        let root = tempfile::tempdir().unwrap();
        let store = store(root.path());

        store.set_inverted_palette(true);
        store.drain(false).unwrap();

        assert!(reload(root.path()).preferences.inverted_palette);

        // and back again, over a store that read the file rather than wrote it
        let restarted = StateStore::load(root.path().join("state.json"));
        restarted.set_inverted_palette(false);
        restarted.drain(false).unwrap();
        assert!(!reload(root.path()).preferences.inverted_palette);
    }

    /// The cookie notice is gone and so is the flag recording its dismissal, but a file
    /// written before it went carries positions for four-hour episodes a listener is part-way
    /// through. Losing those over one dead boolean would be far worse than the notice ever
    /// was, so the field is dropped rather than rejected.
    #[test]
    fn a_state_file_carrying_the_dismissed_notice_flag_still_loads() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("state.json"),
            r#"{"positions":{"seventynine":7200,"seventyeight":120},"last_played":"seventynine","favourites":["seventyseven"],"preferences":{"inverted_palette":true,"notice_dismissed":true}}"#,
        )
        .unwrap();

        let session = reload(root.path());

        assert_eq!(session.positions["seventynine"], 7_200);
        assert_eq!(session.positions["seventyeight"], 120);
        assert_eq!(session.last_played.as_deref(), Some("seventynine"));
        assert!(session.favourites.contains("seventyseven"));
        assert!(session.preferences.inverted_palette);
    }
}
