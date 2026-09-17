use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use reqwest::Client;
use rustc_hash::FxHashSet;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::time::Duration;
use tokio_util::sync::CancellationToken;

use crate::commands::{AlbumPass, PassKind};
use crate::download::filter::DownloadTask;
use crate::icloud::photos::PhotosSession;
use crate::state::{SqliteStateDb, VersionSizeKey};
use crate::test_helpers::MockPhotosFlow;

use super::super::dispatch::download_photos_with_sync;
use super::super::incremental::download_photos_incremental_collecting_inner;
use super::super::models::{DownloadControls, DownloadOutcome, SyncMode};
use super::super::test_support::{
    album_with_session, album_with_session_and_container, changes_zone_response,
    incremental_photo_records_with_url, mock_album, relation_delta_record, retry_test_task,
    seed_complete_album_snapshot, test_config,
};
use super::{RetryTaskKey, take_matching_retry_tasks};

#[test]
fn cleanup_retry_filter_keeps_only_exact_failed_task_keys() {
    let failed = retry_test_task("ASSET_A", VersionSizeKey::Original, "a.jpg");
    let matching_refresh = DownloadTask {
        url: "https://p01.icloud-content.com/fresh-a".into(),
        ..failed.clone()
    };
    let wrong_version = retry_test_task("ASSET_A", VersionSizeKey::Medium, "a.jpg");
    let wrong_path = retry_test_task("ASSET_A", VersionSizeKey::Original, "elsewhere/a.jpg");
    let unrelated = retry_test_task("ASSET_B", VersionSizeKey::Original, "b.jpg");
    let mut pending_keys: FxHashSet<RetryTaskKey> =
        std::iter::once(RetryTaskKey::from(&failed)).collect();
    let mut out = Vec::new();

    take_matching_retry_tasks(
        vec![
            wrong_version,
            wrong_path,
            unrelated,
            matching_refresh.clone(),
        ],
        &mut pending_keys,
        &mut out,
    );

    assert!(pending_keys.is_empty());
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].asset_id.as_ref(), "ASSET_A");
    assert_eq!(out[0].version_size, VersionSizeKey::Original);
    assert_eq!(out[0].download_path, matching_refresh.download_path);
    assert_eq!(
        out[0].url.as_ref(),
        "https://p01.icloud-content.com/fresh-a"
    );
}

#[derive(Clone, Debug)]
struct SharedChangesZoneSession {
    responses: Arc<std::sync::Mutex<std::collections::VecDeque<Value>>>,
}

impl SharedChangesZoneSession {
    fn new(responses: Vec<Value>) -> Self {
        Self {
            responses: Arc::new(std::sync::Mutex::new(responses.into())),
        }
    }
}

#[async_trait::async_trait]
impl PhotosSession for SharedChangesZoneSession {
    async fn post(
        &self,
        _url: &str,
        _body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        self.responses
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
        _url: &str,
        body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
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
            return Ok(json!({"records": self.lookup_records.as_ref().clone()}));
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
    query_calls: Arc<AtomicUsize>,
    expired_records: Arc<Vec<Value>>,
    fresh_records: Arc<Vec<Value>>,
}

impl PendingRetryExpiredUrlSession {
    fn new(expired_records: Vec<Value>, fresh_records: Vec<Value>) -> Self {
        Self {
            query_calls: Arc::new(AtomicUsize::new(0)),
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
        if url.contains("/changes/zone?") {
            let request: Value = serde_json::from_str(&body)?;
            let has_sync_token = request["zones"]
                .as_array()
                .and_then(|zones| zones.first())
                .and_then(|zone| zone.get("syncToken"))
                .and_then(Value::as_str)
                .is_some();
            let records = if has_sync_token {
                Vec::new()
            } else {
                self.fresh_records.as_ref().clone()
            };
            return Ok(changes_zone_response(records, "zone-token-next"));
        }

        if url.contains("/records/lookup?") || url.contains("/records/query?") {
            let call = self.query_calls.fetch_add(1, Ordering::SeqCst);
            return Ok(if call == 0 {
                json!({
                    "records": self.expired_records.as_ref().clone(),
                    "syncToken": "ignored-query-token"
                })
            } else {
                json!({"records": [], "syncToken": "ignored-query-token"})
            });
        }

        Ok(json!({"records": []}))
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
    Mock::given(method("GET"))
        .and(path("/expired-pending.jpg"))
        .respond_with(ResponseTemplate::new(410))
        .mount(&server)
        .await;

    let body = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];
    let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&body));
    Mock::given(method("GET"))
        .and(path("/fresh-pending.jpg"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(body)
                .insert_header("content-type", "image/jpeg"),
        )
        .mount(&server)
        .await;

    let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().expect("state db"));
    let record = crate::test_helpers::TestAssetRecord::new("PENDING_EXPIRED_URL")
        .filename("pending-expired-url.jpg")
        .checksum("ck_pending_expired_url")
        .size(1024)
        .build();
    db.upsert_seen(&record).await.expect("seed pending row");
    db.upsert_asset_master_mapping(
        "PrimarySync",
        "asset-PENDING_EXPIRED_URL",
        "PENDING_EXPIRED_URL",
    )
    .await
    .expect("seed asset/master mapping");

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
    let session = PendingRetryExpiredUrlSession::new(expired_records, fresh_records);
    let passes = vec![AlbumPass {
        kind: PassKind::Unfiled,
        album: album_with_session("PrimarySync", "", Box::new(session)),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];

    let mut config = test_config();
    let dir = TempDir::new().expect("temp dir");
    config.directory = Arc::from(dir.path());
    config.state_db = Some(db.clone());
    config.sync_mode = SyncMode::Incremental {
        zone_sync_token: "zone-token-prev".to_string(),
    };

    let result = download_photos_with_sync(
        &Client::new(),
        &passes,
        Arc::new(config),
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
}

#[tokio::test]
async fn incremental_expired_urls_are_refreshed_and_retried_same_cycle() {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    let server = crate::start_wiremock_or_skip!();
    Mock::given(method("GET"))
        .and(path("/expired-url-1.jpg"))
        .respond_with(ResponseTemplate::new(410))
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
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/fresh-url-2.jpg"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(body)
                .insert_header("content-type", "image/jpeg"),
        )
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

    let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
    seed_complete_album_snapshot(&db, "container-vacation", "Vacation", &[]).await;
    let album_session = SharedChangesZoneSession::new(vec![
        changes_zone_response(stale_records, "zone-token-next"),
        changes_zone_response(fresh_records, "zone-token-next"),
    ]);
    let passes = vec![
        AlbumPass {
            kind: PassKind::Album,
            album: album_with_session_and_container(
                "PrimarySync",
                "Vacation",
                Some("container-vacation"),
                Box::new(album_session),
            ),
            exclude_ids: Arc::new(FxHashSet::default()),
        },
        AlbumPass {
            kind: PassKind::Unfiled,
            album: mock_album("", MockPhotosFlow::new().build()),
            exclude_ids: Arc::new(FxHashSet::default()),
        },
    ];

    let mut config = test_config();
    let dir = TempDir::new().expect("temp dir");
    config.directory = Arc::from(dir.path());
    config.concurrent_downloads = 1;
    config.state_db = Some(db.clone());
    config.sync_mode = SyncMode::Incremental {
        zone_sync_token: "zone-token-prev".to_string(),
    };

    let result = download_photos_with_sync(
        &Client::new(),
        &passes,
        Arc::new(config),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("expired incremental URLs should be refreshed and retried");

    assert!(matches!(result.outcome, DownloadOutcome::Success));
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
    config.recent = Some(10);
    config.state_db = Some(db.clone());
    config.sync_mode = SyncMode::Incremental {
        zone_sync_token: "zone-token-prev".to_string(),
    };

    let result = download_photos_with_sync(
        &Client::new(),
        &passes,
        Arc::new(config),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
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
    config.recent = Some(10);
    config.state_db = Some(db.clone());
    config.sync_mode = SyncMode::Incremental {
        zone_sync_token: "zone-token-prev".to_string(),
    };

    let result = download_photos_with_sync(
        &Client::new(),
        &passes,
        Arc::new(config),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
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
    Mock::given(method("GET"))
        .and(path("/expired-hydrated.jpg"))
        .respond_with(ResponseTemplate::new(410))
        .mount(&server)
        .await;

    let body = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];
    let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&body));
    Mock::given(method("GET"))
        .and(path("/fresh-hydrated.jpg"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(body)
                .insert_header("content-type", "image/jpeg"),
        )
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

    let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
    seed_complete_album_snapshot(&db, "container-vacation", "Vacation", &[]).await;
    let album_session = SharedChangesZoneSession::new(vec![
        changes_zone_response(relation_records(), "zone-token-next"),
        changes_zone_response(stale_records, "zone-token-next"),
        changes_zone_response(fresh_records, "zone-token-next"),
    ]);
    let passes = vec![AlbumPass {
        kind: PassKind::Album,
        album: album_with_session_and_container(
            "PrimarySync",
            "Vacation",
            Some("container-vacation"),
            Box::new(album_session),
        ),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];

    let mut config = test_config();
    let dir = TempDir::new().expect("temp dir");
    config.directory = Arc::from(dir.path());
    config.concurrent_downloads = 1;
    config.folder_structure_albums = Arc::from("{album}");
    config.state_db = Some(db.clone());
    config.sync_mode = SyncMode::Incremental {
        zone_sync_token: "zone-token-prev".to_string(),
    };

    let result = download_photos_with_sync(
        &Client::new(),
        &passes,
        Arc::new(config),
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
}
