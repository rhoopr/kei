//! Global bandwidth throttle for download streams.
//!
//! A single shared token bucket caps total byte throughput across every
//! concurrent download. Wraps `async_speed_limit::Limiter` so the rest of
//! the crate can hold a typed newtype rather than depending on that crate
//! at call sites.
//!
//! Build one [`BandwidthLimiter`] per sync, share it by value (it's `Clone`
//! and the underlying bucket is already shared), and call
//! [`BandwidthLimiter::consume`] before writing each received chunk. When no
//! limit is configured, downloads hold `None` and skip the call entirely.

use async_speed_limit::{clock::StandardClock, Limiter};

#[derive(Clone)]
pub(crate) struct BandwidthLimiter {
    inner: Limiter<StandardClock>,
}

impl BandwidthLimiter {
    pub(crate) fn new(bytes_per_sec: u64) -> Self {
        Self {
            inner: <Limiter>::builder(bytes_per_sec as f64).build(),
        }
    }

    /// Block the caller until `n` bytes of budget are available.
    ///
    /// The underlying limiter handles oversized requests correctly: a chunk
    /// larger than one bucket refill simply waits longer.
    pub(crate) async fn consume(&self, n: usize) {
        self.inner.consume(n).await;
    }

    pub(crate) fn bytes_per_sec(&self) -> u64 {
        self.inner.speed_limit() as u64
    }
}

impl std::fmt::Debug for BandwidthLimiter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BandwidthLimiter")
            .field("bytes_per_sec", &self.bytes_per_sec())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[tokio::test]
    async fn consume_under_limit_is_fast() {
        let limiter = BandwidthLimiter::new(10_000_000);
        let start = Instant::now();
        limiter.consume(1_000).await;
        assert!(
            start.elapsed().as_millis() < 100,
            "small consume under a generous limit should not block"
        );
    }

    #[tokio::test]
    async fn consume_enforces_rate() {
        let limit = 64 * 1024;
        let limiter = BandwidthLimiter::new(limit);
        let start = Instant::now();
        let total = 64 * 1024;
        let chunk = 8 * 1024;
        let mut remaining = total;
        while remaining > 0 {
            let take = remaining.min(chunk);
            limiter.consume(take).await;
            remaining -= take;
        }
        let elapsed = start.elapsed().as_secs_f64();
        let expected = total as f64 / limit as f64;
        assert!(
            elapsed >= expected * 0.6,
            "elapsed {elapsed:.2}s should be close to expected {expected:.2}s"
        );
    }

    #[test]
    fn bytes_per_sec_reports_configured_limit() {
        let limiter = BandwidthLimiter::new(500_000);
        assert_eq!(limiter.bytes_per_sec(), 500_000);
    }
}
