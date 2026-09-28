//! Everything the daemon and the interface both need: one definition of an episode, of
//! every wire message, and of every filesystem path.

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

pub const USER_AGENT: &str = concat!(
    "mfp/",
    env!("CARGO_PKG_VERSION"),
    " (musicforprogramming terminal player)"
);
