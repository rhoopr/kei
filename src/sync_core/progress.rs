//! Periodic progress lines for the long steps of a cycle.
//!
//! A first listing or a large download backlog can take hours. Each step
//! reports at most one line per interval, so logs show steady progress
//! without one line per record.

use std::time::{Duration, Instant};

/// How often a long step reports. Tests report on every check.
const INTERVAL: Duration = if cfg!(test) {
    Duration::ZERO
} else {
    Duration::from_secs(30)
};

pub(crate) struct Progress {
    last: Instant,
}

impl Progress {
    pub(crate) fn start() -> Self {
        Self {
            last: Instant::now(),
        }
    }

    /// Whether a progress line is due; resets the interval when it is.
    pub(crate) fn due(&mut self) -> bool {
        if self.last.elapsed() < INTERVAL {
            return false;
        }
        self.last = Instant::now();
        true
    }
}
