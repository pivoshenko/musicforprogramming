//! What the download cache holds, how big it is, and removing episodes from it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use mfp_core::error::{Error, Result};

/// The suffix a complete verified episode carries.
const COMPLETE_SUFFIX: &str = ".mp3";

/// The suffix an in-progress transfer carries.
const PART_SUFFIX: &str = ".mp3.part";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EpisodeCacheState {
    NotCached,
    /// A part file exists. It is never a playback source.
    Partial {
        bytes: u64,
    },
    /// A complete verified file exists.
    Cached {
        bytes: u64,
    },
}

impl EpisodeCacheState {
    /// How many bytes this episode occupies, zero when it is not cached.
    pub fn bytes(self) -> u64 {
        match self {
            Self::NotCached => 0,
            Self::Partial { bytes } | Self::Cached { bytes } => bytes,
        }
    }

    /// Whether a complete verified file exists, the only state playback may source from disk.
    pub fn is_cached(self) -> bool {
        matches!(self, Self::Cached { .. })
    }
}

/// Whether this identifier names one file inside the cache directory and nothing else.
///
/// The same property [`mfp_core::Episode::id`] documents and `mfp-core` refuses an
/// enrichment slug for, enforced again at the sink: a separator, a parent reference or a NUL
/// from anywhere - a malformed feed, a future caller that skips the catalog - would otherwise
/// let [`evict`] remove a file outside the cache and a transfer write one there.
pub fn names_one_entry(id: &str) -> bool {
    !id.is_empty() && id != "." && id != ".." && !id.contains(['/', '\\', '\0'])
}

/// Where a complete verified episode lives.
///
/// A pure join: the three entry points that touch the filesystem by identifier -
/// [`state_of`], [`evict`] and [`crate::download::DownloadManager::start`] - gate on
/// [`names_one_entry`] first.
pub fn audio_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}{COMPLETE_SUFFIX}"))
}

/// Where an in-progress transfer writes. A pure join, gated as [`audio_path`] is.
pub fn part_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}{PART_SUFFIX}"))
}

/// The length of `path`, or `None` when it is absent or is not a regular file.
pub(crate) fn file_len(path: &Path) -> Option<u64> {
    std::fs::metadata(path)
        .ok()
        .filter(|metadata| metadata.is_file())
        .map(|metadata| metadata.len())
}

/// What the cache holds for this episode.
///
/// A complete file wins over a part file: once the rename has happened the episode is
/// cached whatever else is lying beside it.
pub fn state_of(dir: &Path, id: &str) -> EpisodeCacheState {
    // nothing is written under a name the cache would not write, so nothing is cached
    // under one either
    if !names_one_entry(id) {
        return EpisodeCacheState::NotCached;
    }
    if let Some(bytes) = file_len(&audio_path(dir, id)) {
        return EpisodeCacheState::Cached { bytes };
    }
    match file_len(&part_path(dir, id)) {
        Some(bytes) => EpisodeCacheState::Partial { bytes },
        None => EpisodeCacheState::NotCached,
    }
}

/// Every episode the cache directory holds, keyed by identifier.
///
/// A cache directory that does not exist yet is empty, not an error.
pub fn list(dir: &Path) -> BTreeMap<String, EpisodeCacheState> {
    let mut entries = BTreeMap::new();
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return entries;
    };

    for entry in read_dir.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let Some(id) = name
            .strip_suffix(PART_SUFFIX)
            .or_else(|| name.strip_suffix(COMPLETE_SUFFIX))
        else {
            continue;
        };
        // resolved from the directory rather than this entry, so a part file never masks
        // the complete file beside it
        entries.insert(id.to_owned(), state_of(dir, id));
    }

    entries
}

/// The cache's total size, counting complete files and part files. A cache directory that
/// does not exist yet is zero, not an error.
pub fn total_size(dir: &Path) -> u64 {
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return 0;
    };

    read_dir
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.ends_with(COMPLETE_SUFFIX) || name.ends_with(PART_SUFFIX))
        })
        .filter_map(|entry| file_len(&entry.path()))
        .sum()
}

/// Removes an episode's complete file and any part file, returning the bytes reclaimed.
///
/// Removing an episode that is not cached succeeds with nothing reclaimed. The caller
/// cancels an in-progress download first, and refuses to evict a file currently playing;
/// [`crate::download::DownloadManager::evict`] does both.
pub fn evict(dir: &Path, id: &str) -> Result<u64> {
    if !names_one_entry(id) {
        return Err(Error::Internal(format!("{id} does not name a cache entry")));
    }
    let mut reclaimed = 0;
    for path in [audio_path(dir, id), part_path(dir, id)] {
        let Some(bytes) = file_len(&path) else {
            continue;
        };
        match std::fs::remove_file(&path) {
            Ok(()) => reclaimed += bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(reclaimed)
}

/// Bytes available on the filesystem holding `dir`.
///
/// `dir` need not exist yet: the nearest existing ancestor is measured instead, since that
/// is the filesystem the directory would be created on.
pub fn free_space(dir: &Path) -> Result<u64> {
    use std::os::unix::ffi::OsStrExt;

    let mut probe = dir;
    while !probe.exists() {
        probe = probe.parent().ok_or_else(|| {
            Error::Internal(format!(
                "No existing ancestor of {} to measure free space on",
                dir.display()
            ))
        })?;
    }

    let path = std::ffi::CString::new(probe.as_os_str().as_bytes())
        .map_err(|_| Error::Internal(format!("{} contains a NUL byte", probe.display())))?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: path is a valid NUL-terminated C string and stat is a writable statvfs
    if unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: statvfs returned 0, so it initialised the struct
    let stat = unsafe { stat.assume_init() };

    // the two field widths differ between macOS and Linux, so widen rather than cast
    Ok(widen(stat.f_bavail).saturating_mul(widen(stat.f_frsize)))
}

fn widen(value: impl Into<u64>) -> u64 {
    value.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, bytes: usize) {
        std::fs::write(path, vec![0u8; bytes]).unwrap();
    }

    #[test]
    fn an_empty_cache_reports_zero_and_not_cached() {
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("audio");

        assert_eq!(total_size(&missing), 0);
        assert_eq!(
            state_of(&missing, "seventynine"),
            EpisodeCacheState::NotCached
        );
        assert!(list(&missing).is_empty());
    }

    #[test]
    fn a_part_file_reports_partial_and_a_complete_file_reports_cached() {
        let root = tempfile::tempdir().unwrap();
        write(&part_path(root.path(), "seventynine"), 100);

        assert_eq!(
            state_of(root.path(), "seventynine"),
            EpisodeCacheState::Partial { bytes: 100 }
        );

        write(&audio_path(root.path(), "seventynine"), 400);

        assert_eq!(
            state_of(root.path(), "seventynine"),
            EpisodeCacheState::Cached { bytes: 400 }
        );
    }

    #[test]
    fn the_total_counts_complete_files_and_part_files() {
        let root = tempfile::tempdir().unwrap();
        write(&audio_path(root.path(), "one"), 10);
        write(&audio_path(root.path(), "two"), 20);
        write(&audio_path(root.path(), "three"), 30);
        write(&part_path(root.path(), "four"), 5);
        // an unrelated file the cache does not own
        write(&root.path().join("catalog.json"), 999);

        assert_eq!(total_size(root.path()), 65);

        let listing = list(root.path());
        assert_eq!(listing.len(), 4);
        assert_eq!(listing["one"], EpisodeCacheState::Cached { bytes: 10 });
        assert_eq!(listing["four"], EpisodeCacheState::Partial { bytes: 5 });
        assert!(!listing.contains_key("catalog"));
    }

    #[test]
    fn eviction_removes_both_files_and_reports_the_bytes_reclaimed() {
        let root = tempfile::tempdir().unwrap();
        write(&audio_path(root.path(), "seventynine"), 400);
        write(&part_path(root.path(), "seventynine"), 100);
        write(&audio_path(root.path(), "seventyeight"), 7);

        assert_eq!(evict(root.path(), "seventynine").unwrap(), 500);

        assert_eq!(
            state_of(root.path(), "seventynine"),
            EpisodeCacheState::NotCached
        );
        assert!(!audio_path(root.path(), "seventynine").exists());
        assert!(!part_path(root.path(), "seventynine").exists());
        assert_eq!(total_size(root.path()), 7);
    }

    #[test]
    fn evicting_an_episode_that_is_not_cached_reclaims_nothing() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(evict(root.path(), "seventynine").unwrap(), 0);
    }

    #[test]
    fn an_identifier_that_could_name_a_file_outside_the_cache_is_refused() {
        for id in ["../escape", "one/two", "/absolute", "..", ".", "", "a\0b"] {
            assert!(!names_one_entry(id), "{id:?} was accepted");
        }
        for id in ["seventynine", "ep-0123456789abcdef", "a.b"] {
            assert!(names_one_entry(id), "{id:?} was refused");
        }
    }

    #[test]
    fn a_traversing_identifier_is_never_cached_and_never_evicted() {
        let root = tempfile::tempdir().unwrap();
        let cache = root.path().join("audio");
        std::fs::create_dir_all(&cache).unwrap();
        // what an escape would reach, sitting where the join would land
        let outside = root.path().join("escape.mp3");
        write(&outside, 64);

        assert_eq!(state_of(&cache, "../escape"), EpisodeCacheState::NotCached);
        assert_eq!(
            evict(&cache, "../escape").unwrap_err().code(),
            mfp_core::ErrorCode::Internal
        );
        assert!(outside.exists(), "a file outside the cache was removed");
    }

    #[test]
    fn free_space_is_reported_for_a_directory_that_does_not_exist_yet() {
        let root = tempfile::tempdir().unwrap();
        let unborn = root.path().join("audio").join("nested");

        let free = free_space(&unborn).unwrap();

        assert!(free > 0, "a writable temp directory should have free space");
    }
}
