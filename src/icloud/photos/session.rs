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

/// Classify API errors for retry: network failures and server-side errors
/// (5xx, 429) are transient; client errors (4xx) indicate a real problem.
fn classify_api_error(e: &anyhow::Error) -> RetryAction {
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
pub async fn retry_post(
    session: &dyn PhotosSession,
    url: &str,
    body: &str,
    headers: &[(&str, &str)],
) -> anyhow::Result<Value> {
    let config = RetryConfig::default();
    retry::retry_with_backoff(&config, classify_api_error, || {
        session.post(url, body, headers)
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
