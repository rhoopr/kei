use std::future::Future;
use std::time::Duration;

use rand::RngExt;

/// Parse the `Retry-After` response header as delta-seconds, capped at `max`.
/// Returns `None` for absent, zero, or unparsable values. The HTTP-date form
/// is not accepted (Apple/CloudKit always emit delta-seconds).
pub fn parse_retry_after_header(
    headers: &reqwest::header::HeaderMap,
    max: Duration,
) -> Option<Duration> {
    let secs: u64 = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()?;
    if secs == 0 {
        return None;
    }
    Some(Duration::from_secs(secs).min(max))
}

/// Retry decision returned by the error classifier callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryAction {
    Retry,
    /// Retry after an explicit delay (e.g. honoring a `Retry-After` header).
    /// Overrides the exponential-backoff schedule for this attempt only.
    RetryAfter(std::time::Duration),
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
            max_retries: 3,
            base_delay_secs: 5,
            max_delay_secs: 60,
        }
    }
}

impl RetryConfig {
    /// Compute the delay for a given retry attempt (0-indexed).
    ///
    /// Formula: `min(base_delay * 2^retry, max_delay) + random_jitter(0..base_delay)`
    #[must_use]
    pub(crate) fn delay_for_retry(&self, retry: u32) -> std::time::Duration {
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
/// # Errors
///
/// Returns the last error if all retry attempts are exhausted or the
/// classifier returns `Abort` for a non-retryable error.
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
    let total_attempts = config.max_retries.saturating_add(1);

    for attempt in 0..total_attempts {
        match operation().await {
            Ok(val) => return Ok(val),
            Err(e) => {
                let action = classifier(&e);
                if action == RetryAction::Abort {
                    return Err(e);
                }
                let is_last = attempt + 1 >= total_attempts;
                if is_last {
                    return Err(e);
                }
                let delay = match action {
                    RetryAction::RetryAfter(d) => {
                        let max = std::time::Duration::from_secs(config.max_delay_secs);
                        d.min(max)
                    }
                    _ => config.delay_for_retry(attempt),
                };
                tracing::warn!(
                    attempt = attempt + 1,
                    total_attempts,
                    retry_delay_secs = delay.as_secs(),
                    error = %e,
                    "Retryable error, retrying"
                );
                tokio::time::sleep(delay).await;
            }
        }
    }

    // This is unreachable: the loop always runs at least once (total_attempts >= 1)
    // and either returns Ok, returns Err on abort, or returns Err on last attempt.
    unreachable!("retry loop must return before exhausting iterations")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = RetryConfig::default();
        assert_eq!(config.max_retries, 3);
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

    #[tracing_test::traced_test]
    #[tokio::test]
    async fn test_retry_logs_structured_fields_on_retryable_error() {
        let config = RetryConfig {
            max_retries: 2,
            base_delay_secs: 0,
            max_delay_secs: 0,
        };
        let call_count = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let cc = call_count.clone();
        let _result: Result<i32, String> = retry_with_backoff(
            &config,
            |_| RetryAction::Retry,
            || {
                let cc = cc.clone();
                async move {
                    let n = cc.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if n < 1 {
                        Err("transient failure".to_string())
                    } else {
                        Ok(42)
                    }
                }
            },
        )
        .await;

        assert!(logs_contain("Retryable error, retrying"));
        assert!(logs_contain("attempt=1"));
        assert!(logs_contain("total_attempts=3"));
        assert!(logs_contain("transient failure"));
    }

    #[tracing_test::traced_test]
    #[tokio::test]
    async fn test_retry_logs_each_attempt() {
        let config = RetryConfig {
            max_retries: 3,
            base_delay_secs: 0,
            max_delay_secs: 0,
        };
        let _result: Result<i32, String> = retry_with_backoff(
            &config,
            |_| RetryAction::Retry,
            || async { Err::<i32, _>("keep failing".to_string()) },
        )
        .await;

        // Should log attempts 1, 2, 3 (but not 4 — last attempt just returns the error)
        assert!(logs_contain("attempt=1"));
        assert!(logs_contain("attempt=2"));
        assert!(logs_contain("attempt=3"));
    }

    #[tracing_test::traced_test]
    #[tokio::test]
    async fn test_retry_no_log_on_success() {
        let config = RetryConfig {
            max_retries: 3,
            base_delay_secs: 0,
            max_delay_secs: 0,
        };
        let result: Result<i32, String> =
            retry_with_backoff(&config, |_| RetryAction::Retry, || async { Ok(42) }).await;
        assert_eq!(result.unwrap(), 42);
        assert!(!logs_contain("Retryable error"));
    }

    #[tracing_test::traced_test]
    #[tokio::test]
    async fn test_retry_no_log_on_abort() {
        let config = RetryConfig {
            max_retries: 3,
            base_delay_secs: 0,
            max_delay_secs: 0,
        };
        let _result: Result<i32, String> = retry_with_backoff(
            &config,
            |_| RetryAction::Abort,
            || async { Err::<i32, _>("fatal".to_string()) },
        )
        .await;
        // Abort returns immediately without logging retry
        assert!(!logs_contain("Retryable error"));
    }

    fn headers_with_retry_after(value: &str) -> reqwest::header::HeaderMap {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert(
            reqwest::header::RETRY_AFTER,
            reqwest::header::HeaderValue::from_str(value).unwrap(),
        );
        h
    }

    #[test]
    fn parse_retry_after_delta_seconds() {
        let h = headers_with_retry_after("5");
        assert_eq!(
            parse_retry_after_header(&h, Duration::from_secs(60)),
            Some(Duration::from_secs(5))
        );
        let h = headers_with_retry_after(" 12 ");
        assert_eq!(
            parse_retry_after_header(&h, Duration::from_secs(60)),
            Some(Duration::from_secs(12))
        );
    }

    #[test]
    fn parse_retry_after_zero_treated_as_absent() {
        let h = headers_with_retry_after("0");
        assert_eq!(parse_retry_after_header(&h, Duration::from_secs(60)), None);
    }

    #[test]
    fn parse_retry_after_caps_at_max() {
        let h = headers_with_retry_after("999999");
        assert_eq!(
            parse_retry_after_header(&h, Duration::from_secs(120)),
            Some(Duration::from_secs(120))
        );
    }

    #[test]
    fn parse_retry_after_rejects_http_date() {
        let h = headers_with_retry_after("Sun, 06 Nov 1994 08:49:37 GMT");
        assert_eq!(parse_retry_after_header(&h, Duration::from_secs(60)), None);
    }

    #[test]
    fn parse_retry_after_rejects_junk() {
        let h = headers_with_retry_after("not-a-number");
        assert_eq!(parse_retry_after_header(&h, Duration::from_secs(60)), None);
    }

    #[test]
    fn parse_retry_after_missing_header() {
        let h = reqwest::header::HeaderMap::new();
        assert_eq!(parse_retry_after_header(&h, Duration::from_secs(60)), None);
    }

    #[tokio::test]
    async fn test_retry_after_overrides_exponential_delay() {
        // RetryAction::RetryAfter(d) uses the server-provided delay instead
        // of the configured exponential backoff for that attempt.
        let config = RetryConfig {
            max_retries: 2,
            base_delay_secs: 5, // would normally sleep 5..10s on first retry
            max_delay_secs: 60,
        };
        let call_count = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let cc = call_count.clone();
        let started = std::time::Instant::now();
        let result: Result<i32, String> = retry_with_backoff(
            &config,
            |_| RetryAction::RetryAfter(std::time::Duration::from_millis(50)),
            || {
                let cc = cc.clone();
                async move {
                    let n = cc.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if n < 1 {
                        Err("transient".to_string())
                    } else {
                        Ok(7)
                    }
                }
            },
        )
        .await;
        let elapsed = started.elapsed();
        assert_eq!(result.unwrap(), 7);
        // Server-provided 50ms should dominate over the configured 5s.
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "expected RetryAfter to shorten delay, took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn test_retry_after_capped_at_max_delay() {
        // A Retry-After larger than max_delay_secs is clamped so a pathological
        // server response cannot stall the retry loop indefinitely.
        let config = RetryConfig {
            max_retries: 1,
            base_delay_secs: 0,
            max_delay_secs: 0, // forces the clamp to zero
        };
        let call_count = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let cc = call_count.clone();
        let started = std::time::Instant::now();
        let result: Result<i32, String> = retry_with_backoff(
            &config,
            |_| RetryAction::RetryAfter(std::time::Duration::from_secs(3600)),
            || {
                let cc = cc.clone();
                async move {
                    let n = cc.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if n < 1 {
                        Err("rate limited".to_string())
                    } else {
                        Ok(1)
                    }
                }
            },
        )
        .await;
        let elapsed = started.elapsed();
        assert_eq!(result.unwrap(), 1);
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "expected max_delay_secs clamp, took {elapsed:?}"
        );
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
