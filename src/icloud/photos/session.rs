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

/// Error type for CloudKit server errors embedded in the JSON response body.
/// These are distinct from HTTP-level errors and represent API-level failures.
#[derive(Debug, thiserror::Error)]
#[error("CloudKit server error: {code} — {reason}")]
pub struct CloudKitServerError {
    pub code: String,
    pub reason: String,
    pub retryable: bool,
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
        tracing::warn!(
            error_code = code,
            retryable,
            "CloudKit server error: {reason}"
        );
        return Err(CloudKitServerError {
            code: code.to_string(),
            reason,
            retryable,
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
                tracing::warn!(
                    error_code = code,
                    retryable,
                    "CloudKit per-record error: {reason}"
                );
                return Err(CloudKitServerError {
                    code: code.to_string(),
                    reason,
                    retryable,
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
}
