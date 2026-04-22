use std::time::Duration;

use serde_json::Value;

use crate::retry::{self, parse_retry_after_header, RetryAction, RetryConfig};

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
            let resp_body = match resp.text().await {
                Ok(body) => body,
                Err(e) => {
                    tracing::debug!(
                        error = %e,
                        url = %url,
                        "CloudKit error-response body read failed; proceeding with empty body"
                    );
                    String::new()
                }
            };
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
        let client = self.read().await.http_client();
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
#[error("HTTP {status} for {url}")]
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

/// Shorten `body` to at most `MAX_PRESERVED_BODY` bytes without splitting
/// a multi-byte UTF-8 character.
fn truncate_body(body: &str) -> String {
    if body.len() <= MAX_PRESERVED_BODY {
        return body.to_string();
    }
    let mut end = MAX_PRESERVED_BODY;
    while end > 0 && !body.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &body[..end])
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
#[error("CloudKit server error: {code} — {reason}")]
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
    if let Some(code) = response["serverErrorCode"].as_str() {
        let reason = response["reason"]
            .as_str()
            .or_else(|| response["serverErrorMessage"].as_str())
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

    // Per-record errors: filter out errored records and keep valid ones.
    // Only return Err if ALL records are errored.
    if let Some(records) = response["records"].as_array() {
        // Check if any records have errors before taking the mutable path
        let has_errors = records
            .iter()
            .any(|r| r["serverErrorCode"].as_str().is_some());

        if has_errors {
            // Log each errored record and capture the last error
            let mut last_ck_err = None;
            let mut permanent_errors = 0usize;
            let mut retryable_errors = 0usize;
            for record in records {
                if let Some(code) = record["serverErrorCode"].as_str() {
                    let reason = record["reason"].as_str().unwrap_or("unknown");
                    let record_name = record["recordName"].as_str().unwrap_or("(unknown)");
                    let retryable = RETRYABLE_SERVER_ERRORS
                        .iter()
                        .any(|&s| s.eq_ignore_ascii_case(code));
                    let service_not_activated = is_service_not_activated(code, reason);
                    // Permanent errors (e.g. ACCESS_DENIED on a revoked
                    // shared asset) silently drop records from the
                    // enumeration; log at error! so operators can audit
                    // which assets were skipped and why. Retryable errors
                    // are a matter of the retry loop and stay at warn!.
                    if retryable {
                        retryable_errors += 1;
                        tracing::warn!(
                            record_name,
                            error_code = code,
                            retryable,
                            service_not_activated,
                            "CloudKit per-record error (retryable): {reason}"
                        );
                    } else {
                        permanent_errors += 1;
                        tracing::error!(
                            record_name,
                            error_code = code,
                            retryable,
                            service_not_activated,
                            "CloudKit per-record error (permanent, record skipped): {reason}"
                        );
                    }
                    last_ck_err = Some(CloudKitServerError {
                        code: code.into(),
                        reason: reason.into(),
                        retryable,
                        service_not_activated,
                    });
                }
            }

            // Now mutate in-place: retain only valid records
            let mut response = response;
            let total = response["records"].as_array().map_or(0, Vec::len);
            if let Some(records) = response["records"].as_array_mut() {
                records.retain(|r| r["serverErrorCode"].as_str().is_none());
                let valid_count = records.len();
                if valid_count == 0 {
                    // Control only reaches here because the loop above walked
                    // at least one record with `serverErrorCode` set, which
                    // always assigns `last_ck_err`. The expect encodes that
                    // invariant — it cannot fire at runtime.
                    #[allow(
                        clippy::expect_used,
                        reason = "invariant: valid_count==0 implies the errored loop ran and assigned last_ck_err"
                    )]
                    return Err(last_ck_err.expect("errored is non-empty").into());
                }
                tracing::warn!(
                    errored = total - valid_count,
                    permanent = permanent_errors,
                    retryable = retryable_errors,
                    valid = valid_count,
                    total,
                    "Filtered errored records from CloudKit response"
                );
            }
            return Ok(response);
        }
    }

    Ok(response)
}

/// Classify API errors for retry: network failures, server-side errors
/// (5xx, 429), and retryable `CloudKit` server errors are transient;
/// client errors (4xx) and non-retryable server errors are permanent.
fn classify_api_error(e: &anyhow::Error) -> RetryAction {
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
    retry::retry_with_backoff(retry_config, classify_api_error, || async {
        let response = session.post(url, body.to_owned(), headers).await?;
        check_cloudkit_errors(response)
    })
    .await
}

/// Errors from `changes/zone` when syncToken is invalid.
#[derive(Debug, thiserror::Error)]
pub enum SyncTokenError {
    /// Token is invalid/corrupted — fall back to full enumeration
    #[error("Invalid sync token: {reason}")]
    InvalidToken { reason: Box<str> },
    /// Zone no longer exists — stop syncing this zone
    #[error("Zone not found: {zone_name}")]
    ZoneNotFound { zone_name: Box<str> },
    /// Unexpected zone-level error (e.g. `RETRY_LATER`, THROTTLED) —
    /// treat as transient; do NOT advance the sync token.
    #[error("Unexpected zone error in {zone_name}: {error_code}")]
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
        // Verify that SharedSession can be used as a PhotosSession trait object
        let dir = std::env::temp_dir()
            .join("claude")
            .join("shared_session_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let session = crate::auth::session::Session::new(
            &dir,
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
        let _cloned = boxed.clone_box();

        // Verify clone_box produces a valid trait object
        let _cloned2 = _cloned.clone_box();
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
        // When some records are valid and some errored, valid ones are kept
        let response = serde_json::json!({
            "records": [
                {"recordName": "A"},
                {"serverErrorCode": "RETRY_LATER", "reason": "busy"}
            ]
        });
        let result = check_cloudkit_errors(response).unwrap();
        let records = result["records"].as_array().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["recordName"], "A");
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
        assert_eq!(err.to_string(), "Invalid sync token: bad token");
    }

    #[test]
    fn test_sync_token_error_display_zone_not_found() {
        let err = SyncTokenError::ZoneNotFound {
            zone_name: "SharedSync-ABC".into(),
        };
        assert_eq!(err.to_string(), "Zone not found: SharedSync-ABC");
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
            "Invalid sync token: expired"
        );
    }

    #[test]
    fn test_sync_token_error_display_empty_reason() {
        let err = SyncTokenError::InvalidToken { reason: "".into() };
        assert_eq!(err.to_string(), "Invalid sync token: ");
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
    }

    #[test]
    fn test_post_429_is_retryable() {
        let err = reqwest_status_error(429);
        assert_eq!(classify_api_error(&err), RetryAction::Retry);
    }

    #[test]
    fn test_post_421_aborts() {
        let err = reqwest_status_error(421);
        assert_eq!(classify_api_error(&err), RetryAction::Abort);
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
        // When a per-record error is non-retryable (e.g., ZONE_NOT_FOUND),
        // but there are still valid records, the valid ones should be kept.
        let response = serde_json::json!({
            "records": [
                {"recordName": "VALID_1"},
                {"serverErrorCode": "ZONE_NOT_FOUND", "reason": "not found"},
                {"recordName": "VALID_2"}
            ]
        });
        let result = check_cloudkit_errors(response).unwrap();
        let records = result["records"].as_array().unwrap();
        assert_eq!(records.len(), 2, "two valid records should be preserved");
        assert_eq!(records[0]["recordName"], "VALID_1");
        assert_eq!(records[1]["recordName"], "VALID_2");
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
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// The FIDO failure mode from issue #221: CloudKit returns 401 with
    /// `"no auth method found"` in the JSON body. A real reqwest client
    /// posting to a wiremock endpoint must surface that body on the
    /// resulting `HttpStatusError` so the library.rs mapping can log
    /// the security-key hint.
    #[tokio::test]
    async fn wiremock_401_preserves_body_in_http_status_error() {
        let server = MockServer::start().await;
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
        let server = MockServer::start().await;
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
        let server = MockServer::start().await;
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
        let server = MockServer::start().await;
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

    /// An oversized body is truncated with the `…` marker so a
    /// pathological CloudKit response (HTML error page, stack trace)
    /// can't blow up the error path.
    #[tokio::test]
    async fn wiremock_oversized_body_is_truncated() {
        let server = MockServer::start().await;
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
}
