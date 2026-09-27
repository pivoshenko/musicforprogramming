//! The "a newer version is available" notice: a background check that writes a cache, and a
//! line rendered from that cache.
//!
//! What is displayed is always what a check recorded, never the result of a check the caller
//! waited for. The check runs on a detached thread that the process may outlive by
//! milliseconds, so nothing here can make a command slower, and nothing here can make one
//! fail: every error - offline, rate-limited, unwritable cache - is dropped on the floor.
//!
//! The cache is consulted at most once a day, which is what `npm` and `brew` settled on: a
//! notice is worth one request a day and not one request a command.
//!
//! `$MFP_NO_UPDATE_CHECK` turns the whole thing off, request included.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::{CURRENT, fetch_latest_release, is_newer};

/// How long a recorded answer stands before another request is made.
const TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// How long a fast command will wait for its own check, so the first interactive run that
/// finds an update can say so rather than the one after it. Only ever spent when the check
/// was going to happen anyway and stderr is a terminal watching for the answer.
const GRACE: Duration = Duration::from_millis(400);

#[derive(Debug, Serialize, Deserialize)]
struct Entry {
    checked_at: u64,
    latest_version: String,
}

/// A check in flight. Dropping it abandons the thread, which is the normal end of a run.
pub struct Check {
    done: mpsc::Receiver<()>,
}

fn disabled() -> bool {
    std::env::var("MFP_NO_UPDATE_CHECK").is_ok_and(|value| !value.is_empty() && value != "0")
}

fn cache_path() -> Option<PathBuf> {
    mfp_core::paths::update_check_file().ok()
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

fn read(path: &Path) -> Option<Entry> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// Writes through a temporary file so a reader never sees half a document, and so two runs
/// checking at once cannot interleave into one.
fn write(path: &Path, entry: &Entry) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("json.part");
    std::fs::write(&temporary, serde_json::to_vec(entry)?)?;
    std::fs::rename(&temporary, path)
}

fn fresh(entry: &Entry, now: u64) -> bool {
    now.saturating_sub(entry.checked_at) < TTL.as_secs()
}

/// Starts a check unless one is not wanted or the recorded answer is still fresh.
///
/// The thread only writes the cache; it never prints, and the notice it enables is rendered
/// by whoever asks [`available`] next.
pub fn spawn() -> Option<Check> {
    if disabled() {
        return None;
    }
    let path = cache_path()?;
    if read(&path).is_some_and(|entry| fresh(&entry, now())) {
        return None;
    }

    let (tx, done) = mpsc::channel();
    std::thread::spawn(move || {
        if let Ok(release) = fetch_latest_release() {
            let _ = write(
                &path,
                &Entry {
                    checked_at: now(),
                    latest_version: release.version().to_string(),
                },
            );
        }
        let _ = tx.send(());
    });
    Some(Check { done })
}

/// Waits out [`GRACE`] for `check` when a person is watching, and not at all otherwise.
///
/// A detached thread dies with the process, so without this the check a fast command starts
/// never lands and the cache a scripted-only user has stays empty forever.
pub fn settle(check: Option<Check>) {
    let Some(check) = check else { return };
    if !std::io::stderr().is_terminal() {
        return;
    }
    let _ = check.done.recv_timeout(GRACE);
}

/// Whether `check` has finished, so a long-lived caller can pick the answer up mid-run.
pub fn landed(check: &mut Option<Check>) -> bool {
    let Some(inner) = check else { return false };
    match inner.done.try_recv() {
        Ok(()) | Err(mpsc::TryRecvError::Disconnected) => {
            *check = None;
            true
        }
        Err(mpsc::TryRecvError::Empty) => false,
    }
}

/// The recorded latest version, when it is newer than the running one.
pub fn available() -> Option<String> {
    if disabled() {
        return None;
    }
    let entry = read(&cache_path()?)?;
    is_newer(CURRENT, &entry.latest_version).then_some(entry.latest_version)
}

/// Prints the notice on stderr, where it cannot end up inside piped output.
///
/// Suppressed when stderr is not a terminal and when the command's own output is
/// machine-readable: a notice is for a person reading along, and one nobody reads is noise
/// in a log file.
pub fn print(suppress: bool) {
    if suppress || !std::io::stderr().is_terminal() {
        return;
    }
    if let Some(latest) = available() {
        eprintln!("{}", line(CURRENT, &latest));
    }
}

fn line(current: &str, latest: &str) -> String {
    format!(
        "A newer version is available: {current} -> {latest}. Run `{}`.",
        super::detect_install_method().upgrade_command()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_recorded_answer_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("update-check.json");
        write(
            &path,
            &Entry {
                checked_at: 1_700_000_000,
                latest_version: "9.9.9".into(),
            },
        )
        .unwrap();

        let entry = read(&path).unwrap();
        assert_eq!(entry.checked_at, 1_700_000_000);
        assert_eq!(entry.latest_version, "9.9.9");
    }

    #[test]
    fn a_missing_cache_is_no_answer_rather_than_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read(&dir.path().join("update-check.json")).is_none());
    }

    #[test]
    fn a_corrupt_cache_file_is_no_answer_rather_than_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("update-check.json");
        std::fs::write(&path, b"{ not json").unwrap();
        assert!(read(&path).is_none());
    }

    #[test]
    fn an_answer_is_fresh_until_the_ttl_elapses() {
        let entry = Entry {
            checked_at: 1_000_000,
            latest_version: "1.0.0".into(),
        };
        assert!(fresh(&entry, 1_000_000));
        assert!(fresh(&entry, 1_000_000 + TTL.as_secs() - 1));
        assert!(!fresh(&entry, 1_000_000 + TTL.as_secs()));
    }

    /// A cache written by a clock that has since moved backwards would otherwise read as
    /// aged by an enormous amount, or panic on the subtraction.
    #[test]
    fn an_answer_recorded_in_the_future_is_still_fresh() {
        let entry = Entry {
            checked_at: 2_000_000,
            latest_version: "1.0.0".into(),
        };
        assert!(fresh(&entry, 1_000_000));
    }

    #[test]
    fn the_notice_names_both_versions_and_a_command() {
        let text = line("1.0.0", "1.1.0");
        assert!(text.contains("1.0.0"), "{text}");
        assert!(text.contains("1.1.0"), "{text}");
        assert!(text.contains('`'), "{text}");
    }

    #[test]
    fn the_notice_is_one_line_so_it_cannot_be_mistaken_for_output() {
        assert!(!line("1.0.0", "1.1.0").contains('\n'));
    }

    #[test]
    fn a_landed_check_is_taken_and_not_reported_twice() {
        let (tx, done) = mpsc::channel();
        let mut check = Some(Check { done });
        assert!(!landed(&mut check));
        tx.send(()).unwrap();
        assert!(landed(&mut check));
        assert!(check.is_none());
        assert!(!landed(&mut check));
    }

    #[test]
    fn a_check_whose_thread_vanished_counts_as_landed() {
        let (tx, done) = mpsc::channel::<()>();
        drop(tx);
        let mut check = Some(Check { done });
        assert!(landed(&mut check));
    }

    #[test]
    fn no_check_never_reports_as_landed() {
        assert!(!landed(&mut None));
    }
}
