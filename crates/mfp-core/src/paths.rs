//! Where the player keeps its configuration, caches, durable state, logs, and socket. The
//! daemon and every client resolve these identically, so they always agree on the endpoint
//! and on which files they share.
//!
//! Every location has an explicit override: `$MFP_SOCKET`, `$MFP_CONFIG_DIR`,
//! `$MFP_CACHE_DIR`, and `$MFP_STATE_DIR`. Isolating an instance takes all of them -
//! scoping the socket alone leaves two daemons writing one `state.json` and one cache.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Every directory the player owns, named after the binary as the `$MFP_*` overrides are.
const APP_DIR: &str = "mfp";
const SOCKET_NAME: &str = "daemon.sock";

fn env_var(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|value| !value.is_empty())
}

fn home_dir() -> Result<PathBuf> {
    directories::BaseDirs::new()
        .map(|dirs| dirs.home_dir().to_path_buf())
        .ok_or_else(|| Error::Internal("cannot determine the home directory".into()))
}

fn current_uid() -> u32 {
    // SAFETY: getuid is always safe; it reads the calling process's real user id
    unsafe { libc::getuid() }
}

/// The environment override in force for one of the config, cache, or state directories.
fn overridden_dir(explicit_key: &str) -> Option<PathBuf> {
    resolve_overridden_dir(env_var(explicit_key).as_deref())
}

/// An explicit `$MFP_*_DIR` is used as given: it names one instance's own directory, not
/// a root shared with other applications. `None` leaves the platform default in force.
///
/// Deliberately no `$XDG_*_HOME` fallback: honouring one without the others scatters an
/// instance across several roots - config read from one place, state written to another.
fn resolve_overridden_dir(explicit: Option<&str>) -> Option<PathBuf> {
    explicit
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// The directory holding `config.toml`: `$MFP_CONFIG_DIR` if set, otherwise
/// `~/.config/mfp` on every platform.
pub fn config_dir() -> Result<PathBuf> {
    if let Some(dir) = overridden_dir("MFP_CONFIG_DIR") {
        return Ok(dir);
    }
    Ok(home_dir()?.join(".config").join(APP_DIR))
}

/// The optional configuration file. Its absence is not an error; see [`crate::config`].
pub fn config_file() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.toml"))
}

/// The directory holding the catalog cache and downloaded audio: `$MFP_CACHE_DIR` if set,
/// otherwise `~/.cache/mfp` on every platform.
///
/// Deliberately not `~/Library/Caches` on macOS: the downloads here are a library a user
/// browses and copies from, not a cache the system may reclaim behind their back.
pub fn cache_dir() -> Result<PathBuf> {
    if let Some(dir) = overridden_dir("MFP_CACHE_DIR") {
        return Ok(dir);
    }
    Ok(home_dir()?.join(".cache").join(APP_DIR))
}

/// The directory holding `<identifier>.mp3` and `<identifier>.mp3.part` files.
pub fn audio_cache_dir() -> Result<PathBuf> {
    Ok(cache_dir()?.join("audio").join("episodes"))
}

/// The normalised JSON document holding the cached catalog.
pub fn catalog_cache_file() -> Result<PathBuf> {
    Ok(cache_dir()?.join("catalog.json"))
}

/// The directory holding `state.json` and `daemon.log`: `$MFP_STATE_DIR` if set, otherwise
/// `~/.local/state/mfp` on every platform.
pub fn state_dir() -> Result<PathBuf> {
    if let Some(dir) = overridden_dir("MFP_STATE_DIR") {
        return Ok(dir);
    }
    Ok(home_dir()?.join(".local").join("state").join(APP_DIR))
}

/// The durable listening state file.
pub fn state_file() -> Result<PathBuf> {
    Ok(state_dir()?.join("state.json"))
}

/// The file an autostarted daemon's output is redirected to, and the path a client names
/// when it reports the daemon as unreachable.
pub fn log_file() -> Result<PathBuf> {
    Ok(state_dir()?.join("daemon.log"))
}

/// The Unix domain socket the daemon listens on: `$MFP_SOCKET` if set, otherwise
/// `$XDG_RUNTIME_DIR/mfp/daemon.sock` where that variable is set, and
/// `$TMPDIR/mfp-$UID/daemon.sock` otherwise, with `$TMPDIR` falling back to `/tmp`.
pub fn socket_path() -> PathBuf {
    resolve_socket_path(
        env_var("MFP_SOCKET").as_deref(),
        env_var("XDG_RUNTIME_DIR").as_deref(),
        env_var("TMPDIR").as_deref(),
        current_uid(),
    )
}

fn resolve_socket_path(
    mfp_socket: Option<&str>,
    xdg_runtime_dir: Option<&str>,
    tmpdir: Option<&str>,
    uid: u32,
) -> PathBuf {
    if let Some(explicit) = mfp_socket.filter(|value| !value.is_empty()) {
        return PathBuf::from(explicit);
    }
    if let Some(runtime) = xdg_runtime_dir.filter(|value| !value.is_empty()) {
        return Path::new(runtime).join(APP_DIR).join(SOCKET_NAME);
    }
    let tmp = tmpdir.filter(|value| !value.is_empty()).unwrap_or("/tmp");
    Path::new(tmp)
        .join(format!("{APP_DIR}-{uid}"))
        .join(SOCKET_NAME)
}

/// Creates the socket's parent directory mode `0700`, tightening it if it already exists
/// with broader permissions. Fails rather than binding when it belongs to another user, is
/// a symbolic link, or is something other than a directory.
pub fn ensure_socket_dir(socket_path: &Path) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

    let dir = socket_path.parent().ok_or_else(|| {
        Error::Internal(format!(
            "socket path {} has no parent directory",
            socket_path.display()
        ))
    })?;

    // another user can pre-create the default directory in world-writable `/tmp` as a link
    // to one of ours, which `metadata` would follow: chmod and bind inside a directory of
    // their choosing
    match std::fs::symlink_metadata(dir) {
        Ok(metadata) => {
            if metadata.is_symlink() {
                return Err(Error::Internal(format!(
                    "{} is a symbolic link",
                    dir.display()
                )));
            }
            if !metadata.is_dir() {
                return Err(Error::Internal(format!(
                    "{} exists and is not a directory",
                    dir.display()
                )));
            }
            let uid = current_uid();
            if metadata.uid() != uid {
                return Err(Error::Internal(format!(
                    "{} is owned by uid {} rather than uid {}",
                    dir.display(),
                    metadata.uid(),
                    uid
                )));
            }
            if metadata.permissions().mode() & 0o777 != 0o700 {
                std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)?;
        }
        Err(error) => return Err(error.into()),
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn xdg_runtime_dir_branch() {
        assert_eq!(
            resolve_socket_path(None, Some("/run/user/1000"), None, 1000),
            PathBuf::from("/run/user/1000/mfp/daemon.sock")
        );
    }

    #[test]
    fn tmpdir_branch_when_xdg_runtime_dir_is_absent() {
        assert_eq!(
            resolve_socket_path(None, None, Some("/var/folders/ab/T/"), 501),
            PathBuf::from("/var/folders/ab/T/mfp-501/daemon.sock")
        );
    }

    #[test]
    fn tmpdir_branch_falls_back_to_tmp() {
        assert_eq!(
            resolve_socket_path(None, None, None, 501),
            PathBuf::from("/tmp/mfp-501/daemon.sock")
        );
    }

    #[test]
    fn empty_variables_are_treated_as_unset() {
        assert_eq!(
            resolve_socket_path(Some(""), Some(""), Some(""), 7),
            PathBuf::from("/tmp/mfp-7/daemon.sock")
        );
    }

    #[test]
    fn explicit_override_wins_over_xdg_runtime_dir() {
        assert_eq!(
            resolve_socket_path(
                Some("/tmp/custom.sock"),
                Some("/run/user/1000"),
                Some("/var/tmp"),
                1000
            ),
            PathBuf::from("/tmp/custom.sock")
        );
    }

    #[test]
    fn an_explicit_directory_override_wins_over_xdg() {
        assert_eq!(
            resolve_overridden_dir(Some("/tmp/instance-a/state")),
            Some(PathBuf::from("/tmp/instance-a/state"))
        );
    }

    #[test]
    fn without_an_explicit_override_the_platform_default_stays_in_force() {
        assert_eq!(resolve_overridden_dir(None), None);
    }

    #[test]
    fn empty_directory_variables_are_treated_as_unset() {
        assert_eq!(resolve_overridden_dir(Some("")), None);
    }

    #[test]
    fn missing_socket_directory_is_created_private() {
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("mfp").join("daemon.sock");

        ensure_socket_dir(&socket).unwrap();

        let mode = std::fs::metadata(socket.parent().unwrap())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[test]
    fn a_world_readable_socket_directory_is_tightened() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("mfp");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        ensure_socket_dir(&dir.join("daemon.sock")).unwrap();

        let mode = std::fs::metadata(&dir).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[test]
    fn a_symlinked_socket_directory_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("elsewhere");
        let link = root.path().join("mfp");
        std::fs::create_dir(&target).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let error = ensure_socket_dir(&link.join("daemon.sock")).unwrap_err();

        assert_eq!(error.code(), crate::error::ErrorCode::Internal);
        assert!(error.to_string().contains("symbolic link"));
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    #[test]
    fn a_file_occupying_the_directory_path_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let occupied = root.path().join("mfp");
        std::fs::write(&occupied, b"not a directory").unwrap();

        let error = ensure_socket_dir(&occupied.join("daemon.sock")).unwrap_err();

        assert_eq!(error.code(), crate::error::ErrorCode::Internal);
        assert!(error.to_string().contains("not a directory"));
    }
}
