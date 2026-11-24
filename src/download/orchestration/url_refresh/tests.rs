use super::super::test_support::album_with_session_and_retry_config;
use crate::download::pipeline;
use crate::icloud::photos::PhotoAsset;
use crate::retry::RetryConfig;
use crate::test_helpers::TracingCapture;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Instant;

use reqwest::Client;
use rustc_hash::FxHashSet;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::time::Duration;
use tokio_util::sync::CancellationToken;

use crate::commands::{AlbumPass, PassKind};
use crate::icloud::photos::PhotosSession;
use crate::state::SqliteStateDb;

use super::super::dispatch::download_photos_with_sync;
use super::super::incremental::download_photos_incremental_collecting_inner;
use super::super::models::{DownloadControls, DownloadOutcome, SyncMode};
use super::super::test_support::{
    album_with_session, changes_zone_response, incremental_photo_records_with_url,
    relation_delta_record, seed_complete_album_snapshot, test_config,
};

#[derive(Clone, Debug)]
struct SplitChangesZoneSession {
    delta_responses: Arc<std::sync::Mutex<std::collections::VecDeque<Value>>>,
    hydrate_responses: Arc<std::sync::Mutex<std::collections::VecDeque<Value>>>,
}

impl SplitChangesZoneSession {
    fn new(delta_responses: Vec<Value>, hydrate_responses: Vec<Value>) -> Self {
        Self {
            delta_responses: Arc::new(std::sync::Mutex::new(delta_responses.into())),
            hydrate_responses: Arc::new(std::sync::Mutex::new(hydrate_responses.into())),
        }
    }
}

#[async_trait::async_trait]
impl PhotosSession for SplitChangesZoneSession {
    async fn post(
        &self,
        url: &str,
        body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        if url.contains("/records/lookup?") {
            let responses = self.hydrate_responses.lock().expect("poisoned");
            let current = responses.front().expect("fresh lookup fixture");
            return Ok(json!({"records": current["zones"][0]["records"]}));
        }
        let responses = if body.contains("\"syncToken\"") {
            &self.delta_responses
        } else {
            &self.hydrate_responses
        };
        responses
            .lock()
            .expect("poisoned")
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("unexpected extra changes/zone call"))
    }

    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

#[derive(Clone, Debug)]
struct AssetOnlyExpiredUrlSession {
    delta_records: Arc<Vec<Value>>,
    lookup_records: Arc<Vec<Value>>,
    hydration_records: Arc<Vec<Value>>,
    lookup_calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl PhotosSession for AssetOnlyExpiredUrlSession {
    async fn post(
        &self,
        url: &str,
        body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        if url.contains("/records/lookup?") {
            let call = self.lookup_calls.fetch_add(1, Ordering::SeqCst);
            return Ok(
                json!({"records": if call < 2 { self.lookup_records.as_ref() } else { self.hydration_records.as_ref() }}),
            );
        }
        if url.contains("/changes/zone?") {
            let request: Value = serde_json::from_str(&body)?;
            let has_sync_token = request["zones"]
                .as_array()
                .and_then(|zones| zones.first())
                .and_then(|zone| zone.get("syncToken"))
                .is_some();
            let records = if has_sync_token {
                self.delta_records.as_ref().clone()
            } else {
                self.hydration_records.as_ref().clone()
            };
            return Ok(changes_zone_response(records, "zone-token-next"));
        }
        Ok(json!({"records": []}))
    }

    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

#[derive(Clone, Debug)]
struct PendingRetryExpiredUrlSession {
    expired: Arc<AtomicBool>,
    delta_records: Option<Vec<Value>>,
    expired_records: Arc<Vec<Value>>,
    fresh_records: Arc<Vec<Value>>,
}

impl PendingRetryExpiredUrlSession {
    fn new(
        mut expired_records: Vec<Value>,
        mut fresh_records: Vec<Value>,
        expired: Arc<AtomicBool>,
    ) -> Self {
        for record in expired_records.iter_mut().chain(fresh_records.iter_mut()) {
            if let Some(reference) = record.pointer_mut("/fields/masterRef/value") {
                reference["zoneID"]["ownerRecordName"] = json!("_defaultOwner");
            }
        }
        Self {
            expired,
            delta_records: None,
            expired_records: Arc::new(expired_records),
            fresh_records: Arc::new(fresh_records),
        }
    }
}

#[async_trait::async_trait]
impl PhotosSession for PendingRetryExpiredUrlSession {
    async fn post(
        &self,
        url: &str,
        body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        let records = if self.expired.load(Ordering::SeqCst) {
            &self.fresh_records
        } else {
            &self.expired_records
        };
        if url.contains("/changes/zone?") {
            return Ok(changes_zone_response(
                self.delta_records
                    .clone()
                    .unwrap_or_else(|| records.as_ref().clone()),
                "zone-token-next",
            ));
        }
        super::super::test_support::SelectedMediaSession(records.clone())
            .post(url, body, &[])
            .await
    }

    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

#[tokio::test]
async fn pending_retry_expired_url_hydrates_current_records() {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    let server = crate::start_wiremock_or_skip!();
    let expired = Arc::new(AtomicBool::new(false));
    let expired_response = expired.clone();
    let recovery = Arc::new(AtomicBool::new(false));
    let recovery_response = recovery.clone();
    let failed_transfers = Arc::new(AtomicUsize::new(0));
    let failed_response = failed_transfers.clone();
    let expired_transfers = Arc::new(AtomicUsize::new(0));
    let expired_count = expired_transfers.clone();
    Mock::given(method("GET"))
        .and(path("/expired-pending.jpg"))
        .respond_with(move |_: &wiremock::Request| {
            if !recovery_response.load(Ordering::SeqCst) {
                failed_response.fetch_add(1, Ordering::SeqCst);
                return ResponseTemplate::new(404);
            }
            expired_count.fetch_add(1, Ordering::SeqCst);
            expired_response.store(true, Ordering::SeqCst);
            ResponseTemplate::new(410)
        })
        .mount(&server)
        .await;

    let body = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];
    let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&body));
    Mock::given(method("GET"))
        .and(path("/fresh-pending.jpg"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(body.clone())
                .insert_header("content-type", "image/jpeg"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let dir = TempDir::new().expect("temp dir");
    let database = dir.path().join("state.db");
    let db = super::super::test_support::open_fixture_db(&database).await;
    let sentinel = dir.path().join("existing.jpg");
    std::fs::write(&sentinel, b"existing synthetic media").unwrap();

    let expired_url = format!("{}/expired-pending.jpg", server.uri());
    let fresh_url = format!("{}/fresh-pending.jpg", server.uri());
    let mut expired_records = incremental_photo_records_with_url(
        "PENDING_EXPIRED_URL",
        "pending-expired-url.jpg",
        &expired_url,
        8,
    );
    expired_records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
        json!(checksum.clone());
    let mut fresh_records = incremental_photo_records_with_url(
        "PENDING_EXPIRED_URL",
        "pending-expired-url.jpg",
        &fresh_url,
        8,
    );
    fresh_records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(checksum);
    let mut session = PendingRetryExpiredUrlSession::new(expired_records, fresh_records, expired);
    session.delta_records = Some(Vec::new());
    let passes = vec![super::super::test_support::selected_pass_with_session(
        &db,
        "",
        Box::new(session.clone()),
    )];

    let mut config = test_config();
    config.directory = Arc::from(dir.path());
    config.state_db = Some(db.clone());
    config.sync_mode = SyncMode::Incremental {
        zone_sync_token: "zone-token-prev".to_string(),
    };
    let mut initial = config.clone();
    initial.sync_mode = SyncMode::Full;
    let failed = download_photos_with_sync(
        &Client::new(),
        &passes,
        Arc::new(initial),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(
        matches!(failed.outcome, DownloadOutcome::PartialFailure { .. }),
        "{failed:?}"
    );
    assert_eq!(failed.sync_token, None);
    assert_eq!(failed.stats.downloaded, 0);
    assert_eq!(
        failed_transfers.load(Ordering::SeqCst),
        2,
        "initial transfer and bounded same-cycle recovery both fail"
    );
    assert_eq!(expired_transfers.load(Ordering::SeqCst), 0);
    config.state_db = None;
    drop(passes);
    drop(db);
    let db = super::super::test_support::open_fixture_db(&database).await;
    config.state_db = Some(db.clone());
    let passes = vec![super::super::test_support::selected_pass_with_session(
        &db,
        "",
        Box::new(session),
    )];
    recovery.store(true, Ordering::SeqCst);

    let result = download_photos_with_sync(
        &Client::new(),
        &passes,
        Arc::new(config.clone()),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("expired pending retry URL should hydrate and retry");

    assert!(
        matches!(result.outcome, DownloadOutcome::Success),
        "result: {result:?}"
    );
    assert!(!result.stats.interrupted);
    assert_eq!(result.sync_token.as_deref(), Some("zone-token-next"));
    let summary = db.get_summary().await.expect("summary");
    assert_eq!(summary.downloaded, 1);
    assert_eq!(summary.pending, 0);
    assert_eq!(summary.failed, 0);
    assert_eq!(expired_transfers.load(Ordering::SeqCst), 1);
    let rows = db.get_downloaded_page(0, 10).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id.as_ref(), "asset-PENDING_EXPIRED_URL");
    assert_eq!(
        std::fs::read(rows[0].local_path.as_ref().unwrap()).unwrap(),
        body
    );
    let quiet = download_photos_with_sync(
        &Client::new(),
        &passes,
        Arc::new(config),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(
        matches!(quiet.outcome, DownloadOutcome::Success),
        "{quiet:?}"
    );
    assert_eq!(quiet.stats.downloaded, 0);
    assert_eq!(quiet.sync_token.as_deref(), Some("zone-token-next"));
    assert_eq!(expired_transfers.load(Ordering::SeqCst), 1);
    assert_eq!(
        std::fs::read(rows[0].local_path.as_ref().unwrap()).unwrap(),
        body
    );
    assert_eq!(
        std::fs::read(sentinel).unwrap(),
        b"existing synthetic media"
    );
}

#[tokio::test]
async fn incremental_expired_urls_are_refreshed_and_retried_same_cycle() {
    let (capture, _guard) = TracingCapture::install();
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    let server = crate::start_wiremock_or_skip!();
    let expired = Arc::new(AtomicBool::new(false));
    let expired_response = expired.clone();
    Mock::given(method("GET"))
        .and(path("/expired-url-1.jpg"))
        .respond_with(move |_: &wiremock::Request| {
            expired_response.store(true, Ordering::SeqCst);
            ResponseTemplate::new(410)
        })
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/expired-url-2.jpg"))
        .respond_with(ResponseTemplate::new(410))
        .mount(&server)
        .await;

    let body = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];
    let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&body));
    Mock::given(method("GET"))
        .and(path("/fresh-url-1.jpg"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(body.clone())
                .insert_header("content-type", "image/jpeg"),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/fresh-url-2.jpg"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(body.clone())
                .insert_header("content-type", "image/jpeg"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let stale_url_1 = format!("{}/expired-url-1.jpg", server.uri());
    let stale_url_2 = format!("{}/expired-url-2.jpg", server.uri());
    let fresh_url_1 = format!("{}/fresh-url-1.jpg", server.uri());
    let fresh_url_2 = format!("{}/fresh-url-2.jpg", server.uri());
    let mut stale_records =
        incremental_photo_records_with_url("URL_AGED_ONE", "url-aged-1.jpg", &stale_url_1, 8);
    stale_records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(checksum.clone());
    let mut second_stale =
        incremental_photo_records_with_url("URL_AGED_TWO", "url-aged-2.jpg", &stale_url_2, 8);
    second_stale[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(checksum.clone());
    stale_records.extend(second_stale);

    let mut fresh_records =
        incremental_photo_records_with_url("URL_AGED_ONE", "url-aged-1.jpg", &fresh_url_1, 8);
    fresh_records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(checksum.clone());
    let mut second_fresh =
        incremental_photo_records_with_url("URL_AGED_TWO", "url-aged-2.jpg", &fresh_url_2, 8);
    second_fresh[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(checksum);
    fresh_records.extend(second_fresh);
    for child in ["asset-URL_AGED_ONE", "asset-URL_AGED_TWO"] {
        stale_records.push(relation_delta_record("container-vacation", child));
        fresh_records.push(relation_delta_record("container-vacation", child));
    }

    let dir = TempDir::new().expect("temp dir");
    let sentinel = dir.path().join("existing.jpg");
    std::fs::write(&sentinel, b"existing synthetic media").unwrap();
    let db = super::super::test_support::open_fixture_db(&dir.path().join("state.db")).await;
    seed_complete_album_snapshot(&db, "container-vacation", "Vacation", &[]).await;
    let album_session =
        PendingRetryExpiredUrlSession::new(stale_records, fresh_records, expired.clone());
    let make_passes = |session: Box<dyn PhotosSession>| {
        let unfiled = session.clone_box();
        let mut album = super::super::test_support::selected_pass_with_session(
            &db,
            "container-vacation",
            session,
        );
        album.album.name = Arc::from("Vacation");
        vec![
            album,
            super::super::test_support::selected_pass_with_session(&db, "", unfiled),
        ]
    };
    let passes = make_passes(Box::new(album_session.clone()));

    let mut config = test_config();
    config.directory = Arc::from(dir.path());
    config.concurrent_downloads = 1;
    config.state_db = Some(db.clone());
    config.sync_mode = SyncMode::Incremental {
        zone_sync_token: "zone-token-prev".to_string(),
    };
    let mut initial = config.clone();
    initial.sync_mode = SyncMode::Full;
    let initialized = download_photos_with_sync(
        &Client::new(),
        &make_passes(Box::new(super::super::test_support::SelectedMediaSession(
            Arc::new(Vec::new()),
        ))),
        Arc::new(initial),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(initialized.sync_token.as_deref(), Some("fixture-token"));

    let result = download_photos_with_sync(
        &Client::new(),
        &passes,
        Arc::new(config.clone()),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("expired incremental URLs should be refreshed and retried");

    assert!(
        expired.load(Ordering::SeqCst),
        "exercise a real expired transfer before refreshing"
    );
    assert!(
        matches!(result.outcome, DownloadOutcome::Success),
        "{result:?}\n{:?}",
        capture.events()
    );
    assert_eq!(result.stats.downloaded, 2);
    assert_eq!(result.stats.failed, 0);
    assert!(
        !result.stats.interrupted,
        "a recovered expired URL should not leave the cycle interrupted"
    );
    assert_eq!(result.sync_token.as_deref(), Some("zone-token-next"));
    let summary = db.get_summary().await.expect("summary");
    assert_eq!(summary.downloaded, 2);
    assert_eq!(summary.failed, 0);
    let rows = db.get_downloaded_page(0, 10).await.unwrap();
    assert_eq!(rows.len(), 2);
    for row in &rows {
        assert_eq!(
            std::fs::read(row.local_path.as_ref().unwrap()).unwrap(),
            body
        );
    }
    let quiet_session = PendingRetryExpiredUrlSession {
        delta_records: Some(Vec::new()),
        ..album_session
    };
    let mut quiet_config = config;
    quiet_config.sync_mode = SyncMode::Incremental {
        zone_sync_token: "zone-token-next".to_string(),
    };
    let quiet = download_photos_with_sync(
        &Client::new(),
        &make_passes(Box::new(quiet_session)),
        Arc::new(quiet_config),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(
        matches!(quiet.outcome, DownloadOutcome::Success),
        "{quiet:?}"
    );
    assert_eq!(quiet.stats.downloaded, 0);
    assert_eq!(quiet.sync_token.as_deref(), Some("zone-token-next"));
    for row in &rows {
        assert_eq!(
            std::fs::read(row.local_path.as_ref().unwrap()).unwrap(),
            body
        );
    }
    assert_eq!(
        std::fs::read(sentinel).unwrap(),
        b"existing synthetic media"
    );
}

#[tokio::test]
async fn incremental_preflight_refreshes_aged_urls_before_first_download() {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    let server = crate::start_wiremock_or_skip!();
    Mock::given(method("GET"))
        .and(path("/expired-before-download.jpg"))
        .respond_with(ResponseTemplate::new(410))
        .expect(0)
        .mount(&server)
        .await;

    let body = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];
    let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&body));
    Mock::given(method("GET"))
        .and(path("/fresh-before-download.jpg"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(body)
                .insert_header("content-type", "image/jpeg"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let stale_url = format!("{}/expired-before-download.jpg", server.uri());
    let fresh_url = format!("{}/fresh-before-download.jpg", server.uri());
    let mut stale_records =
        incremental_photo_records_with_url("URL_PREFLIGHT", "preflight.jpg", &stale_url, 8);
    stale_records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(checksum.clone());
    let mut fresh_records =
        incremental_photo_records_with_url("URL_PREFLIGHT", "preflight.jpg", &fresh_url, 8);
    fresh_records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(checksum);

    let session = SplitChangesZoneSession::new(
        vec![changes_zone_response(stale_records, "zone-token-next")],
        vec![changes_zone_response(fresh_records, "zone-token-next")],
    );
    let passes = vec![AlbumPass {
        kind: PassKind::Unfiled,
        album: album_with_session("PrimarySync", "", Box::new(session)),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];

    let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
    let mut config = test_config();
    let dir = TempDir::new().expect("temp dir");
    config.directory = Arc::from(dir.path());
    config.concurrent_downloads = 1;
    config.state_db = Some(db.clone());

    let result = download_photos_incremental_collecting_inner(
        &Client::new(),
        &passes,
        &Arc::new(config),
        "zone-token-prev",
        DownloadControls::download_hidden(),
        CancellationToken::new(),
        Duration::ZERO,
    )
    .await
    .expect("preflight refresh should replace aged URLs before first download");

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert_eq!(result.stats.downloaded, 1);
    assert_eq!(result.stats.failed, 0);
    assert_eq!(result.sync_token.as_deref(), Some("zone-token-next"));
    let summary = db.get_summary().await.expect("summary");
    assert_eq!(summary.downloaded, 1);
    assert_eq!(summary.failed, 0);
}

#[tokio::test]
async fn incremental_asset_only_expired_url_retry_preserves_child_state_identity() {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    let server = crate::start_wiremock_or_skip!();
    Mock::given(method("GET"))
        .and(path("/expired-asset-only.jpg"))
        .respond_with(ResponseTemplate::new(410))
        .expect(1)
        .mount(&server)
        .await;

    let body = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];
    let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&body));
    Mock::given(method("GET"))
        .and(path("/fresh-asset-only.jpg"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(body)
                .insert_header("content-type", "image/jpeg"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let stale_url = format!("{}/expired-asset-only.jpg", server.uri());
    let fresh_url = format!("{}/fresh-asset-only.jpg", server.uri());
    let mut stale_records =
        incremental_photo_records_with_url("URL_ASSET_ONLY", "asset-only.jpg", &stale_url, 8);
    stale_records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(checksum.clone());
    let mut fresh_records =
        incremental_photo_records_with_url("URL_ASSET_ONLY", "asset-only.jpg", &fresh_url, 8);
    fresh_records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(checksum);

    let asset_only_delta = stale_records[1].clone();
    let session = AssetOnlyExpiredUrlSession {
        delta_records: Arc::new(vec![asset_only_delta]),
        lookup_records: Arc::new(stale_records),
        hydration_records: Arc::new(fresh_records),
        lookup_calls: Arc::new(AtomicUsize::new(0)),
    };
    let passes = vec![AlbumPass {
        kind: PassKind::Unfiled,
        album: album_with_session("PrimarySync", "", Box::new(session)),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];

    let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
    let mut config = test_config();
    let dir = TempDir::new().expect("temp dir");
    config.directory = Arc::from(dir.path());
    config.concurrent_downloads = 1;
    // Enter the actual collecting owner directly; recent selection now owns a separate inventory.
    config.state_db = Some(db.clone());
    config.sync_mode = SyncMode::Incremental {
        zone_sync_token: "zone-token-prev".to_string(),
    };

    let result = download_photos_incremental_collecting_inner(
        &Client::new(),
        &passes,
        &Arc::new(config),
        "zone-token-prev",
        DownloadControls::download_hidden(),
        CancellationToken::new(),
        super::INCREMENTAL_PREFLIGHT_URL_REFRESH_AFTER,
    )
    .await
    .expect("asset-only retry should keep its child state identity");

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert_eq!(result.stats.downloaded, 1);
    assert_eq!(result.stats.failed, 0);
    assert_eq!(result.sync_token.as_deref(), Some("zone-token-next"));
    let downloaded = db.get_downloaded_page(0, 10).await.expect("downloaded row");
    assert_eq!(downloaded.len(), 1);
    assert_eq!(downloaded[0].id.as_ref(), "asset-URL_ASSET_ONLY");
}

#[tokio::test]
async fn incremental_expired_url_retry_hydrates_instead_of_replaying_stale_delta() {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    let server = crate::start_wiremock_or_skip!();
    Mock::given(method("GET"))
        .and(path("/expired-replay.jpg"))
        .respond_with(ResponseTemplate::new(410))
        .expect(1)
        .mount(&server)
        .await;

    let body = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];
    let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&body));
    Mock::given(method("GET"))
        .and(path("/fresh-hydrated-replay.jpg"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(body)
                .insert_header("content-type", "image/jpeg"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let stale_url = format!("{}/expired-replay.jpg", server.uri());
    let fresh_url = format!("{}/fresh-hydrated-replay.jpg", server.uri());
    let mut stale_records =
        incremental_photo_records_with_url("URL_REPLAY_STALE", "replay-stale.jpg", &stale_url, 8);
    stale_records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(checksum.clone());
    let mut fresh_records =
        incremental_photo_records_with_url("URL_REPLAY_STALE", "replay-stale.jpg", &fresh_url, 8);
    fresh_records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(checksum);

    let session = SplitChangesZoneSession::new(
        vec![
            changes_zone_response(stale_records.clone(), "zone-token-next"),
            changes_zone_response(stale_records, "zone-token-next"),
        ],
        vec![changes_zone_response(fresh_records, "zone-token-next")],
    );
    let passes = vec![AlbumPass {
        kind: PassKind::Unfiled,
        album: album_with_session("PrimarySync", "", Box::new(session)),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];

    let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
    let mut config = test_config();
    let dir = TempDir::new().expect("temp dir");
    config.directory = Arc::from(dir.path());
    config.concurrent_downloads = 1;
    // Enter the actual collecting owner directly; recent selection now owns a separate inventory.
    config.state_db = Some(db.clone());
    config.sync_mode = SyncMode::Incremental {
        zone_sync_token: "zone-token-prev".to_string(),
    };

    let result = download_photos_incremental_collecting_inner(
        &Client::new(),
        &passes,
        &Arc::new(config),
        "zone-token-prev",
        DownloadControls::download_hidden(),
        CancellationToken::new(),
        super::INCREMENTAL_PREFLIGHT_URL_REFRESH_AFTER,
    )
    .await
    .expect("expired URL retry should hydrate current records, not replay stale delta URLs");

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert_eq!(result.stats.downloaded, 1);
    assert_eq!(result.stats.failed, 0);
    assert!(
        !result.stats.interrupted,
        "hydrated expired URL recovery should not leave the cycle interrupted"
    );
    assert_eq!(result.sync_token.as_deref(), Some("zone-token-next"));
    let summary = db.get_summary().await.expect("summary");
    assert_eq!(summary.downloaded, 1);
    assert_eq!(summary.failed, 0);
}

#[tokio::test]
async fn incremental_expired_url_retry_hydrates_relation_only_album_assets() {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    let server = crate::start_wiremock_or_skip!();
    let expired = Arc::new(AtomicBool::new(false));
    let expired_response = expired.clone();
    Mock::given(method("GET"))
        .and(path("/expired-hydrated.jpg"))
        .respond_with(move |_: &wiremock::Request| {
            expired_response.store(true, Ordering::SeqCst);
            ResponseTemplate::new(410)
        })
        .expect(1)
        .mount(&server)
        .await;

    let body = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];
    let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&body));
    Mock::given(method("GET"))
        .and(path("/fresh-hydrated.jpg"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(body.clone())
                .insert_header("content-type", "image/jpeg"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let stale_url = format!("{}/expired-hydrated.jpg", server.uri());
    let fresh_url = format!("{}/fresh-hydrated.jpg", server.uri());
    let mut stale_records =
        incremental_photo_records_with_url("MASTER_HYDRATED", "hydrated.jpg", &stale_url, 8);
    stale_records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(checksum.clone());
    let mut fresh_records =
        incremental_photo_records_with_url("MASTER_HYDRATED", "hydrated.jpg", &fresh_url, 8);
    fresh_records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(checksum);
    let relation_records = || {
        vec![relation_delta_record(
            "container-vacation",
            "asset-MASTER_HYDRATED",
        )]
    };

    let dir = TempDir::new().expect("temp dir");
    let database = dir.path().join("state.db");
    let sentinel = dir.path().join("existing.jpg");
    std::fs::write(&sentinel, b"existing synthetic media").unwrap();
    let db = super::super::test_support::open_fixture_db(&database).await;
    seed_complete_album_snapshot(&db, "container-vacation", "Vacation", &[]).await;
    // Only a real expired transfer makes the provider's fresh URL available.
    let lookup_calls = Arc::new(AtomicUsize::new(0));
    #[derive(Clone)]
    struct RelationUrlSession {
        current: PendingRetryExpiredUrlSession,
        delta: Vec<Value>,
        lookups: Arc<AtomicUsize>,
    }
    #[async_trait::async_trait]
    impl PhotosSession for RelationUrlSession {
        async fn post(
            &self,
            url: &str,
            body: String,
            headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            if url.contains("/changes/zone?") {
                return Ok(changes_zone_response(self.delta.clone(), "zone-token-next"));
            }
            if url.contains("/records/lookup?") {
                self.lookups.fetch_add(1, Ordering::SeqCst);
            }
            self.current.post(url, body, headers).await
        }
        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(self.clone())
        }
    }
    let album_session = RelationUrlSession {
        current: PendingRetryExpiredUrlSession::new(stale_records, fresh_records, expired),
        delta: relation_records(),
        lookups: lookup_calls.clone(),
    };
    let pass_for = |db: &Arc<SqliteStateDb>, session: Box<dyn PhotosSession>| {
        let mut pass = super::super::test_support::selected_pass_with_session(
            db,
            "container-vacation",
            session,
        );
        pass.album.name = Arc::from("Vacation");
        pass
    };
    let passes = vec![pass_for(&db, Box::new(album_session.clone()))];

    let mut config = test_config();
    config.directory = Arc::from(dir.path());
    config.concurrent_downloads = 1;
    config.folder_structure_albums = Arc::from("{album}");
    config.state_db = Some(db.clone());
    config.sync_mode = SyncMode::Incremental {
        zone_sync_token: "zone-token-prev".to_string(),
    };
    let mut initial = config.clone();
    initial.sync_mode = SyncMode::Full;
    let initialized = download_photos_with_sync(
        &Client::new(),
        &[pass_for(
            &db,
            Box::new(super::super::test_support::SelectedMediaSession(Arc::new(
                Vec::new(),
            ))),
        )],
        Arc::new(initial),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(initialized.sync_token.as_deref(), Some("fixture-token"));

    let result = download_photos_with_sync(
        &Client::new(),
        &passes,
        Arc::new(config.clone()),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("relation-only incremental retry should hydrate fresh URLs");

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert_eq!(result.stats.downloaded, 1);
    assert_eq!(result.stats.failed, 0);
    assert!(
        !result.stats.interrupted,
        "relation-only expired URL recovery should not leave the cycle interrupted"
    );
    assert_eq!(result.sync_token.as_deref(), Some("zone-token-next"));
    let summary = db.get_summary().await.expect("summary");
    assert_eq!(summary.downloaded, 1);
    assert_eq!(summary.failed, 0);
    assert_eq!(
        lookup_calls.load(Ordering::SeqCst),
        5,
        "relation hydration and expired URL refresh discover the master; selection confirms the observed pair in one request"
    );
    config.state_db = None;
    drop(passes);
    drop(db);
    let reopened = super::super::test_support::open_fixture_db(&database).await;
    let rows = reopened.get_downloaded_page(0, 10).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id.as_ref(), "asset-MASTER_HYDRATED");
    assert_eq!(
        std::fs::read(rows[0].local_path.as_ref().unwrap()).unwrap(),
        body
    );
    let passes = vec![pass_for(&reopened, Box::new(album_session.clone()))];
    config.state_db = Some(reopened.clone());
    let replayed_relation = download_photos_with_sync(
        &Client::new(),
        &passes,
        Arc::new(config.clone()),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(matches!(
        replayed_relation.outcome,
        DownloadOutcome::Success
    ));
    assert_eq!(replayed_relation.stats.downloaded, 0);
    assert_eq!(
        lookup_calls.load(Ordering::SeqCst),
        8,
        "relation hydration and current selection confirmation each resolve the pair without downloading again"
    );
    let quiet_session = RelationUrlSession {
        delta: Vec::new(),
        ..album_session
    };
    let quiet_passes = vec![pass_for(&reopened, Box::new(quiet_session))];
    let quiet = download_photos_with_sync(
        &Client::new(),
        &quiet_passes,
        Arc::new(config),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(matches!(quiet.outcome, DownloadOutcome::Success));
    assert_eq!(quiet.stats.downloaded, 0);
    assert_eq!(
        lookup_calls.load(Ordering::SeqCst),
        8,
        "an empty reopened feed does not repeat hydration"
    );
    assert_eq!(
        std::fs::read(rows[0].local_path.as_ref().unwrap()).unwrap(),
        body
    );
    assert_eq!(
        std::fs::read(&sentinel).unwrap(),
        b"existing synthetic media"
    );
}

#[tokio::test]
async fn full_failed_task_cleanup_is_targeted_bounded_and_durable() {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    #[derive(Clone, Debug)]
    struct LookupOnlySession {
        records: Arc<Vec<Value>>,
        lookups: Arc<AtomicUsize>,
        recovery: &'static str,
        shutdown: CancellationToken,
    }
    #[async_trait::async_trait]
    impl PhotosSession for LookupOnlySession {
        async fn post(
            &self,
            url: &str,
            _body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            assert!(
                url.contains("/records/lookup?"),
                "cleanup must not enumerate the library"
            );
            let call = self.lookups.fetch_add(1, Ordering::SeqCst);
            if self.recovery == "shutdown_lookup" {
                self.shutdown.cancel();
            }
            let status = match self.recovery {
                "auth_asset" if call == 0 => Some(401),
                "auth_master" if call == 1 => Some(421),
                "rate_asset" if call == 0 => Some(429),
                "rate_master" if call == 1 => Some(503),
                "exhausted_asset" => Some(429),
                "exhausted_master" if call >= 1 => Some(503),
                _ => None,
            };
            if let Some(status) = status {
                return Err(crate::icloud::photos::session::HttpStatusError {
                    status,
                    url: url.into(),
                    body: Some("private-provider-response".into()),
                    retry_after: None,
                }
                .into());
            }
            Ok(json!({"records": self.records.as_ref()}))
        }
        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(self.clone())
        }
    }

    for recovery in [
        "transfer_failure",
        "fresh",
        "album_fresh",
        "album_unfiled",
        "album_cancelled",
        "album_changed_checksum",
        "album_changed_size",
        "expired",
        "missing",
        "auth_asset",
        "auth_master",
        "rate_asset",
        "rate_master",
        "exhausted_asset",
        "exhausted_master",
        "shutdown_lookup",
    ] {
        let (capture, _guard) = TracingCapture::install();
        let downloads = matches!(
            recovery,
            "transfer_failure"
                | "fresh"
                | "album_fresh"
                | "album_unfiled"
                | "rate_asset"
                | "rate_master"
        );
        let shutdown = CancellationToken::new();
        let server = crate::start_wiremock_or_skip!();
        let body = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];
        Mock::given(method("GET"))
            .and(path("/old.jpg"))
            .respond_with(ResponseTemplate::new(if recovery == "transfer_failure" {
                503
            } else {
                410
            }))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/new.jpg"))
            .respond_with(if downloads {
                ResponseTemplate::new(200).set_body_bytes(body.clone())
            } else {
                ResponseTemplate::new(410)
            })
            .expect(if downloads || recovery == "expired" {
                1
            } else {
                0
            })
            .mount(&server)
            .await;
        let mut old = incremental_photo_records_with_url(
            "FULL_EXPIRED",
            "full-expired.jpg",
            &format!("{}/old.jpg", server.uri()),
            body.len() as u64,
        );
        old[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
            json!(base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&body)));
        let mut fresh = old.clone();
        fresh[0]["fields"]["resOriginalRes"]["value"]["downloadURL"] =
            json!(format!("{}/new.jpg", server.uri()));
        if recovery == "album_changed_checksum" {
            fresh[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!("changed");
        }
        if recovery == "album_changed_size" {
            fresh[0]["fields"]["resOriginalRes"]["value"]["size"] = json!(body.len() + 1);
        }
        let old_asset = PhotoAsset::new(old[0].clone(), old[1].clone());
        let fresh_asset = PhotoAsset::new(fresh[0].clone(), fresh[1].clone());
        let lookups = Arc::new(AtomicUsize::new(0));
        let mut passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: album_with_session_and_retry_config(
                "PrimarySync",
                "",
                None,
                RetryConfig {
                    max_retries: 1,
                    base_delay_secs: 0,
                    max_delay_secs: 0,
                },
                Box::new(LookupOnlySession {
                    records: Arc::new(if recovery == "missing" {
                        Vec::new()
                    } else {
                        fresh
                    }),
                    lookups: Arc::clone(&lookups),
                    recovery,
                    shutdown: shutdown.clone(),
                }),
            ),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];
        if recovery.starts_with("album_") {
            let mut album = passes[0].clone();
            album.kind = PassKind::Album;
            album.album.name = "Album A".into();
            album.album = album.album.with_selector_components(vec!["Album A".into()]);
            passes.insert(0, album);
        }
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("state.db");
        let db = Arc::new(SqliteStateDb::open(&db_path).await.unwrap());
        let mut config = test_config();
        config.retry.max_retries = 0;
        config.directory = Arc::from(dir.path().join("media"));
        config.state_db = Some(db.clone());
        if recovery.starts_with("album_") {
            config.folder_structure = "{album}".into();
            config.folder_structure_albums = Arc::from("{album}");
        }
        let source_pass = if recovery == "album_unfiled" {
            passes.last().unwrap()
        } else {
            &passes[0]
        };
        let stream_config = Arc::new(config.with_pass(source_pass));
        let mut config = Arc::new(config);
        let client = Client::new();
        let controls = DownloadControls::download_hidden();
        let streaming = pipeline::stream_and_download_from_stream(
            &client,
            futures_util::stream::iter(vec![Ok(old_asset)]),
            &stream_config,
            controls,
            1,
            CancellationToken::new(),
            pipeline::StreamRuntime::new(None, None),
        )
        .await
        .unwrap();
        assert_eq!(streaming.url_expired, recovery != "transfer_failure");
        if recovery == "transfer_failure" {
            Arc::make_mut(&mut config).folder_structure = "changed-template".into();
        }
        let original_path = streaming.failed[0].download_path.clone();
        if recovery == "album_cancelled" {
            shutdown.cancel();
        }
        let (outcome, stats) = pipeline::build_download_outcome(
            &client,
            &passes,
            &config,
            controls,
            streaming,
            Instant::now(),
            shutdown.clone(),
        )
        .await
        .unwrap();
        assert!(
            if downloads {
                matches!(outcome, DownloadOutcome::Success)
            } else if recovery.starts_with("auth_") {
                matches!(outcome, DownloadOutcome::SessionExpired { .. })
            } else {
                matches!(outcome, DownloadOutcome::PartialFailure { .. })
            },
            "cleanup outcome for {recovery}: {outcome:?}"
        );
        assert_eq!(stats.downloaded, usize::from(downloads));
        assert_eq!(stats.failed, usize::from(!downloads));
        assert_eq!(
            stats.interrupted,
            recovery.starts_with("auth_")
                || matches!(recovery, "shutdown_lookup" | "album_cancelled")
        );
        assert_eq!(
            stats.rate_limited,
            match recovery {
                "transfer_failure" | "rate_asset" | "rate_master" => 1,
                "exhausted_asset" | "exhausted_master" => 2,
                _ => 0,
            }
        );
        assert_eq!(
            lookups.load(Ordering::SeqCst),
            match recovery {
                "album_cancelled" => 0,
                "missing" | "auth_asset" | "shutdown_lookup" => 1,
                "rate_asset" | "rate_master" | "exhausted_master" => 3,
                _ => 2,
            }
        );
        if recovery.starts_with("auth_") {
            assert!(!crate::sync_cycle::should_store_sync_token(&outcome, false));
        }
        let diagnostics: Vec<_> = capture
            .events()
            .into_iter()
            .filter(|event| event.field("diagnostic") == Some("expired_url_refresh_failed"))
            .collect();
        assert_eq!(
            diagnostics.len(),
            usize::from(recovery.starts_with("auth_") || recovery.starts_with("exhausted_"))
        );
        for event in diagnostics {
            assert_eq!(event.fields.len(), 5);
            for key in [
                "message",
                "diagnostic",
                "failed_records",
                "authentication_failures",
                "rate_limit_observations",
            ] {
                assert!(event.fields.contains_key(key));
            }
            assert!(!format!("{:?}", event.fields).contains("private-provider-response"));
        }
        let reopened = SqliteStateDb::open(&db_path).await.unwrap();
        let summary = reopened.get_summary().await.unwrap();
        assert_eq!(summary.downloaded, u64::from(downloads));
        if downloads {
            let rows = reopened.get_downloaded_page(0, 10).await.unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].local_path.as_ref(), Some(&original_path));
            assert!(!config.directory.join("changed-template").exists());
            if recovery == "album_fresh" {
                assert!(original_path.starts_with(config.directory.join("Album A")));
                assert!(!config.directory.join("full-expired.jpg").exists());
            }
            if recovery == "album_unfiled" {
                assert!(!config.directory.join("Album A").exists());
            }
            assert_eq!(
                std::fs::read(rows[0].local_path.as_ref().unwrap()).unwrap(),
                body
            );
            for _ in 0..2 {
                let mut stable_config = stream_config.as_ref().clone();
                stable_config.state_db =
                    Some(Arc::new(SqliteStateDb::open(&db_path).await.unwrap()));
                let stable = pipeline::stream_and_download_from_stream(
                    &client,
                    futures_util::stream::iter(vec![Ok(fresh_asset.clone())]),
                    &Arc::new(stable_config),
                    controls,
                    1,
                    CancellationToken::new(),
                    pipeline::StreamRuntime::new(None, None),
                )
                .await
                .unwrap();
                assert_eq!(stable.downloaded, 0);
                assert!(stable.failed.is_empty());
                assert_eq!(std::fs::read(&original_path).unwrap(), body);
            }
        } else {
            assert_eq!(
                summary.pending + summary.failed,
                1,
                "unrefreshed work must survive restart"
            );
        }
        server.verify().await;
    }
}
