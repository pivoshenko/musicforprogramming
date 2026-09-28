use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use mfp_core::error::{Error, Result};
use mfp_core::protocol::StateSnapshot;
use serde::{Deserialize, Serialize};

pub type SharedState = Arc<Mutex<StateSnapshot>>;

pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

pub const WRITE_DEBOUNCE: Duration = Duration::from_secs(5);

const WRITER_WAKEUP: Duration = Duration::from_millis(50);

pub const START_MARGIN_SECS: u64 = 30;

pub const END_MARGIN_SECS: u64 = 60;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionState {
    pub positions: BTreeMap<String, u64>,
    pub last_played: Option<String>,

    pub favourites: BTreeSet<String>,
    pub preferences: Preferences,
}

impl SessionState {
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Preferences {
    pub inverted_palette: bool,
}

pub struct StateStore {
    inner: Mutex<Inner>,

    writes: Option<mpsc::Sender<Write>>,
    writer: Option<std::thread::JoinHandle<()>>,
}

struct Inner {
    session: SessionState,

    dirty: bool,
}

enum Write {
    Debounced(SessionState),

    Now(SessionState),

    Drain {
        force: bool,
        ack: mpsc::Sender<Result<()>>,
    },
}

impl StateStore {
    pub fn load(path: PathBuf) -> Self {
        Self::with_debounce(path, WRITE_DEBOUNCE)
    }

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

    pub fn session(&self) -> SessionState {
        lock(&self.inner).session.clone()
    }

    pub fn position(&self, id: &str) -> Option<u64> {
        lock(&self.inner).session.positions.get(id).copied()
    }

    pub fn last_played(&self) -> Option<String> {
        lock(&self.inner).session.last_played.clone()
    }

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

    pub fn set_last_played(&self, id: &str) {
        let mut inner = lock(&self.inner);
        if inner.session.last_played.as_deref() != Some(id) {
            inner.session.last_played = Some(id.to_owned());
            inner.dirty = true;
        }
    }

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

    pub fn tick(&self) {
        if let Some(session) = self.take_dirty() {
            self.send(Write::Debounced(session));
        }
    }

    pub fn flush(&self) {
        if let Some(session) = self.take_dirty() {
            self.send(Write::Now(session));
        }
    }

    pub fn sync(&self) -> Result<()> {
        self.flush();
        self.drain(true)
    }

    fn take_dirty(&self) -> Option<SessionState> {
        let mut inner = lock(&self.inner);
        if !inner.dirty {
            return None;
        }
        inner.dirty = false;
        Some(inner.session.clone())
    }

    fn drain(&self, force: bool) -> Result<()> {
        let (ack, answered) = mpsc::channel();
        self.send(Write::Drain { force, ack });

        answered.recv().unwrap_or(Ok(()))
    }

    fn send(&self, write: Write) {
        if let Some(writes) = &self.writes {
            let _ = writes.send(write);
        }
    }
}

impl Drop for StateStore {
    fn drop(&mut self) {
        self.flush();
        self.writes = None;
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
    }
}

struct Writer {
    path: PathBuf,
    debounce: Duration,

    pending: Option<SessionState>,
    last_write: Instant,

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

        self.attempt();
    }

    fn attempt_if_due(&mut self) {
        if self.last_write.elapsed() >= self.debounce {
            self.attempt();
        }
    }

    fn attempt(&mut self) {
        let Some(session) = self.pending.take() else {
            return;
        };

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

fn is_near_an_edge(position_secs: u64, duration_secs: Option<u64>) -> bool {
    if position_secs <= START_MARGIN_SECS {
        return true;
    }
    match duration_secs {
        Some(duration) => position_secs.saturating_add(END_MARGIN_SECS) >= duration,
        None => false,
    }
}

fn write_atomically(path: &Path, session: &SessionState) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::Internal(format!("{} has no parent directory", path.display())))?;
    std::fs::create_dir_all(parent)?;

    let json = serde_json::to_vec_pretty(session)
        .map_err(|error| Error::Internal(format!("Could not serialise state: {error}")))?;

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

        store.record_position("a", START_MARGIN_SECS, Some(HOURS_TWO));
        assert!(!store.session().positions.contains_key("a"));

        store.record_position("a", START_MARGIN_SECS + 1, Some(HOURS_TWO));
        assert_eq!(store.session().positions["a"], START_MARGIN_SECS + 1);
    }

    #[test]
    fn a_position_just_inside_the_end_margin_is_not_recorded() {
        let root = tempfile::tempdir().unwrap();
        let store = store(root.path());

        store.record_position("a", 7_180, Some(HOURS_TWO));
        assert!(!store.session().positions.contains_key("a"));

        store.record_position("a", HOURS_TWO - END_MARGIN_SECS, Some(HOURS_TWO));
        assert!(!store.session().positions.contains_key("a"));

        store.record_position("a", HOURS_TWO - END_MARGIN_SECS - 1, Some(HOURS_TWO));
        assert_eq!(
            store.session().positions["a"],
            HOURS_TWO - END_MARGIN_SECS - 1
        );
    }

    #[test]
    fn a_position_that_would_overflow_the_end_margin_is_not_recorded() {
        let root = tempfile::tempdir().unwrap();
        let store = store(root.path());

        store.record_position("a", u64::MAX, Some(HOURS_TWO));
        store.record_position("b", u64::MAX - 1, None);

        assert!(!store.session().positions.contains_key("a"));

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

        for position in [40, 41, 42, 43, 44] {
            store.record_position("a", position, Some(HOURS_TWO));
            store.tick();
        }

        store.drain(false).unwrap();
        assert!(
            !path.exists(),
            "the tick wrote inside the debounce interval"
        );

        store.record_position("a", 1_000, Some(HOURS_TWO));
        store.sync().unwrap();

        assert_eq!(reload(root.path()).positions["a"], 1_000);
    }

    #[test]
    fn the_tick_writes_once_the_debounce_has_elapsed() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("state.json");

        let store = StateStore::with_debounce(path.clone(), Duration::ZERO);
        store.record_position("a", 1_000, Some(HOURS_TWO));

        store.tick();
        store.drain(false).unwrap();

        assert_eq!(reload(root.path()).positions["a"], 1_000);

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

        std::mem::forget(store);

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

        let restarted = StateStore::load(root.path().join("state.json"));
        restarted.set_inverted_palette(false);
        restarted.drain(false).unwrap();
        assert!(!reload(root.path()).preferences.inverted_palette);
    }

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
