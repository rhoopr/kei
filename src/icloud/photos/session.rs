use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::Value;

use super::capture::ResponseCapture;
use crate::retry::{self, RetryAction, RetryConfig, parse_retry_after_header};

/// Upper bound on any `Retry-After` hint from CloudKit, chosen so a
/// pathological server value can't stall the retry loop.
const RETRY_AFTER_MAX: Duration = Duration::from_secs(120);

/// Async HTTP session trait for the photos service.
///
/// Abstracted as a trait so album/library code can be tested with stubs
/// without hitting the real iCloud API.
#[async_trait::async_trait]
pub trait PhotosSession: Send + Sync {
    async fn post(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<Value>;

    /// Clone this session into a new boxed trait object.
    fn clone_box(&self) -> Box<dyn PhotosSession>;
}

// Blanket impl lets `reqwest::Client` (from auth) be used directly as a
// `PhotosSession` without an adapter, since Client is Arc-backed and cheap to clone.
#[async_trait::async_trait]
impl PhotosSession for reqwest::Client {
    async fn post(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        let mut builder = self.post(url).body(body);
        for &(k, v) in headers {
            builder = builder.header(k, v);
        }
        let resp = builder.send().await?;
        let status = resp.status();

        if status.is_client_error() || status.is_server_error() {
            let url = resp.url().to_string();
            let retry_after = parse_retry_after_header(resp.headers(), RETRY_AFTER_MAX);
            let resp_body = read_bounded_error_body(resp, &url).await;
            if !resp_body.is_empty() {
                // 421 bodies are the most diagnostic signal for distinguishing
                // ADP-class from session-class misdirected requests (e.g. the
                // "Missing X-APPLE-WEBAUTH-USER cookie" string from issue
                // #199). Surface at WARN so reporters don't need RUST_LOG=debug.
                if status.as_u16() == 421 {
                    tracing::warn!(
                        status = %status,
                        url = %url,
                        body = %resp_body,
                        "CloudKit 421 Misdirected Request response body"
                    );
                } else {
                    tracing::debug!(
                        status = %status,
                        url = %url,
                        body = %resp_body,
                        "CloudKit error response body"
                    );
                }
            }
            let preserved = if resp_body.is_empty() {
                None
            } else {
                Some(truncate_body(&resp_body))
            };
            return Err(HttpStatusError {
                status: status.as_u16(),
                url,
                retry_after,
                body: preserved,
            }
            .into());
        }

        let json: Value = resp.json().await?;
        Ok(json)
    }

    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

#[derive(Clone)]
pub(crate) struct CapturingPhotosSession {
    pub(crate) session: crate::auth::SharedSession,
    pub(crate) capture: Arc<ResponseCapture>,
}

#[async_trait::async_trait]
impl PhotosSession for CapturingPhotosSession {
    async fn post(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        self.capture.check()?;
        let client = self.session.write().await.photos_capture_client()?;
        captured_post(&client, url, body, headers, &self.capture).await
    }

    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

// A captured JSON decode failure must retry just like reqwest::Response::json,
// without putting provider-controlled bytes from serde's error into logs.
#[derive(Debug, thiserror::Error)]
#[error("Could not decode the captured iCloud Photos JSON response")]
struct CapturedJsonError;

async fn captured_post(
    client: &reqwest::Client,
    url: &str,
    body: String,
    headers: &[(&str, &str)],
    capture: &Arc<ResponseCapture>,
) -> anyhow::Result<Value> {
    use anyhow::Context;
    use reqwest::header::{CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE, TRANSFER_ENCODING};

    let mut url = reqwest::Url::parse(url).context("Invalid iCloud Photos request URL")?;
    let origin = url.origin();
    let mut method = reqwest::Method::POST;
    let mut redirects = 0;
    loop {
        // A fresh lease per hop keeps cancellation and finish admission correct.
        let mut request = capture.begin().await?;
        let mut builder = client.request(method.clone(), url.clone());
        if method == reqwest::Method::POST {
            builder = builder.body(body.clone());
        }
        for &(key, value) in headers {
            if method == reqwest::Method::GET
                && [
                    CONTENT_ENCODING,
                    CONTENT_LENGTH,
                    CONTENT_TYPE,
                    TRANSFER_ENCODING,
                ]
                .iter()
                .any(|header| header.as_str().eq_ignore_ascii_case(key))
            {
                continue;
            }
            builder = builder.header(key, value);
        }
        let resp = match builder.send().await {
            Ok(resp) => resp,
            Err(error) => {
                request.complete();
                return Err(error.without_url().into());
            }
        };
        let status = resp.status();
        if matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308)
            && let Some(location) = resp.headers().get(reqwest::header::LOCATION).cloned()
        {
            // Save even rejected redirects, without retaining their bodies in memory.
            request.read_body(resp, Some(0)).await?;
            anyhow::ensure!(
                redirects < 10,
                "iCloud Photos capture redirect limit exceeded"
            );
            let next = location
                .to_str()
                .ok()
                .and_then(|location| url.join(location).ok())
                .context("Invalid iCloud Photos capture redirect location")?;
            anyhow::ensure!(
                next.origin() == origin
                    && next.host_str().is_some()
                    && next.username().is_empty()
                    && next.password().is_none(),
                "Refusing iCloud Photos capture redirect outside the original origin or with credentials"
            );
            if matches!(status.as_u16(), 301..=303) {
                method = reqwest::Method::GET;
            }
            url = next;
            redirects += 1;
            continue;
        }
        let is_error = status.is_client_error() || status.is_server_error();
        let retry_after = parse_retry_after_header(resp.headers(), RETRY_AFTER_MAX);
        let bytes = request
            .read_body(resp, is_error.then_some(MAX_PRESERVED_BODY + 16))
            .await?;
        if is_error {
            let body = String::from_utf8_lossy(&bytes);
            return Err(HttpStatusError {
                status: status.as_u16(),
                url: "[captured iCloud Photos endpoint]".to_string(),
                retry_after,
                body: (!body.is_empty()).then(|| truncate_body(&body)),
            }
            .into());
        }
        return serde_json::from_slice(&bytes).map_err(|_decode_error| CapturedJsonError.into());
    }
}

// SharedSession delegates to the inner Session's http_client(). The read lock
// is held only long enough to clone the Arc-backed Client, then released before
// the actual HTTP call so other tasks can read concurrently.
#[async_trait::async_trait]
impl PhotosSession for crate::auth::SharedSession {
    async fn post(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        let client = self.read().await.http_client().clone();
        PhotosSession::post(&client, url, body, headers).await
    }

    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

/// HTTP error with structured status code for typed error handling.
/// Wraps non-success HTTP responses from CloudKit endpoints.
///
/// `retry_after` is populated from the `Retry-After` response header when
/// present, so callers can honor the server-provided delay on 429/503
/// instead of falling back to exponential backoff alone.
///
/// `body` carries a truncated copy of the response body so downstream
/// error mapping (e.g. detecting "no auth method found" in a CloudKit 401
/// that typically indicates FIDO/security keys on the account) can read
/// the payload without re-requesting. Truncated to keep memory bounded
/// when CloudKit occasionally returns large HTML error pages.
#[derive(Debug, thiserror::Error)]
#[error("Apple returned HTTP {status} for {url}")]
pub(crate) struct HttpStatusError {
    pub status: u16,
    pub url: String,
    pub retry_after: Option<Duration>,
    pub body: Option<String>,
}

/// Maximum number of bytes preserved from an HTTP error body. Apple's
/// CloudKit error JSON is typically a few hundred bytes; HTML error pages
/// and stack traces are occasionally much larger. Cap it so a degenerate
/// response can't bloat the error path.
const MAX_PRESERVED_BODY: usize = 1024;

/// Read a CloudKit error-response body, capping at `MAX_PRESERVED_BODY`
/// bytes plus a small UTF-8 grace margin. The full body can be megabytes
/// (rate-limit HTML pages, stack traces) but the caller only ever keeps
/// the first ~1 KiB for diagnostics, so streaming the first chunks is
/// safer than materialising a 10 MB String we're about to throw away.
async fn read_bounded_error_body(resp: reqwest::Response, url: &str) -> String {
    use futures_util::StreamExt;
    // A few extra bytes past the cap so `truncate_body` has room to land
    // on a char boundary without re-fetching.
    const CAP: usize = MAX_PRESERVED_BODY + 16;
    let mut buf: Vec<u8> = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(bytes) => {
                let remaining = CAP.saturating_sub(buf.len());
                if remaining == 0 {
                    break;
                }
                let take = bytes.len().min(remaining);
                #[allow(
                    clippy::indexing_slicing,
                    reason = "`take` is bounded above by `bytes.len()` via min()"
                )]
                buf.extend_from_slice(&bytes[..take]);
                if buf.len() >= CAP {
                    break;
                }
            }
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    url = %url,
                    "CloudKit error-response body read failed; proceeding with partial body"
                );
                break;
            }
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// Shorten `body` to at most `MAX_PRESERVED_BODY` bytes without splitting
/// a multi-byte UTF-8 character.
fn truncate_body(body: &str) -> String {
    if body.len() <= MAX_PRESERVED_BODY {
        return body.to_string();
    }
    format!("{}…", crate::truncate_str(body, MAX_PRESERVED_BODY))
}

/// `CloudKit` server error codes that indicate a transient condition.
/// These arrive as HTTP 200 with a `serverErrorCode` field in the JSON body.
const RETRYABLE_SERVER_ERRORS: &[&str] =
    &["RETRY_LATER", "TRY_AGAIN_LATER", "CAS_OP_LOCK", "THROTTLED"];

/// `CloudKit` server error codes that indicate the iCloud service is not
/// activated or accessible (e.g. ADP enabled, incomplete iCloud setup).
const SERVICE_NOT_ACTIVATED_ERRORS: &[&str] = &["ZONE_NOT_FOUND", "AUTHENTICATION_FAILED"];

/// Error type for `CloudKit` server errors embedded in the JSON response body.
/// These are distinct from HTTP-level errors and represent API-level failures.
#[derive(Debug, thiserror::Error)]
#[error("Apple CloudKit reported {code}: {reason}")]
pub struct CloudKitServerError {
    pub(crate) code: Box<str>,
    pub(crate) reason: Box<str>,
    pub(crate) retryable: bool,
    /// True when the error indicates the iCloud service is not activated
    /// (ADP enabled, incomplete setup, or private db access disabled).
    pub(crate) service_not_activated: bool,
}

/// Check whether an error code or reason indicates the iCloud service is not
/// activated (ADP enabled, incomplete setup, or private db access disabled).
fn is_service_not_activated(code: &str, reason: &str) -> bool {
    SERVICE_NOT_ACTIVATED_ERRORS
        .iter()
        .any(|&s| s.eq_ignore_ascii_case(code))
        || code.eq_ignore_ascii_case("ACCESS_DENIED")
        || reason
            .to_ascii_lowercase()
            .contains("private db access disabled")
}

/// Check a `CloudKit` JSON response for `serverErrorCode` or per-record errors.
/// Returns `Err` if a server error is found, `Ok(response)` otherwise.
fn check_cloudkit_errors(response: Value) -> anyhow::Result<Value> {
    // Top-level serverErrorCode (e.g. from CAS Op-Lock)
    if let Some(code) = response.get("serverErrorCode").and_then(Value::as_str) {
        let reason = response
            .get("reason")
            .and_then(Value::as_str)
            .or_else(|| response.get("serverErrorMessage").and_then(Value::as_str))
            .unwrap_or("unknown");
        let retryable = RETRYABLE_SERVER_ERRORS
            .iter()
            .any(|&s| s.eq_ignore_ascii_case(code));
        let service_not_activated = is_service_not_activated(code, reason);
        tracing::warn!(
            error_code = code,
            retryable,
            service_not_activated,
            "CloudKit server error: {reason}"
        );
        return Err(CloudKitServerError {
            code: code.into(),
            reason: reason.into(),
            retryable,
            service_not_activated,
        }
        .into());
    }

    // Per-record errors make the whole page unusable. Filtering errored
    // records out of a mixed page can make enumeration look complete while
    // silently dropping assets from the snapshot.
    if let Some(records) = response.get("records").and_then(Value::as_array) {
        let has_errors = records
            .iter()
            .any(|r| r.get("serverErrorCode").and_then(Value::as_str).is_some());

        if has_errors {
            let mut first_retryable = None;
            let mut first_permanent = None;
            for record in records {
                if let Some(code) = record.get("serverErrorCode").and_then(Value::as_str) {
                    let reason = record
                        .get("reason")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    let record_name = record
                        .get("recordName")
                        .and_then(Value::as_str)
                        .unwrap_or("(unknown)");
                    let retryable = RETRYABLE_SERVER_ERRORS
                        .iter()
                        .any(|&s| s.eq_ignore_ascii_case(code));
                    let service_not_activated = is_service_not_activated(code, reason);
                    if retryable {
                        tracing::warn!(
                            record_name,
                            error_code = code,
                            retryable,
                            service_not_activated,
                            "CloudKit per-record error (retryable): {reason}"
                        );
                        first_retryable.get_or_insert_with(|| CloudKitServerError {
                            code: code.into(),
                            reason: reason.into(),
                            retryable,
                            service_not_activated,
                        });
                    } else {
                        tracing::error!(
                            record_name,
                            error_code = code,
                            retryable,
                            service_not_activated,
                            "CloudKit per-record error (permanent): {reason}"
                        );
                        first_permanent.get_or_insert_with(|| CloudKitServerError {
                            code: code.into(),
                            reason: reason.into(),
                            retryable,
                            service_not_activated,
                        });
                    }
                }
            }

            if let Some(err) = first_retryable.or(first_permanent) {
                return Err(err.into());
            }
            anyhow::bail!(
                "CloudKit response contained per-record errors, \
                 but no per-record server error was available"
            );
        }
    }

    Ok(response)
}

/// Classify API errors for retry: network failures, server-side errors
/// (5xx, 429), and retryable `CloudKit` server errors are transient;
/// client errors (4xx) and non-retryable server errors are permanent.
fn classify_api_error(e: &anyhow::Error) -> RetryAction {
    if e.is::<CapturedJsonError>() {
        return RetryAction::Retry;
    }
    if let Some(ck_err) = e.downcast_ref::<CloudKitServerError>() {
        return if ck_err.retryable {
            RetryAction::Retry
        } else {
            RetryAction::Abort
        };
    }
    if let Some(http_err) = e.downcast_ref::<HttpStatusError>() {
        if http_err.status == 429 || http_err.status >= 500 {
            return match http_err.retry_after {
                Some(d) => RetryAction::RetryAfter(d),
                None => RetryAction::Retry,
            };
        }
        return RetryAction::Abort;
    }
    if let Some(reqwest_err) = e.downcast_ref::<reqwest::Error>() {
        if let Some(status) = reqwest_err.status() {
            if status.as_u16() == 429 || status.as_u16() >= 500 {
                return RetryAction::Retry;
            }
            return RetryAction::Abort;
        }
        return RetryAction::Retry;
    }
    RetryAction::Abort
}

fn classify_rate_limit_error(e: &anyhow::Error) -> bool {
    if let Some(http_err) = e.downcast_ref::<HttpStatusError>() {
        return matches!(http_err.status, 429 | 503);
    }
    e.downcast_ref::<reqwest::Error>()
        .and_then(reqwest::Error::status)
        .is_some_and(|status| matches!(status.as_u16(), 429 | 503))
}

#[derive(Debug)]
pub(crate) struct RetriedPostResponse {
    pub(crate) response: Value,
    pub(crate) rate_limit_observations: usize,
}

/// Retry a `session.post()` call with default exponential backoff.
///
/// Inspects each response for `CloudKit` server errors (`serverErrorCode`)
/// and converts retryable ones (e.g. `TRY_AGAIN_LATER`, `CAS_OP_LOCK`)
/// into transient errors that trigger automatic retry.
pub async fn retry_post(
    session: &dyn PhotosSession,
    url: &str,
    body: &str,
    headers: &[(&str, &str)],
    retry_config: &RetryConfig,
) -> anyhow::Result<Value> {
    // CloudKit API retries (album listing, query, sync-token paging) typically
    // run before or alongside a download bar but resolve in well under a
    // second; friendly retry-pause narration would flicker on every transient
    // 503 / CAS_OP_LOCK and add noise without adding signal. Calling the
    // no-mode variant pins to Off; download retries (where the user is actively
    // watching) get the friendly framing via `retry_with_backoff_with_mode`.
    retry::retry_with_backoff(retry_config, classify_api_error, || async {
        let response = session.post(url, body.to_owned(), headers).await?;
        check_cloudkit_errors(response)
    })
    .await
}

/// Retry transport and HTTP failures while leaving record-level CloudKit
/// errors in the response for a batch-aware caller to classify. This is used
/// by `/records/lookup`, where one explicit `UNKNOWN_ITEM` is a successful
/// deletion result and must not fail unrelated records in the same batch.
pub(crate) async fn retry_post_allowing_record_errors(
    session: &dyn PhotosSession,
    url: &str,
    body: &str,
    headers: &[(&str, &str)],
    retry_config: &RetryConfig,
) -> anyhow::Result<RetriedPostResponse> {
    let rate_limit_observations = AtomicUsize::new(0);
    let response = retry::retry_with_backoff(
        retry_config,
        |error| {
            if classify_rate_limit_error(error) {
                rate_limit_observations.fetch_add(1, Ordering::Relaxed);
            }
            classify_api_error(error)
        },
        || async { session.post(url, body.to_owned(), headers).await },
    )
    .await?;
    Ok(RetriedPostResponse {
        response,
        rate_limit_observations: rate_limit_observations.load(Ordering::Relaxed),
    })
}

/// Errors from `changes/zone` when syncToken is invalid.
#[derive(Debug, thiserror::Error)]
pub enum SyncTokenError {
    /// Token is invalid/corrupted — fall back to full enumeration
    #[error("The saved iCloud sync token is no longer valid: {reason}")]
    InvalidToken { reason: Box<str> },
    /// Zone no longer exists — stop syncing this zone
    #[error("Apple no longer reports iCloud Photos zone {zone_name}")]
    ZoneNotFound { zone_name: Box<str> },
    /// Unexpected zone-level error (e.g. `RETRY_LATER`, THROTTLED) —
    /// treat as transient; do NOT advance the sync token.
    #[error("Apple returned an unexpected iCloud Photos zone error for {zone_name}: {error_code}")]
    UnexpectedZoneError {
        zone_name: Box<str>,
        error_code: Box<str>,
    },
}

impl SyncTokenError {
    /// Whether this error should trigger a fallback from incremental to full sync.
    /// Only token/zone-level issues warrant full re-enumeration; transient errors
    /// (THROTTLED, `RETRY_LATER`) should propagate without triggering an expensive fallback.
    pub fn should_fallback_to_full(&self) -> bool {
        matches!(self, Self::InvalidToken { .. } | Self::ZoneNotFound { .. })
    }
}

/// Check if a `ChangesZoneResult` contains a zone-level error.
/// Returns `Ok(())` if no error, `Err(SyncTokenError)` if there is one.
pub fn check_changes_zone_error(
    server_error_code: Option<&str>,
    reason: Option<&str>,
    zone_name: &str,
) -> Result<(), SyncTokenError> {
    match server_error_code {
        Some("BAD_REQUEST") => Err(SyncTokenError::InvalidToken {
            reason: reason.unwrap_or("Unknown sync continuation type").into(),
        }),
        Some("ZONE_NOT_FOUND") => Err(SyncTokenError::ZoneNotFound {
            zone_name: zone_name.into(),
        }),
        Some(code) => Err(SyncTokenError::UnexpectedZoneError {
            zone_name: zone_name.into(),
            error_code: code.into(),
        }),
        None => Ok(()),
    }
}

#[cfg(unix)]
#[cfg(test)]
mod wiremock_tests {
    use std::sync::Arc;

    use wiremock::matchers::method;
    use wiremock::{Mock, ResponseTemplate};

    use super::{CapturingPhotosSession, PhotosSession};
    use crate::icloud::photos::capture::ResponseCapture;

    #[tokio::test]
    async fn capture_http_response_body() {
        let server = crate::start_wiremock_or_skip!();
        let raw = b"{}";
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(raw.to_vec()))
            .mount(&server)
            .await;
        let data = tempfile::tempdir().unwrap();
        let auth = crate::auth::session::Session::new(
            data.path(),
            "capture@example.test",
            "https://example.test",
            None,
        )
        .await
        .unwrap();
        let session = CapturingPhotosSession {
            session: Arc::new(tokio::sync::RwLock::new(auth)),
            capture: ResponseCapture::new(data.path()).await.unwrap(),
        };
        session.post(&server.uri(), "{}".into(), &[]).await.unwrap();
        session.capture.finish().await.unwrap();
        let run = std::fs::read_dir(data.path().join(".diagnostics"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let mut files = std::fs::read_dir(run).unwrap();
        let body = files.next().unwrap().unwrap().path();
        assert_eq!(body.extension().unwrap(), "body");
        assert_eq!(std::fs::read(body).unwrap(), raw);
        assert!(files.next().is_none());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify_non_reqwest_error_aborts() {
        let e: anyhow::Error = anyhow::anyhow!("some other error");
        assert_eq!(classify_api_error(&e), RetryAction::Abort);
    }

    #[tokio::test]
    async fn test_shared_session_implements_photos_session() {
        let dir = tempfile::tempdir().unwrap();
        let session = crate::auth::session::Session::new(
            dir.path(),
            "test@shared.com",
            "https://example.com",
            None,
        )
        .await
        .unwrap();
        let shared: crate::auth::SharedSession =
            std::sync::Arc::new(tokio::sync::RwLock::new(session));

        // Verify it can be boxed as a PhotosSession
        let boxed: Box<dyn PhotosSession> = Box::new(shared.clone());
        assert_eq!(
            std::sync::Arc::strong_count(&shared),
            2,
            "boxing SharedSession as PhotosSession must retain the same shared session"
        );
        let _cloned = boxed.clone_box();
        assert_eq!(
            std::sync::Arc::strong_count(&shared),
            3,
            "clone_box must clone the underlying SharedSession"
        );

        // Verify clone_box produces a valid trait object
        let _cloned2 = _cloned.clone_box();
        assert_eq!(
            std::sync::Arc::strong_count(&shared),
            4,
            "cloned trait object must remain cloneable"
        );
    }

    #[test]
    fn test_check_cloudkit_errors_pass_through_normal() {
        let response = serde_json::json!({"records": [{"recordName": "A"}]});
        let result = check_cloudkit_errors(response.clone());
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), response);
    }

    #[test]
    fn test_check_cloudkit_errors_top_level_retryable() {
        let response = serde_json::json!({
            "serverErrorCode": "TRY_AGAIN_LATER",
            "reason": "Sync zone CAS Op-Lock failed"
        });
        let err = check_cloudkit_errors(response).unwrap_err();
        let ck_err = err.downcast_ref::<CloudKitServerError>().unwrap();
        assert_eq!(&*ck_err.code, "TRY_AGAIN_LATER");
        assert!(ck_err.retryable);
        assert!(!ck_err.service_not_activated);
        assert_eq!(classify_api_error(&err), RetryAction::Retry);
    }

    #[test]
    fn test_check_cloudkit_errors_top_level_non_retryable() {
        let response = serde_json::json!({
            "serverErrorCode": "ZONE_NOT_FOUND",
            "reason": "Zone not found"
        });
        let err = check_cloudkit_errors(response).unwrap_err();
        let ck_err = err.downcast_ref::<CloudKitServerError>().unwrap();
        assert!(!ck_err.retryable);
        assert!(ck_err.service_not_activated);
        assert_eq!(classify_api_error(&err), RetryAction::Abort);
    }

    #[test]
    fn test_check_cloudkit_errors_per_record_mixed() {
        let response = serde_json::json!({
            "records": [
                {"recordName": "A"},
                {"serverErrorCode": "RETRY_LATER", "reason": "busy"}
            ]
        });
        let err = check_cloudkit_errors(response).unwrap_err();
        let ck_err = err.downcast_ref::<CloudKitServerError>().unwrap();
        assert_eq!(&*ck_err.code, "RETRY_LATER");
        assert!(ck_err.retryable);
        assert_eq!(classify_api_error(&err), RetryAction::Retry);
    }

    #[test]
    fn test_check_cloudkit_errors_per_record_all_errored() {
        // When ALL records are errored, return Err
        let response = serde_json::json!({
            "records": [
                {"serverErrorCode": "RETRY_LATER", "reason": "busy"},
                {"serverErrorCode": "RETRY_LATER", "reason": "still busy"}
            ]
        });
        let err = check_cloudkit_errors(response).unwrap_err();
        let ck_err = err.downcast_ref::<CloudKitServerError>().unwrap();
        assert_eq!(&*ck_err.code, "RETRY_LATER");
        assert!(ck_err.retryable);
    }

    #[test]
    fn test_check_cloudkit_errors_cas_op_lock() {
        let response = serde_json::json!({
            "serverErrorCode": "CAS_OP_LOCK",
            "reason": "concurrent write rejected"
        });
        let err = check_cloudkit_errors(response).unwrap_err();
        let ck_err = err.downcast_ref::<CloudKitServerError>().unwrap();
        assert!(ck_err.retryable);
        assert!(!ck_err.service_not_activated);
    }

    #[test]
    fn test_check_cloudkit_errors_throttled() {
        let response = serde_json::json!({
            "serverErrorCode": "THROTTLED",
            "reason": "rate limited"
        });
        let err = check_cloudkit_errors(response).unwrap_err();
        let ck_err = err.downcast_ref::<CloudKitServerError>().unwrap();
        assert!(ck_err.retryable);
        assert!(!ck_err.service_not_activated);
    }

    #[test]
    fn test_check_cloudkit_errors_zone_not_found_is_service_not_activated() {
        let response = serde_json::json!({
            "serverErrorCode": "ZONE_NOT_FOUND",
            "reason": "CKError: Zone not found"
        });
        let err = check_cloudkit_errors(response).unwrap_err();
        let ck_err = err.downcast_ref::<CloudKitServerError>().unwrap();
        assert!(!ck_err.retryable);
        assert!(ck_err.service_not_activated);
    }

    #[test]
    fn test_check_cloudkit_errors_authentication_failed_is_service_not_activated() {
        let response = serde_json::json!({
            "serverErrorCode": "AUTHENTICATION_FAILED",
            "reason": "Authentication failed"
        });
        let err = check_cloudkit_errors(response).unwrap_err();
        let ck_err = err.downcast_ref::<CloudKitServerError>().unwrap();
        assert!(!ck_err.retryable);
        assert!(ck_err.service_not_activated);
    }

    #[test]
    fn test_check_cloudkit_errors_access_denied_is_service_not_activated() {
        let response = serde_json::json!({
            "serverErrorCode": "ACCESS_DENIED",
            "reason": "private db access disabled for this account"
        });
        let err = check_cloudkit_errors(response).unwrap_err();
        let ck_err = err.downcast_ref::<CloudKitServerError>().unwrap();
        assert!(!ck_err.retryable);
        assert!(ck_err.service_not_activated);
    }

    #[test]
    fn test_check_cloudkit_errors_private_db_disabled_by_reason() {
        // Even with an unknown error code, "private db access disabled" in the
        // reason should trigger service_not_activated detection.
        let response = serde_json::json!({
            "serverErrorCode": "UNKNOWN_CODE",
            "reason": "private db access disabled for this account"
        });
        let err = check_cloudkit_errors(response).unwrap_err();
        let ck_err = err.downcast_ref::<CloudKitServerError>().unwrap();
        assert!(ck_err.service_not_activated);
    }

    #[test]
    fn test_classify_network_error_retries() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = rt
            .block_on(reqwest::Client::new().get("http://127.0.0.1:1").send())
            .unwrap_err();
        let e: anyhow::Error = err.into();
        assert_eq!(classify_api_error(&e), RetryAction::Retry);
    }

    #[test]
    fn test_classify_retryable_cloudkit_error() {
        let err: anyhow::Error = CloudKitServerError {
            code: "RETRY_LATER".into(),
            reason: "busy".into(),
            retryable: true,
            service_not_activated: false,
        }
        .into();
        assert_eq!(classify_api_error(&err), RetryAction::Retry);
    }

    #[test]
    fn test_classify_non_retryable_cloudkit_error() {
        let err: anyhow::Error = CloudKitServerError {
            code: "ZONE_NOT_FOUND".into(),
            reason: "missing".into(),
            retryable: false,
            service_not_activated: true,
        }
        .into();
        assert_eq!(classify_api_error(&err), RetryAction::Abort);
    }

    #[test]
    fn test_is_service_not_activated_normal_error() {
        assert!(!is_service_not_activated("RETRY_LATER", "busy"));
    }

    #[test]
    fn test_check_cloudkit_errors_server_error_message_fallback() {
        let response = serde_json::json!({
            "serverErrorCode": "SOME_ERROR",
            "serverErrorMessage": "fallback message"
        });
        let err = check_cloudkit_errors(response).unwrap_err();
        let ck_err = err.downcast_ref::<CloudKitServerError>().unwrap();
        assert_eq!(&*ck_err.reason, "fallback message");
    }

    #[test]
    fn test_check_cloudkit_errors_no_reason_defaults_to_unknown() {
        let response = serde_json::json!({
            "serverErrorCode": "SOME_ERROR"
        });
        let err = check_cloudkit_errors(response).unwrap_err();
        let ck_err = err.downcast_ref::<CloudKitServerError>().unwrap();
        assert_eq!(&*ck_err.reason, "unknown");
    }

    #[test]
    fn test_check_cloudkit_errors_empty_records_ok() {
        let response = serde_json::json!({"records": []});
        assert!(check_cloudkit_errors(response).is_ok());
    }

    #[test]
    fn test_cloudkit_server_error_display() {
        let err = CloudKitServerError {
            code: "TEST".into(),
            reason: "test reason".into(),
            retryable: false,
            service_not_activated: false,
        };
        let msg = err.to_string();
        assert!(msg.contains("TEST"));
        assert!(msg.contains("test reason"));
    }

    #[test]
    fn test_check_changes_zone_error_no_error() {
        let result = check_changes_zone_error(None, None, "PrimarySync");
        assert!(result.is_ok());
    }

    #[test]
    fn test_check_changes_zone_error_unknown_code_is_unexpected() {
        let result = check_changes_zone_error(Some("SOME_OTHER_CODE"), None, "PrimarySync");
        assert!(result.is_err());
        match result.unwrap_err() {
            SyncTokenError::UnexpectedZoneError {
                zone_name,
                error_code,
            } => {
                assert_eq!(&*zone_name, "PrimarySync");
                assert_eq!(&*error_code, "SOME_OTHER_CODE");
            }
            other => panic!("Expected UnexpectedZoneError, got {other:?}"),
        }
    }

    #[test]
    fn test_check_changes_zone_error_bad_request() {
        let result = check_changes_zone_error(
            Some("BAD_REQUEST"),
            Some("Unknown sync continuation type"),
            "PrimarySync",
        );
        assert!(result.is_err());
        match result.unwrap_err() {
            SyncTokenError::InvalidToken { reason } => {
                assert_eq!(&*reason, "Unknown sync continuation type");
            }
            other => panic!("Expected InvalidToken, got {other:?}"),
        }
    }

    #[test]
    fn test_check_changes_zone_error_bad_request_no_reason() {
        let result = check_changes_zone_error(Some("BAD_REQUEST"), None, "PrimarySync");
        match result.unwrap_err() {
            SyncTokenError::InvalidToken { reason } => {
                assert_eq!(&*reason, "Unknown sync continuation type");
            }
            other => panic!("Expected InvalidToken, got {other:?}"),
        }
    }

    #[test]
    fn test_check_changes_zone_error_zone_not_found() {
        let result = check_changes_zone_error(Some("ZONE_NOT_FOUND"), None, "SharedSync-123");
        assert!(result.is_err());
        match result.unwrap_err() {
            SyncTokenError::ZoneNotFound { zone_name } => {
                assert_eq!(&*zone_name, "SharedSync-123");
            }
            other => panic!("Expected ZoneNotFound, got {other:?}"),
        }
    }

    #[test]
    fn test_sync_token_error_display_invalid_token() {
        let err = SyncTokenError::InvalidToken {
            reason: "bad token".into(),
        };
        assert_eq!(
            err.to_string(),
            "The saved iCloud sync token is no longer valid: bad token"
        );
    }

    #[test]
    fn test_sync_token_error_display_zone_not_found() {
        let err = SyncTokenError::ZoneNotFound {
            zone_name: "SharedSync-ABC".into(),
        };
        assert_eq!(
            err.to_string(),
            "Apple no longer reports iCloud Photos zone SharedSync-ABC"
        );
    }

    #[test]
    fn test_sync_token_error_downcast_from_anyhow() {
        let err: anyhow::Error = SyncTokenError::InvalidToken {
            reason: "expired".into(),
        }
        .into();
        let downcasted = err.downcast_ref::<SyncTokenError>();
        assert!(downcasted.is_some());
        assert_eq!(
            downcasted.unwrap().to_string(),
            "The saved iCloud sync token is no longer valid: expired"
        );
    }

    #[test]
    fn test_sync_token_error_display_empty_reason() {
        let err = SyncTokenError::InvalidToken { reason: "".into() };
        assert_eq!(
            err.to_string(),
            "The saved iCloud sync token is no longer valid: "
        );
    }

    /// T-2: Mock session returns HTTP 503 on first call, 200 on second.
    /// `retry_post` should retry the call and return the successful response.
    #[tokio::test]
    async fn test_retry_post_retries_on_503() {
        struct RetrySession {
            call_count: std::sync::atomic::AtomicU32,
        }

        #[async_trait::async_trait]
        impl PhotosSession for RetrySession {
            async fn post(
                &self,
                _url: &str,
                _body: String,
                _headers: &[(&str, &str)],
            ) -> anyhow::Result<Value> {
                let n = self
                    .call_count
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if n == 0 {
                    // First call: simulate 503 via CloudKit serverErrorCode
                    Ok(serde_json::json!({
                        "serverErrorCode": "TRY_AGAIN_LATER",
                        "reason": "Service Unavailable"
                    }))
                } else {
                    // Second call: success
                    Ok(serde_json::json!({
                        "records": [{"recordName": "A1"}]
                    }))
                }
            }

            fn clone_box(&self) -> Box<dyn PhotosSession> {
                panic!("not needed for test")
            }
        }

        let session = RetrySession {
            call_count: std::sync::atomic::AtomicU32::new(0),
        };
        let config = RetryConfig {
            max_retries: 3,
            base_delay_secs: 0,
            max_delay_secs: 0,
        };

        let result = retry_post(&session, "https://example.com/api", "{}", &[], &config).await;
        assert!(
            result.is_ok(),
            "retry_post should succeed on second attempt"
        );

        let response = result.unwrap();
        let records = response["records"].as_array().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["recordName"], "A1");

        // Verify exactly 2 calls were made (1 retry + 1 success)
        assert_eq!(
            session.call_count.load(std::sync::atomic::Ordering::SeqCst),
            2
        );
    }

    /// Build a reqwest::Error with the given HTTP status code (no network needed).
    fn reqwest_status_error(status: u16) -> anyhow::Error {
        let http_resp = http::Response::builder()
            .status(status)
            .body(Vec::<u8>::new())
            .unwrap();
        let resp = reqwest::Response::from(http_resp);
        resp.error_for_status().unwrap_err().into()
    }

    #[test]
    fn test_post_503_is_retryable() {
        let err = reqwest_status_error(503);
        assert_eq!(classify_api_error(&err), RetryAction::Retry);
        assert!(classify_rate_limit_error(&err));
    }

    #[test]
    fn test_post_429_is_retryable() {
        let err = reqwest_status_error(429);
        assert_eq!(classify_api_error(&err), RetryAction::Retry);
        assert!(classify_rate_limit_error(&err));
    }

    #[test]
    fn test_post_421_aborts() {
        let err = reqwest_status_error(421);
        assert_eq!(classify_api_error(&err), RetryAction::Abort);
        assert!(!classify_rate_limit_error(&err));
    }

    // ── Gap: empty records array passes through cleanly ──────────────

    #[test]
    fn test_check_cloudkit_errors_empty_records_array() {
        let response = serde_json::json!({"records": []});
        let result = check_cloudkit_errors(response.clone());
        assert!(result.is_ok());
        let val = result.unwrap();
        assert_eq!(val["records"].as_array().unwrap().len(), 0);
    }

    // ── Gap: response without records key passes through ─────────────

    #[test]
    fn test_check_cloudkit_errors_no_records_key() {
        // Some CloudKit endpoints return data without a "records" key
        // (e.g., zones/list). Should pass through unmodified.
        let response = serde_json::json!({"zones": [{"zoneID": {"zoneName": "PrimarySync"}}]});
        let result = check_cloudkit_errors(response.clone());
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), response);
    }

    // ── Gap: per-record mixed with non-retryable error ───────────────

    #[test]
    fn test_check_cloudkit_errors_per_record_non_retryable_still_filters() {
        let response = serde_json::json!({
            "records": [
                {"recordName": "VALID_1"},
                {"serverErrorCode": "ZONE_NOT_FOUND", "reason": "not found"},
                {"recordName": "VALID_2"}
            ]
        });
        let err = check_cloudkit_errors(response).unwrap_err();
        let ck_err = err.downcast_ref::<CloudKitServerError>().unwrap();
        assert_eq!(&*ck_err.code, "ZONE_NOT_FOUND");
        assert!(!ck_err.retryable);
        assert_eq!(classify_api_error(&err), RetryAction::Abort);
    }

    // ── Gap: SyncTokenError::should_fallback_to_full classification ──

    #[test]
    fn test_sync_token_error_fallback_classification() {
        let invalid = SyncTokenError::InvalidToken {
            reason: "bad token".into(),
        };
        assert!(
            invalid.should_fallback_to_full(),
            "InvalidToken should trigger full fallback"
        );

        let zone_gone = SyncTokenError::ZoneNotFound {
            zone_name: "Primary".into(),
        };
        assert!(
            zone_gone.should_fallback_to_full(),
            "ZoneNotFound should trigger full fallback"
        );

        let transient = SyncTokenError::UnexpectedZoneError {
            zone_name: "Primary".into(),
            error_code: "RETRY_LATER".into(),
        };
        assert!(
            !transient.should_fallback_to_full(),
            "UnexpectedZoneError should NOT trigger full fallback"
        );
    }

    #[test]
    fn classify_http_status_error_retries_5xx() {
        let err = anyhow::Error::new(HttpStatusError {
            status: 503,
            url: "https://example.com".to_string(),
            retry_after: None,
            body: None,
        });
        assert!(matches!(classify_api_error(&err), RetryAction::Retry));
    }

    #[test]
    fn classify_http_status_error_retries_429() {
        let err = anyhow::Error::new(HttpStatusError {
            status: 429,
            url: "https://example.com".to_string(),
            retry_after: None,
            body: None,
        });
        assert!(matches!(classify_api_error(&err), RetryAction::Retry));
    }

    #[test]
    fn classify_http_status_error_honors_retry_after() {
        let err = anyhow::Error::new(HttpStatusError {
            status: 429,
            url: "https://example.com".to_string(),
            retry_after: Some(Duration::from_secs(7)),
            body: None,
        });
        match classify_api_error(&err) {
            RetryAction::RetryAfter(d) => assert_eq!(d, Duration::from_secs(7)),
            other => panic!("expected RetryAfter, got {other:?}"),
        }
    }

    #[test]
    fn classify_http_status_error_503_with_retry_after() {
        let err = anyhow::Error::new(HttpStatusError {
            status: 503,
            url: "https://example.com".to_string(),
            retry_after: Some(Duration::from_secs(2)),
            body: None,
        });
        match classify_api_error(&err) {
            RetryAction::RetryAfter(d) => assert_eq!(d, Duration::from_secs(2)),
            other => panic!("expected RetryAfter, got {other:?}"),
        }
    }

    #[test]
    fn classify_http_status_error_aborts_4xx() {
        let err = anyhow::Error::new(HttpStatusError {
            status: 401,
            url: "https://example.com".to_string(),
            retry_after: None,
            body: None,
        });
        assert!(matches!(classify_api_error(&err), RetryAction::Abort));
    }

    #[test]
    fn classify_http_status_error_aborts_403() {
        let err = anyhow::Error::new(HttpStatusError {
            status: 403,
            url: "https://example.com".to_string(),
            retry_after: None,
            body: None,
        });
        assert!(matches!(classify_api_error(&err), RetryAction::Abort));
    }

    #[test]
    fn truncate_body_leaves_short_bodies_unchanged() {
        let body = r#"{"serverErrorCode":"AUTHENTICATION_FAILED","reason":"no auth method found"}"#;
        assert_eq!(truncate_body(body), body);
    }

    #[test]
    fn truncate_body_clips_oversized_bodies() {
        let body = "x".repeat(MAX_PRESERVED_BODY * 2);
        let out = truncate_body(&body);
        assert!(
            out.len() <= MAX_PRESERVED_BODY + 4,
            "truncated body must stay under the cap plus the marker, got len {}",
            out.len()
        );
        assert!(
            out.ends_with('…'),
            "truncation marker must be appended to signal the clip"
        );
    }

    #[test]
    fn truncate_body_respects_utf8_boundaries() {
        // Multi-byte chars must not be split — pad with a 3-byte char so
        // the naive byte-slice would land in the middle.
        let mut body = "a".repeat(MAX_PRESERVED_BODY - 1);
        body.push('€'); // 3 bytes: 0xE2 0x82 0xAC
        body.push_str(&"b".repeat(MAX_PRESERVED_BODY));
        let out = truncate_body(&body);
        // Must be valid UTF-8 (the format! call would panic otherwise).
        assert!(out.is_char_boundary(out.len() - '…'.len_utf8()));
    }

    // ────────────────────────────────────────────────────────────────
    // wiremock-based tests: exercise the real reqwest path through
    // `PhotosSession::post` to prove that CloudKit error bodies
    // actually end up on `HttpStatusError.body`. The unit-level tests
    // above only cover `truncate_body`; these tests close the loop
    // between "server returned a body" and "caller can read it".
    // ────────────────────────────────────────────────────────────────

    use wiremock::matchers::method as wm_method;
    use wiremock::{Mock, ResponseTemplate};

    /// The FIDO failure mode from issue #221: CloudKit returns 401 with
    /// `"no auth method found"` in the JSON body. A real reqwest client
    /// posting to a wiremock endpoint must surface that body on the
    /// resulting `HttpStatusError` so the library.rs mapping can log
    /// the security-key hint.
    #[tokio::test]
    async fn wiremock_401_preserves_body_in_http_status_error() {
        if crate::test_helpers::skip_if_loopback_bind_blocked(
            "wiremock_401_preserves_body_in_http_status_error",
        ) {
            return;
        }
        let server = crate::start_wiremock_or_skip!();
        let body = r#"{"serverErrorCode":"AUTHENTICATION_FAILED","reason":"no auth method found"}"#;
        Mock::given(wm_method("POST"))
            .respond_with(ResponseTemplate::new(401).set_body_string(body))
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let err = PhotosSession::post(
            &client,
            &format!("{}/records/query", server.uri()),
            "{}".to_string(),
            &[],
        )
        .await
        .expect_err("401 must propagate as an error");
        let http_err = err
            .downcast_ref::<HttpStatusError>()
            .expect("expected HttpStatusError");
        assert_eq!(http_err.status, 401);
        let preserved = http_err
            .body
            .as_deref()
            .expect("401 body must be preserved for downstream FIDO/auth diagnostics");
        assert!(
            preserved.contains("no auth method found"),
            "preserved body must include the FIDO-indicating signal, got: {preserved}"
        );
    }

    /// A 503 body (transient error) is preserved too, so the retry
    /// path's warnings can include server-provided detail. Guards
    /// against a refactor that accidentally scopes body preservation
    /// to 401 only.
    #[tokio::test]
    async fn wiremock_5xx_preserves_body_in_http_status_error() {
        if crate::test_helpers::skip_if_loopback_bind_blocked(
            "wiremock_5xx_preserves_body_in_http_status_error",
        ) {
            return;
        }
        let server = crate::start_wiremock_or_skip!();
        Mock::given(wm_method("POST"))
            .respond_with(ResponseTemplate::new(503).set_body_string("service unavailable"))
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let err = PhotosSession::post(
            &client,
            &format!("{}/records/query", server.uri()),
            "{}".to_string(),
            &[],
        )
        .await
        .expect_err("503 must propagate as an error");
        let http_err = err.downcast_ref::<HttpStatusError>().unwrap();
        assert_eq!(http_err.status, 503);
        assert_eq!(http_err.body.as_deref(), Some("service unavailable"));
    }

    /// An empty error body yields `body: None`, not `Some("")`. Guards
    /// the downstream check `http_err.body.as_deref()` from having to
    /// special-case "" vs absent.
    #[tokio::test]
    async fn wiremock_empty_body_stays_none() {
        if crate::test_helpers::skip_if_loopback_bind_blocked("wiremock_empty_body_stays_none") {
            return;
        }
        let server = crate::start_wiremock_or_skip!();
        Mock::given(wm_method("POST"))
            .respond_with(ResponseTemplate::new(403)) // no body
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let err = PhotosSession::post(
            &client,
            &format!("{}/records/query", server.uri()),
            "{}".to_string(),
            &[],
        )
        .await
        .expect_err("403 must propagate");
        let http_err = err.downcast_ref::<HttpStatusError>().unwrap();
        assert_eq!(http_err.status, 403);
        assert!(
            http_err.body.is_none(),
            "empty response body must be None, not Some(\"\"), got: {:?}",
            http_err.body
        );
    }

    /// HTTP 200 with a truncated / malformed JSON body must surface as an
    /// error, not a silently-empty parse. A silent parse would let a
    /// pathological CloudKit page pretend to be a valid zero-record
    /// response and halt enumeration prematurely.
    #[tokio::test]
    async fn wiremock_200_with_truncated_json_returns_error() {
        if crate::test_helpers::skip_if_loopback_bind_blocked(
            "wiremock_200_with_truncated_json_returns_error",
        ) {
            return;
        }
        let server = crate::start_wiremock_or_skip!();
        Mock::given(wm_method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string("{\"records\": [{\"incomplete\""),
            )
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let result = PhotosSession::post(
            &client,
            &format!("{}/records/query", server.uri()),
            "{}".to_string(),
            &[],
        )
        .await;
        assert!(
            result.is_err(),
            "200 with malformed JSON body must be reported as an error, not a silent empty parse"
        );
    }

    /// `read_bounded_error_body` stops reading off the wire once it
    /// has `MAX_PRESERVED_BODY + 16` bytes, even if the server sent
    /// orders of magnitude more. A regression to `resp.text().await`
    /// would buffer the full body and fail this assertion. The
    /// existing `wiremock_oversized_body_is_truncated` test only
    /// proves the *output* is clipped (because `truncate_body` runs
    /// after the read), so it can't distinguish "streamed and stopped"
    /// from "buffered and trimmed".
    #[tokio::test]
    async fn read_bounded_error_body_caps_at_max_plus_grace() {
        if crate::test_helpers::skip_if_loopback_bind_blocked(
            "read_bounded_error_body_caps_at_max_plus_grace",
        ) {
            return;
        }
        let server = crate::start_wiremock_or_skip!();
        let huge = "x".repeat(64 * 1024);
        Mock::given(wm_method("POST"))
            .respond_with(ResponseTemplate::new(500).set_body_string(&huge))
            .mount(&server)
            .await;

        let resp = reqwest::Client::new()
            .post(format!("{}/records/query", server.uri()))
            .body("{}")
            .send()
            .await
            .expect("request");
        assert!(resp.status().is_server_error());

        let body = read_bounded_error_body(resp, "test").await;
        assert!(
            body.len() <= MAX_PRESERVED_BODY + 16,
            "streaming cap breached: body.len() = {}, cap = {}",
            body.len(),
            MAX_PRESERVED_BODY + 16
        );
    }

    /// An oversized body is truncated with the `…` marker so a
    /// pathological CloudKit response (HTML error page, stack trace)
    /// can't blow up the error path.
    #[tokio::test]
    async fn wiremock_oversized_body_is_truncated() {
        if crate::test_helpers::skip_if_loopback_bind_blocked(
            "wiremock_oversized_body_is_truncated",
        ) {
            return;
        }
        let server = crate::start_wiremock_or_skip!();
        let huge = "x".repeat(MAX_PRESERVED_BODY * 3);
        Mock::given(wm_method("POST"))
            .respond_with(ResponseTemplate::new(500).set_body_string(huge))
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let err = PhotosSession::post(
            &client,
            &format!("{}/records/query", server.uri()),
            "{}".to_string(),
            &[],
        )
        .await
        .expect_err("500 must propagate");
        let http_err = err.downcast_ref::<HttpStatusError>().unwrap();
        let preserved = http_err.body.as_deref().unwrap();
        assert!(
            preserved.ends_with('…'),
            "oversized body must be clipped with the truncation marker, got (len {}): {}",
            preserved.len(),
            preserved
        );
        assert!(
            preserved.len() <= MAX_PRESERVED_BODY + '…'.len_utf8(),
            "truncated body must stay under the cap plus the marker, got len {}",
            preserved.len()
        );
    }

    #[tokio::test]
    async fn retry_post_persistent_503_terminates_within_max_attempts() {
        struct Always503 {
            call_count: std::sync::atomic::AtomicU32,
        }

        #[async_trait::async_trait]
        impl PhotosSession for Always503 {
            async fn post(
                &self,
                _url: &str,
                _body: String,
                _headers: &[(&str, &str)],
            ) -> anyhow::Result<Value> {
                self.call_count
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(serde_json::json!({
                    "serverErrorCode": "TRY_AGAIN_LATER",
                    "reason": "Service Unavailable"
                }))
            }

            fn clone_box(&self) -> Box<dyn PhotosSession> {
                panic!("not needed for test")
            }
        }

        let session = Always503 {
            call_count: std::sync::atomic::AtomicU32::new(0),
        };
        let config = RetryConfig {
            max_retries: 4,
            base_delay_secs: 0,
            max_delay_secs: 0,
        };

        let result = retry_post(&session, "https://example.com/api", "{}", &[], &config).await;
        assert!(result.is_err(), "persistent 503 must eventually fail");

        let err = result.unwrap_err();
        let ck_err = err.downcast_ref::<CloudKitServerError>().unwrap();
        assert!(
            ck_err.retryable,
            "final error must still be tagged retryable"
        );

        let total_calls = session.call_count.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            total_calls, 5,
            "must be exactly max_retries + 1 = 5 attempts, got {total_calls}"
        );
    }
}
