//! The background player daemon.
//!
//! The daemon is the sole authority over playback and download state. It outlives every
//! client - closing a pane or quitting the interface must not interrupt audio - and
//! exits only on an explicit `shutdown` or a configured idle timeout.
//!
//! `rodio` blocks and wants a thread of its own while the socket server and the downloads
//! want `tokio`, so [`audio`] runs on one dedicated OS thread that never awaits and
//! everything else on the runtime, never blocking on audio. The two meet at
//! [`state::SharedState`].

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

pub mod audio;
pub mod download;
pub mod ipc;
pub mod state;
