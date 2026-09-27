//! The Unix domain socket server and the daemon's lifecycle.
//!
//! No network transport, and the socket path itself is the single-instance lock: with no
//! separate PID file, the lock and the endpoint can never disagree.

pub mod server;

use std::time::Duration;

/// How long a subscribed connection may go without a snapshot.
///
/// Level-triggered rather than edge-triggered per change, so socket traffic is bounded
/// however chatty playback becomes, and it ticks while nothing plays. Also the daemon's own
/// persistence tick.
pub const EVENT_TICK: Duration = Duration::from_millis(250);

/// The shortest gap between two snapshots pushed because the spectrum moved.
///
/// Twenty a second is enough for an analyser to animate and far short of one per audio
/// buffer. Gated once for the whole daemon rather than per connection, so the rate is
/// independent of how many clients are attached.
pub const SPECTRUM_PUSH_INTERVAL: Duration = Duration::from_millis(50);

/// How often the shared state is examined for something worth pushing.
///
/// The audio thread and the download tasks write it without notifying anyone, so pushing is
/// decided by polling. Deliberately far shorter than [`SPECTRUM_PUSH_INTERVAL`]: a pause is
/// pushed on the first poll that sees it, never held to the spectrum's gate.
pub const PUSH_POLL: Duration = Duration::from_millis(5);
