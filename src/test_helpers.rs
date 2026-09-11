//! Shared test fixtures and builders.
//!
//! Provides ergonomic builders for commonly constructed test objects
//! and reusable mock implementations of core traits.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use serde_json::{Value, json};

use crate::icloud::photos::PhotoAsset;
use crate::icloud::photos::session::PhotosSession;
use crate::state::types::{AssetMetadata, AssetRecord, MediaType, VersionSizeKey};

#[cfg(test)]
fn loopback_bind_unavailable_reason() -> Option<String> {
    static RESULT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    RESULT
        .get_or_init(|| {
            let addr = std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, 0));
            match std::net::TcpListener::bind(addr) {
                Ok(listener) => {
                    drop(listener);
                    None
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::PermissionDenied
                        || e.raw_os_error() == Some(1) =>
                {
                    Some(format!("loopback bind is not permitted on this host: {e}"))
                }
                Err(e) => panic!("loopback bind probe failed unexpectedly: {e}"),
            }
        })
        .clone()
}

#[cfg(test)]
pub(crate) fn skip_if_loopback_bind_blocked(test_name: &str) -> bool {
    if let Some(reason) = loopback_bind_unavailable_reason() {
        eprintln!("skipping {test_name}: {reason}");
        true
    } else {
        false
    }
}

#[cfg(test)]
pub(crate) async fn start_wiremock_or_skip(test_name: &str) -> Option<wiremock::MockServer> {
    if skip_if_loopback_bind_blocked(test_name) {
        None
    } else {
        Some(wiremock::MockServer::start().await)
    }
}

#[cfg(test)]
#[macro_export]
macro_rules! start_wiremock_or_skip {
    () => {{
        match $crate::test_helpers::start_wiremock_or_skip(module_path!()).await {
            Some(server) => server,
            None => return,
        }
    }};
    ($test_name:expr) => {{
        match $crate::test_helpers::start_wiremock_or_skip($test_name).await {
            Some(server) => server,
            None => return,
        }
    }};
}

// ── Tracing capture helper ─────────────────────────────────────────

#[cfg(test)]
#[derive(Debug, Clone)]
pub struct CapturedEvent {
    pub level: tracing::Level,
    pub fields: std::collections::HashMap<String, String>,
}

#[cfg(test)]
impl CapturedEvent {
    pub fn field(&self, name: &str) -> Option<&str> {
        self.fields.get(name).map(String::as_str)
    }

    pub fn message(&self) -> Option<&str> {
        self.field("message")
    }
}

#[cfg(test)]
#[derive(Clone, Default)]
pub struct TracingCapture {
    events: Arc<Mutex<Vec<CapturedEvent>>>,
}

#[cfg(test)]
impl TracingCapture {
    pub fn install() -> (Self, tracing::subscriber::DefaultGuard) {
        use tracing_subscriber::prelude::*;

        let capture = Self::default();
        let subscriber = tracing_subscriber::registry().with(CaptureLayer {
            events: Arc::clone(&capture.events),
        });
        let guard = tracing::subscriber::set_default(subscriber);
        (capture, guard)
    }

    pub fn events(&self) -> Vec<CapturedEvent> {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn contains_event(&self, predicate: impl Fn(&CapturedEvent) -> bool) -> bool {
        self.events().iter().any(predicate)
    }
}

#[cfg(test)]
struct CaptureLayer {
    events: Arc<Mutex<Vec<CapturedEvent>>>,
}

#[cfg(test)]
impl<S> tracing_subscriber::Layer<S> for CaptureLayer
where
    S: tracing::Subscriber,
{
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(CapturedEvent {
                level: *event.metadata().level(),
                fields: visitor.fields,
            });
    }
}

#[cfg(test)]
#[derive(Default)]
struct FieldVisitor {
    fields: std::collections::HashMap<String, String>,
}

#[cfg(test)]
impl tracing::field::Visit for FieldVisitor {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.fields
            .insert(field.name().to_string(), value.to_string());
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.fields
            .insert(field.name().to_string(), value.to_string());
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.fields
            .insert(field.name().to_string(), value.to_string());
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.fields
            .insert(field.name().to_string(), value.to_string());
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.fields
            .insert(field.name().to_string(), format!("{value:?}"));
    }
}

// ── AssetRecord builder ─────────────────────────────────────────────

/// Builder for `AssetRecord::new_pending()` with sensible test defaults.
///
/// ```ignore
/// let record = TestAssetRecord::new("MY_ID").build();
/// let record = TestAssetRecord::new("MY_ID").checksum("abc").size(5000).build();
/// ```
pub struct TestAssetRecord {
    library: String,
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
            library: crate::icloud::photos::PRIMARY_ZONE_NAME.to_string(),
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

    pub fn library(mut self, library: &str) -> Self {
        self.library = library.to_string();
        self
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
            std::sync::Arc::from(self.library),
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
    timezone_offset: Option<i32>,
    favorite: bool,
    live_photo: Option<LivePhotoFields>,
    adjusted_version: Option<AdjustedVersionFields>,
    live_adjusted: Option<LivePhotoFields>,
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

struct AdjustedVersionFields {
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
            timezone_offset: None,
            favorite: false,
            live_photo: None,
            adjusted_version: None,
            live_adjusted: None,
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

    pub fn timezone_offset(mut self, seconds: i32) -> Self {
        self.timezone_offset = Some(seconds);
        self
    }

    /// Toggle the source favourite flag, which iCloud maps to a 5-star rating.
    pub fn favorite(mut self, favorite: bool) -> Self {
        self.favorite = favorite;
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

    pub fn adjusted_version(
        mut self,
        url: &str,
        checksum: &str,
        size: u64,
        file_type: &str,
    ) -> Self {
        self.adjusted_version = Some(AdjustedVersionFields {
            url: url.to_string(),
            checksum: checksum.to_string(),
            size,
            file_type: file_type.to_string(),
        });
        self
    }

    pub fn live_adjusted(mut self, url: &str, checksum: &str, size: u64) -> Self {
        self.live_adjusted = Some(LivePhotoFields {
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

        if let Some(adjusted) = &self.adjusted_version {
            fields["resJPEGFullRes"] = json!({"value": {
                "size": adjusted.size,
                "downloadURL": adjusted.url,
                "fileChecksum": adjusted.checksum,
            }});
            fields["resJPEGFullFileType"] = json!({"value": adjusted.file_type});
        }

        if let Some(lp) = &self.live_adjusted {
            fields["resVidComplRes"] = json!({"value": {
                "size": lp.size,
                "downloadURL": lp.url,
                "fileChecksum": lp.checksum,
            }});
            fields["resVidComplFileType"] = json!({"value": "com.apple.quicktime-movie"});
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
        let mut asset = json!({
            "fields": {
                "assetDate": {"value": self.asset_date},
                "isFavorite": {"value": i64::from(self.favorite)},
            },
        });
        if let Some(offset) = self.timezone_offset {
            asset["fields"]["timeZoneOffset"] = json!({"value": offset});
        }
        PhotoAsset::new(master, asset)
    }
}

// ── CloudKit/Photos response flow builder ───────────────────────────

/// Small builder for the queued CloudKit responses used by `MockPhotosSession`.
///
/// This intentionally only covers the response shapes current tests repeat:
/// album-count batches, `/records/query` pages, `/changes/database` pages,
/// `/changes/zone` pages, and queued transport errors. It is not a fake
/// CloudKit implementation.
pub struct MockPhotosFlow {
    session: MockPhotosSession,
}

impl MockPhotosFlow {
    pub fn new() -> Self {
        Self {
            session: MockPhotosSession::new(),
        }
    }

    pub fn album_count(mut self, count: u64) -> Self {
        self.session = self.session.ok(json!({
            "batch": [{"records": [{"fields": {"itemCount": {"value": count}}}]}]
        }));
        self
    }

    pub fn album_count_response(mut self, response: Value) -> Self {
        self.session = self.session.ok(response);
        self
    }

    pub fn query_page(mut self, records: Vec<Value>, sync_token: Option<&str>) -> Self {
        let mut page = json!({ "records": records });
        if let Some(token) = sync_token {
            page["syncToken"] = json!(token);
        }
        self.session = self.session.ok(page);
        self
    }

    pub fn query_photo_page(mut self, record_name: &str, sync_token: Option<&str>) -> Self {
        self.session = self
            .session
            .ok(mock_photo_query_page(record_name, sync_token));
        self
    }

    pub fn empty_query_page(self, sync_token: Option<&str>) -> Self {
        self.query_page(Vec::new(), sync_token)
    }

    pub fn changes_database(
        mut self,
        sync_token: &str,
        changed_zones: &[(&str, &str)],
        more_coming: bool,
    ) -> Self {
        let zones: Vec<Value> = changed_zones
            .iter()
            .map(|(zone_name, zone_sync_token)| {
                json!({
                    "zoneID": {"zoneName": zone_name, "ownerRecordName": "_defaultOwner"},
                    "syncToken": zone_sync_token,
                })
            })
            .collect();
        self.session = self.session.ok(json!({
            "syncToken": sync_token,
            "moreComing": more_coming,
            "zones": zones,
        }));
        self
    }

    pub fn changes_zone_page(
        mut self,
        records: Vec<Value>,
        sync_token: &str,
        more_coming: bool,
    ) -> Self {
        self.session = self.session.ok(json!({
            "zones": [{
                "zoneID": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"},
                "syncToken": sync_token,
                "moreComing": more_coming,
                "records": records,
            }]
        }));
        self
    }

    pub fn changes_photo_page(
        self,
        record_name: &str,
        sync_token: &str,
        more_coming: bool,
    ) -> Self {
        self.changes_zone_page(mock_photo_records(record_name), sync_token, more_coming)
    }

    pub fn changes_zone_error(
        mut self,
        server_error_code: &str,
        reason: &str,
        sync_token: &str,
    ) -> Self {
        self.session = self.session.ok(json!({
            "zones": [{
                "zoneID": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"},
                "syncToken": sync_token,
                "moreComing": false,
                "serverErrorCode": server_error_code,
                "reason": reason,
            }]
        }));
        self
    }

    pub fn error(mut self, message: &str) -> Self {
        self.session = self.session.err(message);
        self
    }

    pub fn build(self) -> MockPhotosSession {
        self.session
    }
}

pub(crate) fn mock_photo_query_page(record_name: &str, sync_token: Option<&str>) -> Value {
    let mut page = json!({ "records": mock_photo_records(record_name) });
    if let Some(token) = sync_token {
        page["syncToken"] = json!(token);
    }
    page
}

fn mock_photo_records(record_name: &str) -> Vec<Value> {
    mock_photo_records_for_zone_with_filename(record_name, "PrimarySync", "test.jpg")
}

pub(crate) fn mock_photo_records_for_zone_with_filename(
    record_name: &str,
    zone: &str,
    filename: &str,
) -> Vec<Value> {
    mock_photo_records_for_zone_with_filename_and_asset_date(
        record_name,
        zone,
        filename,
        1700000000000,
    )
}

pub(crate) fn mock_photo_records_for_zone_with_filename_and_asset_date(
    record_name: &str,
    zone: &str,
    filename: &str,
    asset_date: i64,
) -> Vec<Value> {
    vec![
        json!({
            "recordName": record_name,
            "recordType": "CPLMaster",
            "fields": {
                "filenameEnc": {"value": filename, "type": "STRING"},
                "resOriginalRes": {
                    "value": {
                        "downloadURL": format!("https://p01.icloud-content.com/{record_name}.jpg"),
                        "size": 1024,
                        "fileChecksum": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
                    }
                },
                "resOriginalWidth": {"value": 100, "type": "INT64"},
                "resOriginalHeight": {"value": 100, "type": "INT64"},
                "resOriginalFileType": {"value": "public.jpeg"},
                "itemType": {"value": "public.jpeg"},
                "adjustmentRenderType": {"value": 0, "type": "INT64"}
            },
            "recordChangeTag": "ct1"
        }),
        json!({
            "recordName": format!("asset-{record_name}"),
            "recordType": "CPLAsset",
            "fields": {
                "masterRef": {
                        "value": {
                            "recordName": record_name,
                            "zoneID": {"zoneName": zone}
                        },
                        "type": "REFERENCE"
                    },
                "assetDate": {"value": asset_date, "type": "TIMESTAMP"},
                "addedDate": {"value": asset_date, "type": "TIMESTAMP"}
            },
            "recordChangeTag": "ct2"
        }),
    ]
}

#[derive(Clone, Debug)]
pub struct DynamicRecentPhotosSession {
    ids: Arc<Vec<String>>,
    zone: Arc<str>,
    token: Arc<str>,
    filename_prefix: Arc<str>,
    error_at_offset: Option<u64>,
    offsets: Arc<Mutex<Vec<u64>>>,
    results_limits: Arc<Mutex<Vec<u64>>>,
    emitted_ids: Arc<Mutex<Vec<String>>>,
}

impl DynamicRecentPhotosSession {
    pub fn new(total_assets: u64) -> Self {
        Self::from_ids(
            (0..total_assets)
                .map(|i| format!("master-{i:04}"))
                .collect(),
        )
    }

    pub fn from_ids(ids: Vec<String>) -> Self {
        Self {
            ids: Arc::new(ids),
            zone: Arc::from("PrimarySync"),
            token: Arc::from("zone-token"),
            filename_prefix: Arc::from("photo"),
            error_at_offset: None,
            offsets: Arc::new(Mutex::new(Vec::new())),
            results_limits: Arc::new(Mutex::new(Vec::new())),
            emitted_ids: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn with_zone(mut self, zone: &str) -> Self {
        self.zone = Arc::from(zone);
        self
    }

    pub fn with_token(mut self, token: &str) -> Self {
        self.token = Arc::from(token);
        self
    }

    pub fn with_filename_prefix(mut self, prefix: &str) -> Self {
        self.filename_prefix = Arc::from(prefix);
        self
    }

    pub fn with_error_at_offset(mut self, offset: u64) -> Self {
        self.error_at_offset = Some(offset);
        self
    }

    pub fn offsets(&self) -> Vec<u64> {
        self.offsets.lock().expect("offsets lock").clone()
    }

    pub fn results_limits(&self) -> Vec<u64> {
        self.results_limits
            .lock()
            .expect("results limits lock")
            .clone()
    }

    pub fn emitted_ids(&self) -> Vec<String> {
        self.emitted_ids.lock().expect("emitted ids lock").clone()
    }
}

#[async_trait::async_trait]
impl PhotosSession for DynamicRecentPhotosSession {
    async fn post(
        &self,
        url: &str,
        body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        if url.contains("/internal/records/query/batch") {
            return Ok(json!({
                "batch": [{"records": [{"fields": {"itemCount": {"value": self.ids.len() as u64}}}]}]
            }));
        }

        if !url.contains("/records/query?") {
            return Ok(json!({"records": []}));
        }

        let request: Value = serde_json::from_str(&body)?;
        let offset = request["query"]["filterBy"]
            .as_array()
            .and_then(|filters| {
                filters.iter().find_map(|filter| {
                    (filter["fieldName"] == "startRank")
                        .then(|| filter["fieldValue"]["value"].as_u64())
                        .flatten()
                })
            })
            .unwrap_or(0);
        let results_limit = request["resultsLimit"].as_u64().unwrap_or(0);
        self.offsets.lock().expect("offsets lock").push(offset);
        self.results_limits
            .lock()
            .expect("results limits lock")
            .push(results_limit);

        if self
            .error_at_offset
            .is_some_and(|error_offset| offset >= error_offset)
        {
            return Err(anyhow::anyhow!(
                "simulated records/query failure at offset {offset}"
            ));
        }

        let start = usize::try_from(offset).unwrap_or(usize::MAX);
        let page_assets = usize::try_from(results_limit / 2).unwrap_or(usize::MAX);
        let end = start.saturating_add(page_assets).min(self.ids.len());
        if start >= end {
            return Ok(json!({"records": [], "syncToken": self.token.as_ref()}));
        }

        let mut emitted = self.emitted_ids.lock().expect("emitted ids lock");
        let mut records = Vec::with_capacity((end - start) * 2);
        for index in start..end {
            let id = &self.ids[index];
            emitted.push(id.clone());
            records.extend(mock_photo_records_for_zone_with_filename(
                id,
                &self.zone,
                &format!("{}-{index:04}.jpg", self.filename_prefix),
            ));
        }
        drop(emitted);

        Ok(json!({"records": records, "syncToken": self.token.as_ref()}))
    }

    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

impl Default for MockPhotosFlow {
    fn default() -> Self {
        Self::new()
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

        let response = {
            let mut responses = self.responses.lock().expect("poisoned");
            if url.contains("/internal/records/query/batch") {
                match responses.front() {
                    Some(MockResponse::Ok(value)) if value.get("batch").is_some() => {
                        responses.pop_front()
                    }
                    Some(MockResponse::Err(_)) => responses.pop_front(),
                    _ => None,
                }
            } else {
                responses.pop_front()
            }
        };

        match response {
            Some(MockResponse::Ok(v)) => Ok(v),
            Some(MockResponse::Err(msg)) => Err(anyhow::anyhow!("{msg}")),
            None if url.contains("/internal/records/query/batch") => Ok(json!({
                "batch": [{"records": [{"fields": {"itemCount": {"value": 0}}}]}]
            })),
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

// ── Source EXIF GPS fixtures ────────────────────────────────────────

/// Facts that `exif_with_source_gps` yields once read and composed.
#[cfg(feature = "xmp")]
pub const SOURCE_GPS_DATETIME: &str = "2024-06-15T10:20:30.125Z";
#[cfg(feature = "xmp")]
pub const SOURCE_GPS_SPEED: &str = "12345/100";
#[cfg(feature = "xmp")]
pub const SOURCE_GPS_SPEED_REF: &str = "K";
#[cfg(feature = "xmp")]
pub const SOURCE_GPS_H_POSITIONING_ERROR: &str = "3/2";

/// EXIF `uR64` rational from a numerator and denominator.
#[cfg(feature = "xmp")]
pub fn ur64(nominator: u32, denominator: u32) -> little_exif::rational::uR64 {
    little_exif::rational::uR64 {
        nominator,
        denominator,
    }
}

/// EXIF metadata carrying the standard source GPS facts: GPSDateStamp
/// 2024:06:15, GPSTimeStamp 10:20:30.125, GPSSpeedRef K, GPSSpeed 123.45,
/// GPSHPositioningError 1.5.
#[cfg(feature = "xmp")]
pub fn exif_with_source_gps() -> little_exif::metadata::Metadata {
    use little_exif::exif_tag::ExifTag;
    use little_exif::metadata::Metadata;

    let mut metadata = Metadata::new();
    metadata.set_tag(ExifTag::GPSDateStamp("2024:06:15".into()));
    metadata.set_tag(ExifTag::GPSTimeStamp(vec![
        ur64(10, 1),
        ur64(20, 1),
        ur64(30_125, 1_000),
    ]));
    metadata.set_tag(ExifTag::GPSSpeedRef("K".into()));
    metadata.set_tag(ExifTag::GPSSpeed(vec![ur64(12_345, 100)]));
    metadata.set_tag(ExifTag::GPSHPositioningError(vec![ur64(3, 2)]));
    metadata
}

/// The standard source GPS EXIF without a `GPSTimeStamp`, so a date alone
/// cannot compose a timestamp.
#[cfg(feature = "xmp")]
pub fn source_gps_without_time_stamp() -> little_exif::metadata::Metadata {
    use little_exif::exif_tag::ExifTag;
    use little_exif::metadata::Metadata;

    let mut metadata = Metadata::new();
    metadata.set_tag(ExifTag::GPSDateStamp("2024:06:15".into()));
    metadata.set_tag(ExifTag::GPSSpeedRef("K".into()));
    metadata.set_tag(ExifTag::GPSSpeed(vec![ur64(12_345, 100)]));
    metadata.set_tag(ExifTag::GPSHPositioningError(vec![ur64(3, 2)]));
    metadata
}

/// Minimal JPEG (SOI + APP0 JFIF + EOI) carrying the standard source GPS EXIF.
#[cfg(feature = "xmp")]
pub fn minimal_jpeg_with_source_gps() -> Vec<u8> {
    let mut bytes = vec![
        0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00, 0x00,
        0x01, 0x00, 0x01, 0x00, 0x00, 0xFF, 0xD9,
    ];
    exif_with_source_gps()
        .write_to_vec(&mut bytes, little_exif::filetype::FileExtension::JPEG)
        .expect("write source GPS EXIF into minimal JPEG");
    bytes
}

/// Minimal JPEG whose source EXIF location and capture time deliberately
/// differ from the CloudKit values used by metadata writer tests.
#[cfg(feature = "xmp")]
pub fn minimal_jpeg_with_source_gps_and_location() -> Vec<u8> {
    use little_exif::exif_tag::ExifTag;

    let mut metadata = exif_with_source_gps();
    metadata.set_tag(ExifTag::DateTimeOriginal("2030:01:02 03:04:05".into()));
    metadata.set_tag(ExifTag::GPSLatitudeRef("N".into()));
    metadata.set_tag(ExifTag::GPSLatitude(vec![
        ur64(1, 1),
        ur64(30, 1),
        ur64(0, 1),
    ]));
    metadata.set_tag(ExifTag::GPSLongitudeRef("E".into()));
    metadata.set_tag(ExifTag::GPSLongitude(vec![
        ur64(2, 1),
        ur64(30, 1),
        ur64(0, 1),
    ]));
    metadata.set_tag(ExifTag::GPSAltitudeRef(vec![0]));
    metadata.set_tag(ExifTag::GPSAltitude(vec![ur64(333, 1)]));

    let mut bytes = vec![
        0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00, 0x00,
        0x01, 0x00, 0x01, 0x00, 0x00, 0xFF, 0xD9,
    ];
    metadata
        .write_to_vec(&mut bytes, little_exif::filetype::FileExtension::JPEG)
        .expect("write conflicting source EXIF into minimal JPEG");
    bytes
}

/// Minimal little-endian TIFF carrying the standard source GPS EXIF.
#[cfg(feature = "xmp")]
pub fn minimal_tiff_with_source_gps() -> Vec<u8> {
    minimal_tiff_with_source_gps_endian(false)
}

/// Minimal big-endian TIFF carrying the standard source GPS EXIF.
#[cfg(feature = "xmp")]
pub fn minimal_big_endian_tiff_with_source_gps() -> Vec<u8> {
    minimal_tiff_with_source_gps_endian(true)
}

#[cfg(feature = "xmp")]
fn minimal_tiff_with_source_gps_endian(big_endian: bool) -> Vec<u8> {
    fn push_u16(bytes: &mut Vec<u8>, value: u16, big_endian: bool) {
        let encoded = if big_endian {
            value.to_be_bytes()
        } else {
            value.to_le_bytes()
        };
        bytes.extend_from_slice(&encoded);
    }

    fn push_u32(bytes: &mut Vec<u8>, value: u32, big_endian: bool) {
        let encoded = if big_endian {
            value.to_be_bytes()
        } else {
            value.to_le_bytes()
        };
        bytes.extend_from_slice(&encoded);
    }

    fn push_ifd_entry(
        bytes: &mut Vec<u8>,
        tag: u16,
        value_type: u16,
        count: u32,
        value: u32,
        big_endian: bool,
    ) {
        push_u16(bytes, tag, big_endian);
        push_u16(bytes, value_type, big_endian);
        push_u32(bytes, count, big_endian);
        push_u32(bytes, value, big_endian);
    }

    const IFD0_OFFSET: u32 = 8;
    const GPS_IFD_OFFSET: u32 = 26;
    const GPS_DATE_OFFSET: u32 = 92;
    const GPS_TIME_OFFSET: u32 = 104;
    const GPS_SPEED_OFFSET: u32 = 128;
    const GPS_ERROR_OFFSET: u32 = 136;

    let mut bytes = Vec::with_capacity(144);
    bytes.extend_from_slice(if big_endian { b"MM" } else { b"II" });
    push_u16(&mut bytes, 42, big_endian);
    push_u32(&mut bytes, IFD0_OFFSET, big_endian);

    push_u16(&mut bytes, 1, big_endian);
    push_ifd_entry(&mut bytes, 0x8825, 4, 1, GPS_IFD_OFFSET, big_endian);
    push_u32(&mut bytes, 0, big_endian);

    push_u16(&mut bytes, 5, big_endian);
    push_ifd_entry(&mut bytes, 0x0007, 5, 3, GPS_TIME_OFFSET, big_endian);
    push_ifd_entry(
        &mut bytes,
        0x000C,
        2,
        2,
        if big_endian {
            u32::from_be_bytes([b'K', 0, 0, 0])
        } else {
            u32::from_le_bytes([b'K', 0, 0, 0])
        },
        big_endian,
    );
    push_ifd_entry(&mut bytes, 0x000D, 5, 1, GPS_SPEED_OFFSET, big_endian);
    push_ifd_entry(&mut bytes, 0x001D, 2, 11, GPS_DATE_OFFSET, big_endian);
    push_ifd_entry(&mut bytes, 0x001F, 5, 1, GPS_ERROR_OFFSET, big_endian);
    push_u32(&mut bytes, 0, big_endian);

    bytes.extend_from_slice(b"2024:06:15\0");
    bytes.push(0);
    for (numerator, denominator) in [(10, 1), (20, 1), (30_125, 1_000)] {
        push_u32(&mut bytes, numerator, big_endian);
        push_u32(&mut bytes, denominator, big_endian);
    }
    push_u32(&mut bytes, 12_345, big_endian);
    push_u32(&mut bytes, 100, big_endian);
    push_u32(&mut bytes, 3, big_endian);
    push_u32(&mut bytes, 2, big_endian);

    debug_assert_eq!(bytes.len(), 144);
    bytes
}

/// Structurally valid `ftyp`-only HEIC with no `meta` box, so it carries no
/// EXIF item. Readers must treat it as media with no source GPS, not an error.
#[cfg(feature = "xmp")]
pub fn heif_ftyp_without_meta_bytes() -> Vec<u8> {
    vec![
        0, 0, 0, 24, b'f', b't', b'y', b'p', b'h', b'e', b'i', b'c', 0, 0, 0, 0, b'h', b'e', b'i',
        b'c', b'm', b'i', b'f', b'1',
    ]
}

/// Asserts the standard source GPS time and speed, without accuracy when
/// native coordinates are absent or differ from the provider location.
#[cfg(feature = "xmp")]
pub fn assert_source_gps_in_xmp(meta: &xmp_toolkit::XmpMeta) {
    use xmp_toolkit::xmp_ns;

    let text = |name: &str| meta.property(xmp_ns::EXIF, name).expect(name).value;
    assert_eq!(text("GPSTimeStamp"), SOURCE_GPS_DATETIME);
    assert_eq!(text("GPSSpeedRef"), SOURCE_GPS_SPEED_REF);
    assert_eq!(text("GPSSpeed"), SOURCE_GPS_SPEED);
    assert!(!meta.contains_property(xmp_ns::EXIF, "GPSHPositioningError"));
    assert!(
        !meta.contains_property("http://cipa.jp/exif/1.0/", "GPSHPositioningError"),
        "Apple-compatible sidecars must not duplicate GPSHPositioningError in exifEX"
    );
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

    #[tokio::test]
    async fn mock_photos_flow_queues_common_cloudkit_shapes() {
        let mock = MockPhotosFlow::new()
            .album_count(7)
            .changes_database("db-token", &[("PrimarySync", "zone-token")], true)
            .error("transport failure")
            .build();

        let count = mock
            .post("https://example.com/count", "{}".to_owned(), &[])
            .await
            .expect("count response");
        assert_eq!(
            count["batch"][0]["records"][0]["fields"]["itemCount"]["value"],
            7
        );

        let changes = mock
            .post("https://example.com/changes/database", "{}".to_owned(), &[])
            .await
            .expect("changes database response");
        assert_eq!(changes["syncToken"], "db-token");
        assert_eq!(changes["zones"][0]["zoneID"]["zoneName"], "PrimarySync");
        assert_eq!(changes["zones"][0]["syncToken"], "zone-token");
        assert!(changes["moreComing"].as_bool().unwrap_or(false));

        let err = mock
            .post("https://example.com/fail", "{}".to_owned(), &[])
            .await
            .expect_err("queued transport error");
        assert!(err.to_string().contains("transport failure"));
    }
}
