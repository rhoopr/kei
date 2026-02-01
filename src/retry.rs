use std::future::Future;

use rand::Rng;

/// Retry decision returned by the error classifier callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryAction {
    Retry,
    Abort,
}

/// Exponential backoff configuration with jitter to prevent thundering herd
/// when multiple concurrent downloads hit the same transient failure.
#[derive(Debug, Clone, Copy)]
pub struct RetryConfig {
    pub max_retries: u32,
    pub base_delay_secs: u64,
    pub max_delay_secs: u64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 2,
            base_delay_secs: 5,
            max_delay_secs: 60,
        }
    }
}

impl RetryConfig {
    /// Compute the delay for a given retry attempt (0-indexed).
    ///
    /// Formula: `min(base_delay * 2^retry, max_delay) + random_jitter(0..base_delay)`
    pub fn delay_for_retry(&self, retry: u32) -> std::time::Duration {
        let exp_delay = self
            .base_delay_secs
            .saturating_mul(1u64.checked_shl(retry).unwrap_or(u64::MAX));
        let capped = exp_delay.min(self.max_delay_secs);
        let jitter = if self.base_delay_secs > 0 {
            rand::rng().random_range(0..self.base_delay_secs)
        } else {
            0
        };
        std::time::Duration::from_secs(capped + jitter)
    }
}

/// Retry an async operation with exponential backoff and jitter.
///
/// - `config`: retry configuration
/// - `classifier`: inspects an error and returns `Retry` or `Abort`
/// - `operation`: the async closure to retry
///
/// Returns the first `Ok` result, or the last error if retries are exhausted
/// or the classifier returns `Abort`.
pub async fn retry_with_backoff<F, Fut, T, E, C>(
    config: &RetryConfig,
    classifier: C,
    operation: F,
) -> Result<T, E>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<T, E>>,
    C: Fn(&E) -> RetryAction,
    E: std::fmt::Display,
{
    let total_attempts = config.max_retries + 1; // 1 initial + max_retries retries
    let mut last_err: Option<E> = None;

    for attempt in 0..total_attempts {
        match operation().await {
            Ok(val) => return Ok(val),
            Err(e) => {
                if classifier(&e) == RetryAction::Abort {
                    return Err(e);
                }
                let is_last = attempt + 1 >= total_attempts;
                if is_last {
                    last_err = Some(e);
                    break;
                }
                let delay = config.delay_for_retry(attempt);
                tracing::warn!(
                    "Retryable error (attempt {}/{}), retrying in {}s: {}",
                    attempt + 1,
                    total_attempts,
                    delay.as_secs(),
                    e
                );
                tokio::time::sleep(delay).await;
            }
        }
    }

    Err(last_err.expect("loop must have run at least once"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = RetryConfig::default();
        assert_eq!(config.max_retries, 2);
        assert_eq!(config.base_delay_secs, 5);
        assert_eq!(config.max_delay_secs, 60);
    }

    #[test]
    fn test_delay_exponential_backoff() {
        let config = RetryConfig {
            max_retries: 5,
            base_delay_secs: 2,
            max_delay_secs: 60,
        };
        // retry 0: base=2*1=2, jitter in 0..2, total in 2..4
        let d = config.delay_for_retry(0);
        assert!(d.as_secs() >= 2 && d.as_secs() < 4);

        // retry 1: base=2*2=4, jitter in 0..2, total in 4..6
        let d = config.delay_for_retry(1);
        assert!(d.as_secs() >= 4 && d.as_secs() < 6);

        // retry 2: base=2*4=8, jitter in 0..2, total in 8..10
        let d = config.delay_for_retry(2);
        assert!(d.as_secs() >= 8 && d.as_secs() < 10);
    }

    #[test]
    fn test_delay_capped_at_max() {
        let config = RetryConfig {
            max_retries: 10,
            base_delay_secs: 5,
            max_delay_secs: 30,
        };
        // retry 10: 5*1024 >> 30, so capped at 30 + jitter(0..5)
        let d = config.delay_for_retry(10);
        assert!(d.as_secs() >= 30 && d.as_secs() < 35);
    }

    #[test]
    fn test_delay_zero_base() {
        let config = RetryConfig {
            max_retries: 3,
            base_delay_secs: 0,
            max_delay_secs: 60,
        };
        let d = config.delay_for_retry(0);
        assert_eq!(d.as_secs(), 0);
    }

    #[tokio::test]
    async fn test_retry_succeeds_first_try() {
        let config = RetryConfig {
            max_retries: 3,
            base_delay_secs: 0,
            max_delay_secs: 0,
        };
        let result: Result<i32, String> =
            retry_with_backoff(&config, |_| RetryAction::Retry, || async { Ok(42) }).await;
        assert_eq!(result.unwrap(), 42);
    }

    #[tokio::test]
    async fn test_retry_abort_on_non_retryable() {
        let config = RetryConfig {
            max_retries: 3,
            base_delay_secs: 0,
            max_delay_secs: 0,
        };
        let call_count = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let cc = call_count.clone();
        let result: Result<i32, String> = retry_with_backoff(
            &config,
            |_| RetryAction::Abort,
            || {
                let cc = cc.clone();
                async move {
                    cc.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Err("fatal".to_string())
                }
            },
        )
        .await;
        assert_eq!(result.unwrap_err(), "fatal");
        assert_eq!(call_count.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_retry_succeeds_after_failures() {
        let config = RetryConfig {
            max_retries: 3,
            base_delay_secs: 0,
            max_delay_secs: 0,
        };
        let call_count = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let cc = call_count.clone();
        let result: Result<i32, String> = retry_with_backoff(
            &config,
            |_| RetryAction::Retry,
            || {
                let cc = cc.clone();
                async move {
                    let n = cc.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if n < 2 {
                        Err("transient".to_string())
                    } else {
                        Ok(99)
                    }
                }
            },
        )
        .await;
        assert_eq!(result.unwrap(), 99);
        assert_eq!(call_count.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn test_retry_exhausted() {
        let config = RetryConfig {
            max_retries: 2,
            base_delay_secs: 0,
            max_delay_secs: 0,
        };
        let call_count = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let cc = call_count.clone();
        let result: Result<i32, String> = retry_with_backoff(
            &config,
            |_| RetryAction::Retry,
            || {
                let cc = cc.clone();
                async move {
                    cc.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Err("still failing".to_string())
                }
            },
        )
        .await;
        assert_eq!(result.unwrap_err(), "still failing");
        // 1 initial + 2 retries = 3 attempts
        assert_eq!(call_count.load(std::sync::atomic::Ordering::SeqCst), 3);
    }
}
