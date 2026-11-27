//! Shared test fixtures and builders.
//!
//! Provides ergonomic builders for commonly constructed test objects
//! and reusable mock implementations of core traits.

pub(crate) mod allocation;

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use crate::icloud::photos::session::PhotosSession;

/// A previous-engine state file at `path`: the released v0.24 schema with
/// its account header bound to `owner`, as `kei migrate-state` leaves it.
pub(crate) fn legacy_state_db(path: &Path, owner: &crate::state::db::account::AccountOwner) {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch(include_str!("../tests/data/released-v0240-schema.sql"))
        .unwrap();
    conn.execute_batch(
        "CREATE TABLE account_owner (singleton INTEGER PRIMARY KEY CHECK(singleton=1),
             version INTEGER NOT NULL, account_key TEXT NOT NULL, provider_key TEXT NOT NULL);",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO account_owner VALUES (1, 1, ?1, ?2)",
        rusqlite::params![owner.key(), owner.provider_key()],
    )
    .unwrap();
}

/// Synthetic QuickTime track metadata without an ISO FullBox prefix.
#[cfg(feature = "xmp")]
pub(crate) fn bare_track_meta_movie() -> Vec<u8> {
    let atom = |kind: &[u8; 4], body: &[u8]| {
        let mut bytes = u32::try_from(body.len() + 8)
            .unwrap()
            .to_be_bytes()
            .to_vec();
        bytes.extend_from_slice(kind);
        bytes.extend_from_slice(body);
        bytes
    };
    let mut handler = vec![0; 24];
    handler[8..12].copy_from_slice(b"mdta");
    let track_meta = atom(b"meta", &atom(b"hdlr", &handler));
    let mut movie = atom(b"mvhd", &[0; 100]);
    movie.extend(atom(b"trak", &track_meta));
    let mut original = atom(b"ftyp", b"qt  \0\0\0\0qt  ");
    original.extend(atom(b"moov", &movie));
    original.extend(atom(b"mdat", b"unchanged media"));
    original
}

/// Supported synthetic QuickTime metadata with a caller-selected user-data tail.
#[cfg(feature = "xmp")]
pub(crate) fn quicktime_user_data_movie(tail: &[u8]) -> Vec<u8> {
    let mut bytes = bare_track_meta_movie();
    let meta = bytes.windows(4).position(|value| value == b"meta").unwrap();
    bytes.splice(meta + 4..meta + 4, [0; 4]);
    for kind in [b"moov", b"trak", b"meta"] {
        let start = bytes.windows(4).position(|value| value == kind).unwrap() - 4;
        let size = u32::from_be_bytes(bytes[start..start + 4].try_into().unwrap());
        bytes[start..start + 4].copy_from_slice(&(size + 4).to_be_bytes());
    }
    let mut user_data = b"\0\0\0\x14udta\0\0\0\x0cnamekept".to_vec();
    user_data.extend_from_slice(tail);
    let size = u32::try_from(user_data.len()).unwrap();
    user_data[..4].copy_from_slice(&size.to_be_bytes());
    let movie = bytes.windows(4).position(|value| value == b"moov").unwrap() - 4;
    let size = u32::from_be_bytes(bytes[movie..movie + 4].try_into().unwrap());
    let end = movie + usize::try_from(size).unwrap();
    bytes[movie..movie + 4]
        .copy_from_slice(&(size + u32::try_from(user_data.len()).unwrap()).to_be_bytes());
    bytes.splice(end..end, user_data);
    bytes
}

/// Synthetic sparse-share shape reported in #765, with invented identifiers.
#[cfg(test)]
pub(crate) fn sparse_shared_asset_record() -> Value {
    serde_json::from_str(include_str!("../tests/data/sparse_shared_asset.json"))
        .expect("synthetic sparse-share fixture must be valid JSON")
}

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

// ── PhotoAsset builder ──────────────────────────────────────────────

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

    pub fn error(mut self, message: &str) -> Self {
        self.session = self.session.err(message);
        self
    }

    pub fn build(self) -> MockPhotosSession {
        self.session
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

/// Dedicated Linux subprocess fixture rendezvous; absent from release builds.
/// The parent uses Child::kill and verifies SIGKILL, so no Rust unwinding or
/// cooperative cancellation can complete the paused transition.
#[cfg(target_os = "linux")]
pub(crate) fn process_death_point(point: &str) {
    if std::env::var("KEI_TEST_PROCESS_DEATH_POINT").as_deref() != Ok(point) {
        return;
    }
    let root = std::path::PathBuf::from(
        std::env::var_os("KEI_TEST_PROCESS_DEATH_ROOT").expect("dedicated fixture root"),
    );
    assert_eq!(
        std::fs::read(root.join("fixture-owner")).unwrap(),
        b"kei-synthetic-process-death"
    );
    // Publish the rendezvous atomically so the parent cannot read an empty
    // marker between creation and write. This is process synchronization only.
    std::fs::write(root.join("ready.tmp"), point).unwrap();
    std::fs::rename(root.join("ready.tmp"), root.join("ready")).unwrap();
    loop {
        std::thread::park();
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn process_death_force_journal() -> bool {
    std::env::var("KEI_TEST_PROCESS_DEATH_POINT").is_ok_and(|point| {
        matches!(
            point.as_str(),
            "journal-displaced" | "journal-installed" | "journal-committed"
        )
    })
}

// A path-scoped, one-shot seam for the retained-source proof boundary. Other
// parallel fixtures cannot consume it, and the guard removes unused hooks.
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
