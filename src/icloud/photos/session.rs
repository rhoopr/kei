use serde_json::Value;

use crate::retry::{self, RetryAction, RetryConfig};

/// Async HTTP session trait for the photos service.
///
/// Abstracted as a trait so album/library code can be tested with stubs
/// without hitting the real iCloud API.
#[async_trait::async_trait]
#[allow(dead_code)] // get() not called yet; part of public session API for future use
pub trait PhotosSession: Send + Sync {
    async fn post(&self, url: &str, body: &str, headers: &[(&str, &str)]) -> anyhow::Result<Value>;

    async fn get(&self, url: &str, headers: &[(&str, &str)]) -> anyhow::Result<reqwest::Response>;

    /// Clone this session into a new boxed trait object.
    fn clone_box(&self) -> Box<dyn PhotosSession>;
}

// Blanket impl lets `reqwest::Client` (from auth) be used directly as a
// `PhotosSession` without an adapter, since Client is Arc-backed and cheap to clone.
#[async_trait::async_trait]
impl PhotosSession for reqwest::Client {
    async fn post(&self, url: &str, body: &str, headers: &[(&str, &str)]) -> anyhow::Result<Value> {
        let mut builder = self.post(url).body(body.to_owned());
        for &(k, v) in headers {
            builder = builder.header(k, v);
        }
        let resp = builder.send().await?;
        let json: Value = resp.json().await?;
        Ok(json)
    }

    async fn get(&self, url: &str, headers: &[(&str, &str)]) -> anyhow::Result<reqwest::Response> {
        let mut builder = reqwest::Client::get(self, url);
        for &(k, v) in headers {
            builder = builder.header(k, v);
        }
        let resp = builder.send().await?;
        Ok(resp)
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
    async fn post(&self, url: &str, body: &str, headers: &[(&str, &str)]) -> anyhow::Result<Value> {
        let client = self.read().await.http_client();
        PhotosSession::post(&client, url, body, headers).await
    }

    async fn get(&self, url: &str, headers: &[(&str, &str)]) -> anyhow::Result<reqwest::Response> {
        let client = self.read().await.http_client();
        PhotosSession::get(&client, url, headers).await
    }

    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

/// CloudKit server error codes that indicate a transient condition.
/// These arrive as HTTP 200 with a `serverErrorCode` field in the JSON body.
const RETRYABLE_SERVER_ERRORS: &[&str] =
    &["RETRY_LATER", "TRY_AGAIN_LATER", "CAS_OP_LOCK", "THROTTLED"];

/// CloudKit server error codes that indicate the iCloud service is not
/// activated or accessible (e.g. ADP enabled, incomplete iCloud setup).
const SERVICE_NOT_ACTIVATED_ERRORS: &[&str] = &["ZONE_NOT_FOUND", "AUTHENTICATION_FAILED"];

/// Error type for CloudKit server errors embedded in the JSON response body.
/// These are distinct from HTTP-level errors and represent API-level failures.
#[derive(Debug, thiserror::Error)]
#[error("CloudKit server error: {code} — {reason}")]
pub struct CloudKitServerError {
    pub code: String,
    pub reason: String,
    pub retryable: bool,
    /// True when the error indicates the iCloud service is not activated
    /// (ADP enabled, incomplete setup, or private db access disabled).
    pub service_not_activated: bool,
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

/// Check a CloudKit JSON response for `serverErrorCode` or per-record errors.
/// Returns `Err` if a server error is found, `Ok(response)` otherwise.
fn check_cloudkit_errors(response: Value) -> anyhow::Result<Value> {
    // Top-level serverErrorCode (e.g. from CAS Op-Lock)
    if let Some(code) = response["serverErrorCode"].as_str() {
        let reason = response["reason"]
            .as_str()
            .or_else(|| response["serverErrorMessage"].as_str())
            .unwrap_or("unknown")
            .to_string();
        let retryable = RETRYABLE_SERVER_ERRORS
            .iter()
            .any(|&s| s.eq_ignore_ascii_case(code));
        let service_not_activated = is_service_not_activated(code, &reason);
        tracing::warn!(
            error_code = code,
            retryable,
            service_not_activated,
            "CloudKit server error: {reason}"
        );
        return Err(CloudKitServerError {
            code: code.to_string(),
            reason,
            retryable,
            service_not_activated,
        }
        .into());
    }

    // Per-record errors in the records array
    if let Some(records) = response["records"].as_array() {
        for record in records {
            if let Some(code) = record["serverErrorCode"].as_str() {
                let reason = record["reason"].as_str().unwrap_or("unknown").to_string();
                let retryable = RETRYABLE_SERVER_ERRORS
                    .iter()
                    .any(|&s| s.eq_ignore_ascii_case(code));
                let service_not_activated = is_service_not_activated(code, &reason);
                tracing::warn!(
                    error_code = code,
                    retryable,
                    service_not_activated,
                    "CloudKit per-record error: {reason}"
                );
                return Err(CloudKitServerError {
                    code: code.to_string(),
                    reason,
                    retryable,
                    service_not_activated,
                }
                .into());
            }
        }
    }

    Ok(response)
}

/// Classify API errors for retry: network failures, server-side errors
/// (5xx, 429), and retryable CloudKit server errors are transient;
/// client errors (4xx) and non-retryable server errors are permanent.
fn classify_api_error(e: &anyhow::Error) -> RetryAction {
    if let Some(ck_err) = e.downcast_ref::<CloudKitServerError>() {
        return if ck_err.retryable {
            RetryAction::Retry
        } else {
            RetryAction::Abort
        };
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
/// Inspects each response for CloudKit server errors (`serverErrorCode`)
/// and converts retryable ones (e.g. `TRY_AGAIN_LATER`, `CAS_OP_LOCK`)
/// into transient errors that trigger automatic retry.
pub async fn retry_post(
    session: &dyn PhotosSession,
    url: &str,
    body: &str,
    headers: &[(&str, &str)],
) -> anyhow::Result<Value> {
    let config = RetryConfig::default();
    retry::retry_with_backoff(&config, classify_api_error, || async {
        let response = session.post(url, body, headers).await?;
        check_cloudkit_errors(response)
    })
    .await
}

/// Errors from `changes/zone` when syncToken is invalid.
#[derive(Debug, thiserror::Error)]
pub enum SyncTokenError {
    /// Token is invalid/corrupted — fall back to full enumeration
    #[error("Invalid sync token: {reason}")]
    InvalidToken { reason: String },
    /// Zone no longer exists — stop syncing this zone
    #[error("Zone not found: {zone_name}")]
    ZoneNotFound { zone_name: String },
    /// Unexpected zone-level error (e.g. RETRY_LATER, THROTTLED) —
    /// treat as transient; do NOT advance the sync token.
    #[error("Unexpected zone error in {zone_name}: {error_code}")]
    UnexpectedZoneError {
        zone_name: String,
        error_code: String,
    },
}

impl SyncTokenError {
    /// Whether this error should trigger a fallback from incremental to full sync.
    /// Only token/zone-level issues warrant full re-enumeration; transient errors
    /// (THROTTLED, RETRY_LATER) should propagate without triggering an expensive fallback.
    pub fn should_fallback_to_full(&self) -> bool {
        matches!(
            self,
            SyncTokenError::InvalidToken { .. } | SyncTokenError::ZoneNotFound { .. }
        )
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
            reason: reason
                .unwrap_or("Unknown sync continuation type")
                .to_string(),
        }),
        Some("ZONE_NOT_FOUND") => Err(SyncTokenError::ZoneNotFound {
            zone_name: zone_name.to_string(),
        }),
        Some(code) => Err(SyncTokenError::UnexpectedZoneError {
            zone_name: zone_name.to_string(),
            error_code: code.to_string(),
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
        assert_eq!(ck_err.code, "TRY_AGAIN_LATER");
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
    fn test_check_cloudkit_errors_per_record() {
        let response = serde_json::json!({
            "records": [
                {"recordName": "A"},
                {"serverErrorCode": "RETRY_LATER", "reason": "busy"}
            ]
        });
        let err = check_cloudkit_errors(response).unwrap_err();
        let ck_err = err.downcast_ref::<CloudKitServerError>().unwrap();
        assert_eq!(ck_err.code, "RETRY_LATER");
        assert!(ck_err.retryable);
        assert!(!ck_err.service_not_activated);
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
        assert_eq!(ck_err.reason, "fallback message");
    }

    #[test]
    fn test_check_cloudkit_errors_no_reason_defaults_to_unknown() {
        let response = serde_json::json!({
            "serverErrorCode": "SOME_ERROR"
        });
        let err = check_cloudkit_errors(response).unwrap_err();
        let ck_err = err.downcast_ref::<CloudKitServerError>().unwrap();
        assert_eq!(ck_err.reason, "unknown");
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
                assert_eq!(zone_name, "PrimarySync");
                assert_eq!(error_code, "SOME_OTHER_CODE");
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
                assert_eq!(reason, "Unknown sync continuation type");
            }
            other => panic!("Expected InvalidToken, got {other:?}"),
        }
    }

    #[test]
    fn test_check_changes_zone_error_bad_request_no_reason() {
        let result = check_changes_zone_error(Some("BAD_REQUEST"), None, "PrimarySync");
        match result.unwrap_err() {
            SyncTokenError::InvalidToken { reason } => {
                assert_eq!(reason, "Unknown sync continuation type");
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
                assert_eq!(zone_name, "SharedSync-123");
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
        let err = SyncTokenError::InvalidToken {
            reason: String::new(),
        };
        assert_eq!(err.to_string(), "Invalid sync token: ");
    }
}
