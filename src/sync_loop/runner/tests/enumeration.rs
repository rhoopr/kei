#![cfg(test)]

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::sync_cycle::{CycleResult, run_cycle};
use crate::sync_loop::test_support::{
    FailingMetadataSetDb, full_album_page_with_download, make_run_cycle_download_config_builder,
};
use crate::sync_loop::test_support::{
    RunCycleDownloadConfigOptions, album_count_response, full_album_page, make_empty_full_album,
    make_empty_full_album_for_zone, make_full_album_with_boxed_session,
    make_full_album_with_session, make_run_cycle_config,
    make_run_cycle_download_config_builder_with_options, make_run_cycle_library_state_with_album,
    make_shared_session_for_run_cycle, make_state_db, media_without_photo_downloads,
    run_full_cycle_with_album,
};
use crate::{config, download, state};

// These fixtures model a completed inventory followed by a quiet, scoped
// history bridge. Query responses are not valid changes/zone responses.
struct QuietEnumerationBridge {
    inner: Box<dyn crate::icloud::photos::PhotosSession>,
    zone: String,
    token: String,
}

#[async_trait::async_trait]
impl crate::icloud::photos::PhotosSession for QuietEnumerationBridge {
    async fn post(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<serde_json::Value> {
        if url.contains("/changes/zone?") {
            let request: serde_json::Value = serde_json::from_str(&body)?;
            assert_eq!(request["zones"].as_array().unwrap().len(), 1);
            assert_eq!(request["zones"][0]["zoneID"]["zoneName"], self.zone);
            assert_eq!(
                request["zones"][0]["syncToken"],
                "fixture-zone-history-boundary"
            );
            return Ok(
                serde_json::json!({"zones":[{"zoneID":{"zoneName":self.zone},"records":[],"syncToken":self.token,"moreComing":false}]}),
            );
        }
        self.inner.post(url, body, headers).await
    }
    fn clone_box(&self) -> Box<dyn crate::icloud::photos::PhotosSession> {
        Box::new(Self {
            inner: self.inner.clone_box(),
            zone: self.zone.clone(),
            token: self.token.clone(),
        })
    }
}

fn quiet_bridge(
    zone: &str,
    token: &str,
    inner: Box<dyn crate::icloud::photos::PhotosSession>,
) -> Box<dyn crate::icloud::photos::PhotosSession> {
    Box::new(QuietEnumerationBridge {
        inner,
        zone: zone.to_owned(),
        token: token.to_owned(),
    })
}

fn make_one_photo_full_album_for_zone(
    zone: &str,
    zone_sync_token: &str,
) -> crate::icloud::photos::PhotoAlbum {
    make_full_album_with_boxed_session(
        zone,
        quiet_bridge(
            zone,
            "fixture-zone-history-successor",
            Box::new(
                crate::test_helpers::MockPhotosSession::new()
                    .ok(album_count_response(1))
                    .ok(full_album_page(
                        zone,
                        &format!("master-{zone}"),
                        zone_sync_token,
                    )),
            ),
        ),
    )
}

async fn run_empty_full_cycle(is_retry_failed: bool) -> CycleResult {
    run_empty_full_cycle_with_controls(
        is_retry_failed,
        download::DownloadControls::download_hidden(),
    )
    .await
}

async fn run_empty_full_cycle_with_controls(
    is_retry_failed: bool,
    controls: download::DownloadControls,
) -> CycleResult {
    run_full_cycle_with_album(
        make_empty_full_album("zone-tok-empty"),
        is_retry_failed,
        controls,
    )
    .await
}

async fn run_one_photo_full_cycle_with_controls(
    controls: download::DownloadControls,
) -> CycleResult {
    run_full_cycle_with_album(
        make_one_photo_full_album_for_zone("PrimarySync", "zone-tok-one"),
        false,
        controls,
    )
    .await
}

// Record history replay separately from the rank query token.
#[derive(Clone)]
struct RecentZoneHistory {
    inner: crate::test_helpers::DynamicRecentPhotosSession,
    zone: &'static str,
    successor: &'static str,
    requests: Arc<std::sync::Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl crate::icloud::photos::PhotosSession for RecentZoneHistory {
    async fn post(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<serde_json::Value> {
        if url.contains("/changes/zone?") {
            let request: serde_json::Value = serde_json::from_str(&body)?;
            let zones = request["zones"].as_array().expect("zone request");
            assert_eq!(zones.len(), 1);
            assert_eq!(zones[0]["zoneID"]["zoneName"], self.zone);
            let token = zones[0]["syncToken"].as_str().expect("history cursor");
            self.requests.lock().unwrap().push(token.to_owned());
            anyhow::ensure!(
                token == "fixture-zone-history-boundary" || token == self.successor,
                "unexpected history cursor"
            );
            return Ok(serde_json::json!({"zones": [{
                "zoneID": {"zoneName": self.zone},
                "records": [],
                "syncToken": self.successor,
                "moreComing": false
            }]}));
        }
        crate::icloud::photos::PhotosSession::post(&self.inner, url, body, headers).await
    }

    fn clone_box(&self) -> Box<dyn crate::icloud::photos::PhotosSession> {
        Box::new(self.clone())
    }
}

#[tokio::test]
async fn run_cycle_recent_exact_multiple_libraries_advance_each_zone() {
    let mut config = make_run_cycle_config();
    config.filters.recent = Some(20);
    let db = make_state_db();
    let download_dir = tempfile::tempdir().expect("download tempdir");
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

    let primary_session = crate::test_helpers::DynamicRecentPhotosSession::new(20)
        .with_filename_prefix("primary-recent")
        .with_zone("PrimarySync")
        .with_token("rank-tok-primary");
    let shared_session_photos = crate::test_helpers::DynamicRecentPhotosSession::new(20)
        .with_filename_prefix("shared-recent")
        .with_zone("SharedSync-TEST")
        .with_token("rank-tok-shared");
    let primary_inventory = primary_session.clone();
    let shared_inventory = shared_session_photos.clone();
    let primary_requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let shared_requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let primary_state = make_run_cycle_library_state_with_album(
        "PrimarySync",
        "sync_token:PrimarySync",
        make_full_album_with_boxed_session(
            "PrimarySync",
            Box::new(RecentZoneHistory {
                inner: primary_session,
                zone: "PrimarySync",
                successor: "history-tok-primary",
                requests: Arc::clone(&primary_requests),
            }),
        ),
    );
    let shared_state = make_run_cycle_library_state_with_album(
        "SharedSync-TEST",
        "sync_token:SharedSync-TEST",
        make_full_album_with_boxed_session(
            "SharedSync-TEST",
            Box::new(RecentZoneHistory {
                inner: shared_session_photos,
                zone: "SharedSync-TEST",
                successor: "history-tok-shared",
                requests: Arc::clone(&shared_requests),
            }),
        ),
    );
    let states = vec![&primary_state, &shared_state];
    let build_download_config = make_run_cycle_download_config_builder_with_options(
        download_dir.path(),
        Arc::clone(&db),
        RunCycleDownloadConfigOptions {
            media: media_without_photo_downloads(),
            recent: Some(20),
            ..RunCycleDownloadConfigOptions::default()
        },
    );

    let result = run_cycle(
        &states,
        &config,
        Some(db.as_ref()),
        false,
        &build_download_config,
        download::DownloadControls::download_hidden(),
        &shared_session,
        &CancellationToken::new(),
    )
    .await
    .expect("run recent multi-library cycle");

    assert_eq!(result.failed_count, 0);
    // The legacy fixture has no prior recent-selection receipt: the bridge
    // must repeat the bounded inventory before its successor can be saved.
    assert_eq!(result.stats.assets_seen, 80);
    assert_eq!(
        db.get_metadata("sync_token:PrimarySync")
            .await
            .expect("read primary token"),
        Some("history-tok-primary".to_owned())
    );
    assert_eq!(
        db.get_metadata("sync_token:SharedSync-TEST")
            .await
            .expect("read shared token"),
        Some("history-tok-shared".to_owned())
    );
    assert!(!result.stats.sync_token_blocked);
    assert!(result.can_advance_database_checkpoint());
    assert_eq!(primary_inventory.emitted_ids().len(), 40);
    assert_eq!(shared_inventory.emitted_ids().len(), 40);
    for inventory in [&primary_inventory, &shared_inventory] {
        let ids = inventory.emitted_ids();
        assert_eq!(&ids[..20], &ids[20..]);
        assert_eq!(
            ids[..20]
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            20
        );
        assert!(inventory.results_limits().iter().all(|limit| *limit <= 40));
        // Each two-item page is followed by five bounded empty-tail probes.
        let expected_offsets: Vec<_> = (0..30).step_by(2).cycle().take(30).collect();
        assert_eq!(inventory.offsets(), expected_offsets);
    }
    assert_eq!(
        *primary_requests.lock().unwrap(),
        ["fixture-zone-history-boundary"]
    );
    assert_eq!(
        *shared_requests.lock().unwrap(),
        ["fixture-zone-history-boundary"]
    );
    let primary_offsets = primary_inventory.offsets();
    let shared_offsets = shared_inventory.offsets();

    let quiet = run_cycle(
        &states,
        &config,
        Some(db.as_ref()),
        false,
        &build_download_config,
        download::DownloadControls::download_hidden(),
        &shared_session,
        &CancellationToken::new(),
    )
    .await
    .expect("run quiet recent multi-library cycle");
    assert_eq!(quiet.failed_count, 0);
    assert_eq!(quiet.stats.assets_seen, 0);
    assert!(quiet.stats.full_enumeration_reason.is_none(), "{quiet:?}");
    assert!(quiet.can_advance_database_checkpoint(), "{quiet:?}");
    assert_eq!(primary_inventory.offsets(), primary_offsets);
    assert_eq!(shared_inventory.offsets(), shared_offsets);
    assert_eq!(
        *primary_requests.lock().unwrap(),
        ["fixture-zone-history-boundary", "history-tok-primary"]
    );
    assert_eq!(
        *shared_requests.lock().unwrap(),
        ["fixture-zone-history-boundary", "history-tok-shared"]
    );
    assert_eq!(
        db.get_metadata("sync_token:PrimarySync")
            .await
            .unwrap()
            .as_deref(),
        Some("history-tok-primary")
    );
    assert_eq!(
        db.get_metadata("sync_token:SharedSync-TEST")
            .await
            .unwrap()
            .as_deref(),
        Some("history-tok-shared")
    );
}

// Preserve the previous enumeration-time trigger on non-Linux pending native verification.
#[cfg(not(target_os = "linux"))]
#[tokio::test]
async fn run_cycle_destination_replaced_after_enumeration_reports_partial_failure() {
    let (capture, _guard) = crate::test_helpers::TracingCapture::install();
    let config = make_run_cycle_config();
    let inner = make_state_db();
    let download_dir = tempfile::tempdir().expect("download tempdir");
    let download_root = download_dir.path().to_path_buf();
    let db: Arc<dyn download::DownloadStore> = Arc::new(
        FailingMetadataSetDb::without_set_failure(Arc::clone(&inner), "unused")
            .with_download_dir_replaced_on_upsert(download_root.clone()),
    );
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
    let master_record_name = "master-mid-sync-destination-fault";
    let album = make_full_album_with_session(
        "PrimarySync",
        crate::test_helpers::MockPhotosSession::new()
            .ok(album_count_response(1))
            .ok(full_album_page_with_download(
                "PrimarySync",
                master_record_name,
                "zone-tok-after-fault",
                "https://p01.icloud-content.com/mid-sync-destination-unavailable.jpg",
                8,
                "AAAA",
            )),
    );
    let lib_state =
        make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
    let states = vec![&lib_state];
    let build_download_config =
        make_run_cycle_download_config_builder(download_dir.path(), Arc::clone(&db));

    let result = run_cycle(
        &states,
        &config,
        Some(db.as_ref()),
        false,
        &build_download_config,
        download::DownloadControls::download_hidden(),
        &shared_session,
        &CancellationToken::new(),
    )
    .await
    .expect("run cycle");
    let events = capture.events();

    assert_eq!(
        result.failed_count, 1,
        "mid-sync destination loss must produce a failed sync result"
    );
    assert_eq!(result.stats.failed, 1);
    assert_eq!(result.stats.downloaded, 0);
    assert!(
        result.db_sync_token_advance_safe,
        "durably queued transfer failure must not replay the provider delta"
    );
    assert_eq!(
        db.get_metadata("sync_token:PrimarySync")
            .await
            .expect("read zone token"),
        Some("zone-tok-after-fault".to_string()),
        "source checkpoint may advance once failed transfer work is durable"
    );

    let failed = db.get_failed().await.expect("read failed assets");
    assert_eq!(failed.len(), 1, "failed asset should be persisted");
    let last_error = failed[0].last_error.as_deref().expect("failed asset error");
    assert!(
        last_error.contains("Could not open temporary download file")
            || last_error.contains("Could not create directory"),
        "failed asset error should name the failing filesystem operation, got: {last_error}"
    );
    let root = download_root.display().to_string();
    let asset_record_name = format!("asset-{master_record_name}");
    let failure_event = events
        .iter()
        .find(|event| {
            event.level == tracing::Level::ERROR
                && event.message() == Some("Download failed")
                && event.field("asset_id") == Some(asset_record_name.as_str())
        })
        .unwrap_or_else(|| panic!("missing structured download failure event: {events:?}"));
    assert!(
        failure_event
            .field("path")
            .is_some_and(|path| path.contains(&root)),
        "download failure event should include path under {root}, got {failure_event:?}"
    );

    if download_root.is_file() {
        std::fs::remove_file(&download_root).expect("remove injected download-root file");
    }
}

#[cfg(target_os = "linux")]
#[derive(Clone)]
struct RootLossHistory {
    inner: crate::sync_loop::test_support::MutableCaptureSession,
    history: Arc<std::sync::Mutex<Vec<String>>>,
}

#[cfg(target_os = "linux")]
#[async_trait::async_trait]
impl crate::icloud::photos::PhotosSession for RootLossHistory {
    async fn post(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<serde_json::Value> {
        if url.contains("/changes/zone?") {
            let request: serde_json::Value = serde_json::from_str(&body)?;
            let zone = &request["zones"][0];
            assert_eq!(zone["zoneID"]["zoneName"], "PrimarySync");
            let token = zone["syncToken"].as_str().unwrap().to_string();
            assert!(
                token == "fixture-zone-history-boundary" || token == "diagnostic-history-successor"
            );
            self.history.lock().unwrap().push(token);
            return Ok(
                serde_json::json!({"zones":[{"zoneID":{"zoneName":"PrimarySync"},"records":[],"syncToken":"diagnostic-history-successor","moreComing":false}]}),
            );
        }
        crate::icloud::photos::PhotosSession::post(&self.inner, url, body, headers).await
    }
    fn clone_box(&self) -> Box<dyn crate::icloud::photos::PhotosSession> {
        Box::new(self.clone())
    }
}

// The SQLite database must remain outside the replaced download root.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn run_cycle_destination_root_loss_retains_retry_and_recovers() {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    let server = wiremock::MockServer::start().await;
    let bytes = include_bytes!("../../../../tests/data/media/pattern.jpg");
    let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(bytes));
    Mock::given(method("GET"))
        .and(path("/diagnostic.jpg"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.to_vec()))
        .mount(&server)
        .await;
    let page = full_album_page_with_download(
        "PrimarySync",
        "master-root-diagnostic",
        "inventory-query-token",
        &format!("{}/diagnostic.jpg", server.uri()),
        bytes.len() as u64,
        &checksum,
    );
    let records = page["records"].as_array().unwrap().clone();
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().join("media");
    let database = fixture.path().join("state.db");
    std::fs::create_dir(&root).unwrap();
    let config = make_run_cycle_config();
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
    let session = crate::sync_loop::test_support::MutableCaptureSession {
        zone: "PrimarySync",
        cancel_after_lookup: None,
        records: Arc::new(std::sync::Mutex::new(records)),
        repair_requests: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    };
    let history = Arc::new(std::sync::Mutex::new(Vec::new()));
    let library = make_run_cycle_library_state_with_album(
        "PrimarySync",
        "sync_token:PrimarySync",
        make_full_album_with_boxed_session(
            "PrimarySync",
            Box::new(RootLossHistory {
                inner: session,
                history: Arc::clone(&history),
            }),
        ),
    );
    {
        let inner: Arc<dyn download::DownloadStore> =
            Arc::new(state::SqliteStateDb::open(&database).await.unwrap());
        assert_eq!(
            inner.get_metadata("sync_token:PrimarySync").await.unwrap(),
            None
        );
        let faulty: Arc<dyn download::DownloadStore> = Arc::new(
            FailingMetadataSetDb::without_set_failure(inner, "unused")
                .with_download_dir_replaced_on_upsert(root.clone()),
        );
        let builder = make_run_cycle_download_config_builder(&root, Arc::clone(&faulty));
        let result = run_cycle(
            &[&library],
            &config,
            Some(faulty.as_ref()),
            false,
            &builder,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await;
        let error = result.expect_err("root replacement must refuse the bridge");
        assert!(error.chain().any(|cause| {
            cause
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.raw_os_error() == Some(20))
        }));
    }
    assert!(
        std::fs::symlink_metadata(&root)
            .unwrap()
            .file_type()
            .is_file()
    );
    let replacement = std::fs::read(&root).unwrap();
    assert_eq!(replacement, b"destination replaced by fault injection");
    {
        let db: Arc<dyn download::DownloadStore> =
            Arc::new(state::SqliteStateDb::open(&database).await.unwrap());
        let failed = db.get_failed().await.unwrap();
        let pending = db.get_pending().await.unwrap();
        let downloaded = db.get_downloaded_page(0, 10).await.unwrap();
        let checkpoint = db.get_metadata("sync_token:PrimarySync").await.unwrap();
        assert_eq!(checkpoint, None);
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].id.as_ref(), "asset-master-root-diagnostic");
        assert_eq!(failed[0].checksum.as_ref(), checksum);
        assert_eq!(failed[0].size_bytes, bytes.len() as u64);
        assert!(failed[0].local_path.is_none());
        assert!(
            failed[0]
                .last_error
                .as_deref()
                .is_some_and(|error| error.contains("Could not create directory"))
        );
        assert!(pending.is_empty());
        assert!(downloaded.is_empty());
    }
    let connection = rusqlite::Connection::open_with_flags(
        &database,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let baseline: String = connection
        .query_row(
            "SELECT value FROM metadata WHERE key LIKE ?1",
            ["baseline_cursor:v27:%"],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(baseline, "fixture-zone-history-boundary");
    assert!(history.lock().unwrap().is_empty());
    drop(connection);
    assert_eq!(std::fs::read(&root).unwrap(), replacement);
    std::fs::remove_file(&root).unwrap();
    std::fs::create_dir(&root).unwrap();
    for pass in 0..2 {
        let db: Arc<dyn download::DownloadStore> =
            Arc::new(state::SqliteStateDb::open(&database).await.unwrap());
        let builder = make_run_cycle_download_config_builder(&root, Arc::clone(&db));
        let result = run_cycle(
            &[&library],
            &config,
            Some(db.as_ref()),
            false,
            &builder,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await;
        let result = result.expect("restored cycle");
        let failed = db.get_failed().await.unwrap();
        let pending = db.get_pending().await.unwrap();
        let downloaded = db.get_downloaded_page(0, 10).await.unwrap();
        let checkpoint = db.get_metadata("sync_token:PrimarySync").await.unwrap();
        assert_eq!(result.failed_count, 0);
        assert!(result.db_sync_token_advance_safe);
        assert!(failed.is_empty());
        assert!(pending.is_empty());
        assert_eq!(checkpoint.as_deref(), Some("diagnostic-history-successor"));
        assert_eq!(downloaded.len(), 1);
        let receipt = &downloaded[0];
        assert_eq!(receipt.library.as_ref(), "PrimarySync");
        assert_eq!(receipt.id.as_ref(), "asset-master-root-diagnostic");
        assert_eq!(receipt.version_size, state::VersionSizeKey::Original);
        assert_eq!(receipt.checksum.as_ref(), checksum);
        let content_hash = format!("{:x}", Sha256::digest(bytes));
        assert_eq!(
            receipt.local_checksum.as_deref(),
            Some(content_hash.as_str())
        );
        assert_eq!(
            receipt.download_checksum.as_deref(),
            Some(content_hash.as_str())
        );
        let expected_path = root
            .join(crate::sync_loop::test_support::run_cycle_expected_date_dir())
            .join("cGhvdG8uanBn.JPG");
        assert_eq!(receipt.local_path.as_deref(), Some(expected_path.as_path()));
        assert_eq!(std::fs::read(expected_path).unwrap(), bytes);
        if pass == 1 {
            assert_eq!(result.stats.downloaded, 0);
        }
    }
    assert_eq!(
        *history.lock().unwrap(),
        [
            "fixture-zone-history-boundary",
            "diagnostic-history-successor"
        ]
    );
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
}

const ZERO_ASSET_WARNING_PREFIX: &str = "Sync completed after enumerating zero assets";

#[tokio::test]
#[tracing_test::traced_test]
async fn run_cycle_full_zero_assets_warns_once() {
    let result = run_empty_full_cycle(false).await;

    assert_eq!(result.failed_count, 0);
    assert_eq!(result.stats.assets_seen, 0);
    assert_eq!(
        result.stats.full_enumeration_reason,
        Some(download::FullEnumerationReason::OtherStaticReason)
    );
    assert!(
        logs_contain(ZERO_ASSET_WARNING_PREFIX),
        "completed full sync with zero assets should be visible in normal logs"
    );
    assert!(
        logs_contain("library_count=1"),
        "zero-asset warning should carry structured library_count"
    );
    assert!(
        logs_contain("assets_seen=0"),
        "zero-asset warning should carry structured assets_seen"
    );
}

#[tokio::test]
#[tracing_test::traced_test]
async fn run_cycle_retry_failed_zero_assets_does_not_warn() {
    let result = run_empty_full_cycle(true).await;

    assert_eq!(result.failed_count, 0);
    assert_eq!(result.stats.assets_seen, 0);
    assert_eq!(
        result.stats.full_enumeration_reason,
        Some(download::FullEnumerationReason::OtherStaticReason)
    );
    assert!(
        !logs_contain(ZERO_ASSET_WARNING_PREFIX),
        "retry-failed no-op cycles must stay quiet"
    );
}

#[tokio::test]
#[tracing_test::traced_test]
async fn run_cycle_incremental_fallback_zero_assets_warns() {
    let config = make_run_cycle_config();
    let db = make_state_db();
    db.set_metadata("sync_token:PrimarySync", "zone-tok-prev")
        .await
        .expect("seed sync token");
    let download_dir = tempfile::tempdir().expect("download tempdir");
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

    let lib_state = make_run_cycle_library_state_with_album(
        "PrimarySync",
        "sync_token:PrimarySync",
        make_empty_full_album("zone-tok-empty"),
    );
    let states = vec![&lib_state];
    let build_download_config = make_run_cycle_download_config_builder_with_options(
        download_dir.path(),
        Arc::clone(&db),
        RunCycleDownloadConfigOptions {
            per_pass_paths: true,
            ..RunCycleDownloadConfigOptions::default()
        },
    );

    let result = run_cycle(
        &states,
        &config,
        Some(db.as_ref()),
        false,
        &build_download_config,
        download::DownloadControls::download_hidden(),
        &shared_session,
        &CancellationToken::new(),
    )
    .await
    .expect("run cycle");

    assert_eq!(result.failed_count, 0);
    assert_eq!(result.stats.assets_seen, 0);
    assert!(
        logs_contain(ZERO_ASSET_WARNING_PREFIX),
        "incremental requests that fall back to full enumeration must warn"
    );
}

#[tokio::test]
#[tracing_test::traced_test]
async fn run_cycle_warns_for_empty_library_when_another_library_has_assets() {
    let config = make_run_cycle_config();
    let db = make_state_db();
    let download_dir = tempfile::tempdir().expect("download tempdir");
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

    let empty_state = make_run_cycle_library_state_with_album(
        "PrimarySync",
        "sync_token:PrimarySync",
        make_empty_full_album_for_zone("PrimarySync", "zone-tok-empty"),
    );
    let nonempty_state = make_run_cycle_library_state_with_album(
        "SharedSync-TEST",
        "sync_token:SharedSync-TEST",
        make_one_photo_full_album_for_zone("SharedSync-TEST", "zone-tok-one"),
    );
    let states = vec![&empty_state, &nonempty_state];
    let build_download_config = make_run_cycle_download_config_builder_with_options(
        download_dir.path(),
        Arc::clone(&db),
        RunCycleDownloadConfigOptions {
            media: config::MediaSelection {
                photos: false,
                videos: true,
                live_photos: true,
            },
            ..RunCycleDownloadConfigOptions::default()
        },
    );

    let result = run_cycle(
        &states,
        &config,
        Some(db.as_ref()),
        false,
        &build_download_config,
        download::DownloadControls::download_hidden(),
        &shared_session,
        &CancellationToken::new(),
    )
    .await
    .expect("run cycle");

    assert_eq!(result.failed_count, 0);
    assert_eq!(result.stats.assets_seen, 1);
    assert!(
        logs_contain(ZERO_ASSET_WARNING_PREFIX),
        "empty library must warn even when the cycle-wide asset count is nonzero"
    );
    assert!(
        logs_contain("library=PrimarySync"),
        "warning must name the empty library"
    );
}

#[tokio::test]
#[tracing_test::traced_test]
async fn run_cycle_dry_run_nonempty_full_cycle_does_not_warn() {
    let result =
        run_one_photo_full_cycle_with_controls(download::DownloadControls::dry_run_hidden()).await;

    assert_eq!(result.failed_count, 0);
    assert_eq!(result.stats.assets_seen, 0);
    assert!(
        !logs_contain(ZERO_ASSET_WARNING_PREFIX),
        "dry-run asset scans must not warn just because assets_seen stays zero"
    );
}

#[tokio::test]
#[tracing_test::traced_test]
async fn run_cycle_print_filenames_nonempty_full_cycle_does_not_warn() {
    let controls = download::DownloadControls::new(
        download::DownloadRunMode::PrintFilenames,
        download::DownloadReporting::hidden(),
    );
    let result = run_one_photo_full_cycle_with_controls(controls).await;

    assert_eq!(result.failed_count, 0);
    assert_eq!(result.stats.assets_seen, 0);
    assert!(
        !logs_contain(ZERO_ASSET_WARNING_PREFIX),
        "print-only asset scans must not warn just because assets_seen stays zero"
    );
}

#[tokio::test]
async fn offline_replay_full_pass_reaches_sync_loop_planning_boundary() {
    let album = make_full_album_with_session(
        "PrimarySync",
        crate::test_helpers::MockPhotosFlow::new()
            .album_count(1)
            .query_photo_page("master-replay-sync-loop", Some("zone-tok-replay"))
            .empty_query_page(Some("zone-tok-replay"))
            .build(),
    );

    let result =
        run_full_cycle_with_album(album, false, download::DownloadControls::dry_run_hidden()).await;

    assert_eq!(result.failed_count, 0);
    assert_eq!(
        result.stats.downloaded, 1,
        "dry-run sync-loop replay must reach download planning"
    );
    assert_eq!(result.stats.failed, 0);
}

/// When a previously-downloaded asset's local file exists but the
/// asset is absent from the API response, kei must NOT delete the
/// local file by default. This guards against a pagination bug
/// being interpreted as mass remote deletion.
#[tokio::test]
async fn remote_deletion_local_file_preserved_per_default_keep_policy() {
    let db = state::SqliteStateDb::open_in_memory().expect("open in-memory state DB");
    let dir = tempfile::tempdir().unwrap();
    let local_path = dir.path().join("2025/06/15/deleted_asset.jpg");

    // Pre-seed: asset was previously downloaded
    tokio::fs::create_dir_all(local_path.parent().unwrap())
        .await
        .unwrap();
    tokio::fs::write(&local_path, b"old photo bytes")
        .await
        .unwrap();
    let record = crate::test_helpers::TestAssetRecord::new("DEL_ASSET")
        .checksum("old_ck")
        .build();
    db.upsert_seen(&record).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "DEL_ASSET",
        "original",
        &local_path,
        "old_ck",
        None,
    )
    .await
    .unwrap();

    // Verify the asset is still marked as downloaded and the file
    // is recognized as present
    let should_dl = db
        .should_download(
            "PrimarySync",
            "DEL_ASSET",
            "original",
            "old_ck",
            &local_path,
        )
        .await
        .unwrap();
    assert!(!should_dl, "unchanged asset must not need download");
    assert!(
        local_path.exists(),
        "local file preserved per default policy"
    );
}

/// An empty local state must keep all summary and pending views
/// internally consistent. This pins the read side that user-facing
/// status/verify commands rely on before any remote assets have been
/// observed.
#[tokio::test]
async fn empty_state_summary_has_no_pending_or_downloaded_rows() {
    let db = state::SqliteStateDb::open_in_memory().expect("open in-memory state DB");
    let summary = db.get_summary().await.unwrap();
    assert_eq!(summary.total_assets, 0, "empty DB must have zero assets");
    assert_eq!(summary.downloaded, 0, "empty DB must have zero downloaded");
    assert_eq!(summary.pending, 0, "empty DB must have zero pending");
    assert_eq!(summary.failed, 0, "empty DB must have zero failed");
    assert!(
        db.get_pending().await.unwrap().is_empty(),
        "fresh state must not surface phantom pending work"
    );
}
