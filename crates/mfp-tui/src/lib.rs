//! The terminal interface and the headless transport commands, in one binary.
//!
//! The interface holds no authoritative state: it renders what the daemon last reported and
//! sends commands back. Quitting it does not stop playback.

// Tests assert with `unwrap` and `expect` on purpose: a panic is the failure report, and
// threading `Result` obscures what the test checks
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::float_cmp
    )
)]

pub mod cli;
pub mod client;
pub mod ui;
pub mod update;
