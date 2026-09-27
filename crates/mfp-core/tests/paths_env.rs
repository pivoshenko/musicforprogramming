//! The one test that steers the path resolvers through the process environment.
//!
//! It lives in its own integration test file, and so in its own process with a single test.
//! `std::env::set_var` is unsafe: the write races every thread reading the environment,
//! including the C code behind DNS, TLS, and log timestamps, and the unit test binary runs
//! its tests on a thread pool - so the same test there could not justify its own writes.

// An integration test is its own crate, so the library's cfg(test) exemption does not reach it
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::float_cmp
)]

use std::path::PathBuf;

use mfp_core::paths::{
    audio_cache_dir, cache_dir, catalog_cache_file, config_dir, config_file, log_file, state_dir,
    state_file, update_check_file,
};

#[test]
fn directory_overrides_scope_an_instance_without_disturbing_the_defaults() {
    let _restore = TakenVars::take(&[
        "MFP_CONFIG_DIR",
        "MFP_CACHE_DIR",
        "MFP_STATE_DIR",
        "XDG_CONFIG_HOME",
        "XDG_CACHE_HOME",
        "XDG_STATE_HOME",
    ]);
    // the XDG variables above are taken purely to prove they are ignored: the defaults
    // below must hold whether or not the caller has them set

    let home = PathBuf::from(std::env::var("HOME").expect("HOME names the home directory"));
    // none of the three roots varies by platform
    let config = home.join(".config").join("mfp");
    let cache = home.join(".cache").join("mfp");
    let state = home.join(".local").join("state").join("mfp");
    assert_eq!(config_dir().unwrap(), config);
    assert_eq!(config_file().unwrap(), config.join("config.toml"));
    assert_eq!(cache_dir().unwrap(), cache);
    assert_eq!(catalog_cache_file().unwrap(), cache.join("catalog.json"));
    assert_eq!(
        audio_cache_dir().unwrap(),
        cache.join("audio").join("episodes")
    );
    assert_eq!(
        update_check_file().unwrap(),
        cache.join("update-check.json")
    );
    assert_eq!(state_dir().unwrap(), state);
    assert_eq!(state_file().unwrap(), state.join("state.json"));
    assert_eq!(log_file().unwrap(), state.join("daemon.log"));

    // setting an XDG root must NOT move anything: honouring one of these without the
    // others scatters an instance across several roots
    set_var("XDG_STATE_HOME", "/tmp/xdg-state");
    set_var("XDG_CACHE_HOME", "/tmp/xdg-cache");
    set_var("XDG_CONFIG_HOME", "/tmp/xdg-config");
    assert_eq!(state_file().unwrap(), state.join("state.json"));
    assert_eq!(
        audio_cache_dir().unwrap(),
        cache.join("audio").join("episodes")
    );
    assert_eq!(config_dir().unwrap(), config);

    // two instances scoped by MFP_STATE_DIR share neither state file nor cache
    set_var("MFP_STATE_DIR", "/tmp/instance-a/state");
    set_var("MFP_CACHE_DIR", "/tmp/instance-a/cache");
    let (state_a, audio_a) = (state_file().unwrap(), audio_cache_dir().unwrap());
    let check_a = update_check_file().unwrap();
    set_var("MFP_STATE_DIR", "/tmp/instance-b/state");
    set_var("MFP_CACHE_DIR", "/tmp/instance-b/cache");
    let (state_b, audio_b) = (state_file().unwrap(), audio_cache_dir().unwrap());
    let check_b = update_check_file().unwrap();

    assert_eq!(state_a, PathBuf::from("/tmp/instance-a/state/state.json"));
    assert_eq!(
        audio_a,
        PathBuf::from("/tmp/instance-a/cache/audio/episodes")
    );
    assert_eq!(
        check_a,
        PathBuf::from("/tmp/instance-a/cache/update-check.json")
    );
    assert_ne!(state_a, state_b);
    assert_ne!(audio_a, audio_b);
    assert_ne!(check_a, check_b);
}

fn set_var(key: &str, value: &str) {
    // SAFETY: this file is one test in a process of its own, so the write races no other
    // thread of this process reading the environment
    unsafe { std::env::set_var(key, value) };
}

/// Clears the named variables and restores what they held on drop.
struct TakenVars(Vec<(String, Option<String>)>);

impl TakenVars {
    fn take(keys: &[&str]) -> Self {
        let taken = keys
            .iter()
            .map(|key| ((*key).to_owned(), std::env::var(key).ok()))
            .collect();
        for key in keys {
            // SAFETY: as in set_var above
            unsafe { std::env::remove_var(key) };
        }
        Self(taken)
    }
}

impl Drop for TakenVars {
    fn drop(&mut self) {
        for (key, value) in &self.0 {
            match value {
                // SAFETY: as in set_var above
                Some(value) => unsafe { std::env::set_var(key, value) },
                // SAFETY: as in set_var above
                None => unsafe { std::env::remove_var(key) },
            }
        }
    }
}
