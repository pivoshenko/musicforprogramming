pub mod server;

use std::time::Duration;

pub const EVENT_TICK: Duration = Duration::from_millis(250);

pub const SPECTRUM_PUSH_INTERVAL: Duration = Duration::from_millis(50);

pub const PUSH_POLL: Duration = Duration::from_millis(5);
