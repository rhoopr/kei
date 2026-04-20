//! Shared test fixtures and builders.
//!
//! Provides ergonomic builders for commonly constructed test objects
//! and reusable mock implementations of core traits.

use std::collections::VecDeque;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use crate::icloud::photos::session::PhotosSession;
use crate::icloud::photos::PhotoAsset;
use crate::state::types::{AssetMetadata, AssetRecord, MediaType, VersionSizeKey};

// ── AssetRecord builder ─────────────────────────────────────────────

/// Builder for `AssetRecord::new_pending()` with sensible test defaults.
///
/// ```ignore
/// let record = TestAssetRecord::new("MY_ID").build();
/// let record = TestAssetRecord::new("MY_ID").checksum("abc").size(5000).build();
/// ```
pub struct TestAssetRecord {
    id: String,
    version_size: VersionSizeKey,
    checksum: String,
    filename: String,
    created_at: DateTime<Utc>,
    added_at: Option<DateTime<Utc>>,
    size_bytes: u64,
    media_type: MediaType,
    metadata: Option<AssetMetadata>,
}

impl TestAssetRecord {
    pub fn new(id: &str) -> Self {
        Self {
            id: id.to_string(),
            version_size: VersionSizeKey::Original,
            checksum: "checksum123".to_string(),
            filename: "photo.jpg".to_string(),
            created_at: Utc::now(),
            added_at: None,
            size_bytes: 12345,
            media_type: MediaType::Photo,
            metadata: None,
        }
    }

    pub fn checksum(mut self, c: &str) -> Self {
        self.checksum = c.to_string();
        self
    }

    pub fn filename(mut self, f: &str) -> Self {
        self.filename = f.to_string();
        self
    }

    pub fn created_at(mut self, t: DateTime<Utc>) -> Self {
        self.created_at = t;
        self
    }

    pub fn added_at(mut self, t: DateTime<Utc>) -> Self {
        self.added_at = Some(t);
        self
    }

    pub fn size(mut self, s: u64) -> Self {
        self.size_bytes = s;
        self
    }

    pub fn media_type(mut self, m: MediaType) -> Self {
        self.media_type = m;
        self
    }

    pub fn metadata(mut self, m: AssetMetadata) -> Self {
        self.metadata = Some(m);
        self
    }

    pub fn version_size(mut self, v: VersionSizeKey) -> Self {
        self.version_size = v;
        self
    }

    pub fn build(self) -> AssetRecord {
        let record = AssetRecord::new_pending(
            self.id,
            self.version_size,
            self.checksum,
            self.filename,
            self.created_at,
            self.added_at,
            self.size_bytes,
            self.media_type,
        );
        if let Some(meta) = self.metadata {
            record.with_metadata(meta)
        } else {
            record
        }
    }
}

// ── PhotoAsset builder ──────────────────────────────────────────────

/// Builder for `PhotoAsset::new()` with sensible test defaults.
///
/// ```ignore
/// let asset = TestPhotoAsset::new("TEST_1").build();
/// let asset = TestPhotoAsset::new("LIVE_1")
///     .filename("IMG_0001.HEIC")
///     .item_type("public.heic")
///     .orig_file_type("public.heic")
///     .live_photo("https://p01.icloud-content.com/mov", "mov_ck", 3000)
///     .build();
/// ```
pub struct TestPhotoAsset {
    record_name: String,
    filename: String,
    item_type: String,
    orig_size: u64,
    orig_url: String,
    orig_checksum: String,
    orig_file_type: String,
    asset_date: f64,
    live_photo: Option<LivePhotoFields>,
    alt_version: Option<AltVersionFields>,
}

struct LivePhotoFields {
    url: String,
    checksum: String,
    size: u64,
}

struct AltVersionFields {
    url: String,
    checksum: String,
    size: u64,
    file_type: String,
}

impl TestPhotoAsset {
    pub fn new(record_name: &str) -> Self {
        Self {
            record_name: record_name.to_string(),
            filename: "photo.jpg".to_string(),
            item_type: "public.jpeg".to_string(),
            orig_size: 1000,
            orig_url: "https://p01.icloud-content.com/orig".to_string(),
            orig_checksum: "abc123".to_string(),
            orig_file_type: "public.jpeg".to_string(),
            asset_date: 1736899200000.0,
            live_photo: None,
            alt_version: None,
        }
    }

    pub fn filename(mut self, f: &str) -> Self {
        self.filename = f.to_string();
        self
    }

    pub fn item_type(mut self, t: &str) -> Self {
        self.item_type = t.to_string();
        self
    }

    pub fn orig_size(mut self, s: u64) -> Self {
        self.orig_size = s;
        self
    }

    pub fn orig_url(mut self, u: &str) -> Self {
        self.orig_url = u.to_string();
        self
    }

    pub fn orig_checksum(mut self, c: &str) -> Self {
        self.orig_checksum = c.to_string();
        self
    }

    pub fn orig_file_type(mut self, t: &str) -> Self {
        self.orig_file_type = t.to_string();
        self
    }

    pub fn asset_date(mut self, d: f64) -> Self {
        self.asset_date = d;
        self
    }

    pub fn live_photo(mut self, url: &str, checksum: &str, size: u64) -> Self {
        self.live_photo = Some(LivePhotoFields {
            url: url.to_string(),
            checksum: checksum.to_string(),
            size,
        });
        self
    }

    pub fn alt_version(mut self, url: &str, checksum: &str, size: u64, file_type: &str) -> Self {
        self.alt_version = Some(AltVersionFields {
            url: url.to_string(),
            checksum: checksum.to_string(),
            size,
            file_type: file_type.to_string(),
        });
        self
    }

    pub fn build(self) -> PhotoAsset {
        let mut fields = json!({
            "filenameEnc": {"value": self.filename, "type": "STRING"},
            "itemType": {"value": self.item_type},
            "resOriginalRes": {"value": {
                "size": self.orig_size,
                "downloadURL": self.orig_url,
                "fileChecksum": self.orig_checksum,
            }},
            "resOriginalFileType": {"value": self.orig_file_type},
        });

        if let Some(lp) = &self.live_photo {
            fields["resOriginalVidComplRes"] = json!({"value": {
                "size": lp.size,
                "downloadURL": lp.url,
                "fileChecksum": lp.checksum,
            }});
            fields["resOriginalVidComplFileType"] = json!({"value": "com.apple.quicktime-movie"});
        }

        if let Some(alt) = &self.alt_version {
            fields["resOriginalAltRes"] = json!({"value": {
                "size": alt.size,
                "downloadURL": alt.url,
                "fileChecksum": alt.checksum,
            }});
            fields["resOriginalAltFileType"] = json!({"value": alt.file_type});
        }

        let master = json!({
            "recordName": self.record_name,
            "fields": fields,
        });
        let asset = json!({
            "fields": {"assetDate": {"value": self.asset_date}},
        });
        PhotoAsset::new(master, asset)
    }
}

// ── Mock PhotosSession ──────────────────────────────────────────────

/// Recorded call to `MockPhotosSession::post()`.
#[derive(Debug, Clone)]
pub struct RecordedCall {
    pub url: String,
    pub _body: String,
}

/// Response action for a single `post()` call.
pub enum MockResponse {
    /// Return `Ok(value)`.
    Ok(Value),
    /// Return `Err(...)`.
    Err(String),
}

/// A configurable mock `PhotosSession` that supports:
/// - Sequenced responses (success or error per call)
/// - Call recording for assertion
/// - Fallback to empty `{"records": []}` when the queue is exhausted
///
/// ```ignore
/// let mock = MockPhotosSession::new()
///     .ok(json!({"records": [...]}))
///     .err("simulated failure")
///     .ok(json!({"records": []}));
/// ```
pub struct MockPhotosSession {
    responses: Mutex<VecDeque<MockResponse>>,
    calls: Mutex<Vec<RecordedCall>>,
}

impl MockPhotosSession {
    pub fn new() -> Self {
        Self {
            responses: Mutex::new(VecDeque::new()),
            calls: Mutex::new(Vec::new()),
        }
    }

    /// Queue a successful response.
    pub fn ok(self, value: Value) -> Self {
        self.responses
            .lock()
            .expect("poisoned")
            .push_back(MockResponse::Ok(value));
        self
    }

    /// Queue an error response.
    pub fn err(self, message: &str) -> Self {
        self.responses
            .lock()
            .expect("poisoned")
            .push_back(MockResponse::Err(message.to_string()));
        self
    }

    /// Return all recorded calls for assertion.
    pub fn recorded_calls(&self) -> Vec<RecordedCall> {
        self.calls.lock().expect("poisoned").clone()
    }

    /// Return the number of calls made.
    pub fn call_count(&self) -> usize {
        self.calls.lock().expect("poisoned").len()
    }
}

#[async_trait::async_trait]
impl PhotosSession for MockPhotosSession {
    async fn post(
        &self,
        url: &str,
        body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        self.calls.lock().expect("poisoned").push(RecordedCall {
            url: url.to_string(),
            _body: body,
        });

        let response = self.responses.lock().expect("poisoned").pop_front();

        match response {
            Some(MockResponse::Ok(v)) => Ok(v),
            Some(MockResponse::Err(msg)) => Err(anyhow::anyhow!("{msg}")),
            None => Ok(json!({"records": []})),
        }
    }

    fn clone_box(&self) -> Box<dyn PhotosSession> {
        let remaining: Vec<MockResponse> = {
            let queue = self.responses.lock().expect("poisoned");
            queue
                .iter()
                .map(|r| match r {
                    MockResponse::Ok(v) => MockResponse::Ok(v.clone()),
                    MockResponse::Err(msg) => MockResponse::Err(msg.clone()),
                })
                .collect()
        };
        let mut new = MockPhotosSession::new();
        *new.responses.get_mut().unwrap() = remaining.into();
        Box::new(new)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_session_records_calls_and_sequences_responses() {
        let mock = MockPhotosSession::new()
            .ok(json!({"records": [{"id": 1}]}))
            .err("server error");

        assert_eq!(mock.call_count(), 0);

        let r1 = mock
            .post("https://example.com/query", "{}".to_owned(), &[])
            .await;
        assert!(r1.is_ok());
        assert_eq!(mock.call_count(), 1);

        let r2 = mock
            .post("https://example.com/changes", "{}".to_owned(), &[])
            .await;
        assert!(r2.is_err());
        assert_eq!(mock.call_count(), 2);

        // Exhausted queue falls back to empty records
        let r3 = mock
            .post("https://example.com/extra", "{}".to_owned(), &[])
            .await;
        assert_eq!(r3.unwrap(), json!({"records": []}));

        let calls = mock.recorded_calls();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0].url, "https://example.com/query");
        assert_eq!(calls[1].url, "https://example.com/changes");
    }
}
