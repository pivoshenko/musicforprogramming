//! The optional configuration file.
//!
//! Every setting has a default, so a missing file yields those defaults rather than an error
//! and the player runs unconfigured. A file that exists but cannot be parsed is an error:
//! ignoring a typo would leave a user's setting quietly unapplied.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Player settings, as read from `config.toml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Seconds the daemon may sit with nothing playing, nothing downloading, and no client
    /// connected before it exits. `None`, the default, disables it - it idles indefinitely.
    pub idle_timeout_secs: Option<u64>,
    /// How many downloads may transfer at once. Further requests queue.
    pub max_concurrent_downloads: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            idle_timeout_secs: None,
            max_concurrent_downloads: 2,
        }
    }
}

impl Config {
    /// Loads the configuration from [`crate::paths::config_file`].
    pub fn load() -> Result<Self> {
        Self::load_from(&crate::paths::config_file()?)
    }

    /// Loads the configuration from `path`, returning the defaults when it does not exist.
    pub fn load_from(path: &Path) -> Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(error.into()),
        };

        // a typo in a user's own file is their mistake to see, not an internal one
        toml::from_str(&text).map_err(|error| {
            Error::InvalidParams(format!("{} is not valid: {error}", path.display()))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_file_yields_defaults() {
        let root = tempfile::tempdir().unwrap();
        let config = Config::load_from(&root.path().join("config.toml")).unwrap();
        assert_eq!(config, Config::default());
    }

    #[test]
    fn the_idle_timeout_defaults_to_disabled() {
        assert_eq!(Config::default().idle_timeout_secs, None);
    }

    #[test]
    fn an_empty_file_yields_defaults() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.toml");
        std::fs::write(&path, "").unwrap();
        assert_eq!(Config::load_from(&path).unwrap(), Config::default());
    }

    #[test]
    fn a_partial_file_leaves_the_rest_at_their_defaults() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.toml");
        std::fs::write(&path, "idle_timeout_secs = 600\n").unwrap();

        let config = Config::load_from(&path).unwrap();

        assert_eq!(config.idle_timeout_secs, Some(600));
        assert_eq!(
            config.max_concurrent_downloads,
            Config::default().max_concurrent_downloads
        );
    }

    /// Loudness belongs to the operating system's mixer and the cache limit was never
    /// implemented, so both settings are gone - but a file naming them must still load the
    /// settings beside them.
    #[test]
    fn a_file_naming_a_setting_that_no_longer_exists_still_loads() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.toml");
        std::fs::write(
            &path,
            "volume = 0.3\nmuted = true\ncache_max_bytes = 1024\nidle_timeout_secs = 600\n",
        )
        .unwrap();

        let config = Config::load_from(&path).unwrap();

        assert_eq!(config.idle_timeout_secs, Some(600));
    }

    #[test]
    fn an_unparseable_file_is_reported_rather_than_ignored() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.toml");
        std::fs::write(&path, "idle_timeout_secs = = 3\n").unwrap();

        let error = Config::load_from(&path).unwrap_err();

        assert_eq!(error.code(), crate::error::ErrorCode::InvalidParams);
        assert!(error.to_string().contains("is not valid"));
    }
}
