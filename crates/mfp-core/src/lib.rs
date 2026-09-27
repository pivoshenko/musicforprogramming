//! Everything the daemon and the interface both need: one definition of an episode, of every
//! wire message, and of every filesystem path. No I/O policy of its own beyond retrieving
//! the catalog; the daemon is the sole writer of playback and download state, and clients
//! render what it last reported.

// Test code asserts with `unwrap` and `expect` on purpose: a panic there is the failure
// report, and threading `Result` through a test obscures what it is checking
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::float_cmp
    )
)]

pub mod catalog;
pub mod config;
pub mod error;
pub mod model;
pub mod paths;
pub mod protocol;

pub use config::Config;
pub use error::{Error, ErrorCode, Result};
pub use model::{Catalog, Episode};

/// The User-Agent every request to musicforprogramming.net carries. Upstream is a small
/// independent host, so requests identify themselves.
pub const USER_AGENT: &str = concat!(
    "mfp/",
    env!("CARGO_PKG_VERSION"),
    " (musicforprogramming terminal player)"
);
