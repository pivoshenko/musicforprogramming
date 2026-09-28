use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Seconds the daemon may sit idle before exiting. `None`, the default, disables it.
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
    pub fn load() -> Result<Self> {
        Self::load_from(&crate::paths::config_file()?)
    }

    /// Returns the defaults when `path` does not exist; a file that exists but will not
    /// parse is an error, so a typo is never silently ignored.
    pub fn load_from(path: &Path) -> Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(error.into()),
        };

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
