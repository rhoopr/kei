use serde_json::Value;

/// Minimal async session used by the photos service.
/// The concrete implementation lives in `crate::auth::session`.
#[async_trait::async_trait]
#[allow(dead_code)]
pub trait PhotosSession: Send + Sync {
    async fn post(
        &self,
        url: &str,
        body: &str,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<Value>;

    async fn get(
        &self,
        url: &str,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<reqwest::Response>;

    /// Clone this session into a new boxed trait object.
    fn clone_box(&self) -> Box<dyn PhotosSession>;
}

// A convenience blanket implementation for `reqwest::Client` so that
// callers can use it directly without a full auth session.
#[async_trait::async_trait]
impl PhotosSession for reqwest::Client {
    async fn post(
        &self,
        url: &str,
        body: &str,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        let mut builder = self.post(url).body(body.to_owned());
        for &(k, v) in headers {
            builder = builder.header(k, v);
        }
        let resp = builder.send().await?;
        let json: Value = resp.json().await?;
        Ok(json)
    }

    async fn get(
        &self,
        url: &str,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<reqwest::Response> {
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
