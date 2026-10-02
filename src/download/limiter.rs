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

use async_speed_limit::{Limiter, clock::StandardClock};

#[derive(Clone)]
pub(crate) struct BandwidthLimiter {
    inner: Limiter<StandardClock>,
}

impl BandwidthLimiter {
    pub(crate) fn new(bytes_per_sec: u64) -> Self {
        Self {
            #[allow(
                clippy::cast_precision_loss,
                reason = "bandwidth limits are configured by humans and fit easily in f64 precision"
            )]
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

    /// Refund a reservation cancelled before its chunk can be written.
    /// Upstream consumption reserves immediately and does not refund on drop.
    pub(crate) async fn consume_or_cancel(
        &self,
        n: usize,
        token: &tokio_util::sync::CancellationToken,
    ) -> bool {
        let consumed = self.inner.consume(n);
        tokio::select! {
            biased;
            () = token.cancelled() => {
                self.inner.unconsume(n);
                false
            }
            () = consumed => true,
        }
    }

    pub(crate) fn bytes_per_sec(&self) -> u64 {
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "speed_limit is a non-negative rate configured as u64 originally; round-tripping is lossless for realistic values"
        )]
        let v = self.inner.speed_limit() as u64;
        v
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
    #[tokio::test]
    async fn cancellation_refunds_only_its_reservation_and_preserves_other_waiters() {
        let limiter = BandwidthLimiter::new(1000);
        let keep_token = tokio_util::sync::CancellationToken::new();
        let cancelled_token = tokio_util::sync::CancellationToken::new();
        let kept = limiter.consume_or_cancel(1000, &keep_token);
        tokio::pin!(kept);
        assert!(futures_util::poll!(&mut kept).is_pending());
        let cancelled = limiter.consume_or_cancel(100_000, &cancelled_token);
        tokio::pin!(cancelled);
        assert!(futures_util::poll!(&mut cancelled).is_pending());
        assert_eq!(limiter.inner.total_bytes_consumed(), 101_000);
        cancelled_token.cancel();
        assert!(!cancelled.await);
        assert_eq!(limiter.inner.total_bytes_consumed(), 1000);
        // Refunding the cancelled reservation must not release another waiter early.
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut kept)
                .await
                .is_err()
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(2), kept)
                .await
                .unwrap()
        );
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(200),
                limiter.consume_or_cancel(1, &keep_token)
            )
            .await
            .unwrap()
        );
        assert_eq!(limiter.inner.total_bytes_consumed(), 1001);
        keep_token.cancel();
        assert_eq!(
            limiter.inner.total_bytes_consumed(),
            1001,
            "cancellation after acquisition must not refund committed budget"
        );
        assert!(!limiter.consume_or_cancel(10, &keep_token).await);
        assert_eq!(
            limiter.inner.total_bytes_consumed(),
            1001,
            "already cancelled reservations refund exactly once"
        );
    }
}
