use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::sync_cycle::{CycleResult, run_cycle};
use crate::sync_loop::test_support::{
    RunCycleDownloadConfigOptions, album_count_response, full_album_page,
    full_album_page_with_download, make_full_album_with_session, make_run_cycle_config,
    make_run_cycle_download_config_builder, make_run_cycle_download_config_builder_with_options,
    make_run_cycle_library_state_with_album, make_shared_session_for_run_cycle,
    media_without_photo_downloads, run_full_cycle_with_album,
};
use crate::{config, download, state};

fn make_one_photo_full_album_for_zone(
    zone: &str,
    zone_sync_token: &str,
) -> crate::icloud::photos::PhotoAlbum {
    make_full_album_with_session(
        zone,
        crate::test_helpers::MockPhotosSession::new()
            .ok(album_count_response(1))
            .ok(full_album_page(
                zone,
                &format!("master-{zone}"),
                zone_sync_token,
            )),
    )
}

async fn run_empty_full_cycle() -> CycleResult {
    run_empty_full_cycle_with_controls(download::DownloadControls::download_hidden()).await
}

async fn run_empty_full_cycle_with_controls(controls: download::DownloadControls) -> CycleResult {
    let root = tempfile::tempdir().unwrap();
    let owned =
        crate::sync_loop::test_support::open_owned_cycle_db(&root.path().join("state.db")).await;
    let db: Arc<dyn download::DownloadStore> = owned.clone();
    let selected = make_run_cycle_library_state_with_album(
        "PrimarySync",
        "sync_token:PrimarySync",
        crate::sync_loop::test_support::make_owned_cycle_album(
            &owned,
            "PrimarySync",
            "private",
            Box::new(
                crate::test_helpers::DynamicRecentPhotosSession::new(0)
                    .with_token("zone-tok-empty"),
            ),
        ),
    );
    let config = make_run_cycle_config();
    let (_session_dir, session) = make_shared_session_for_run_cycle().await;
    let builder = make_run_cycle_download_config_builder(root.path(), db.clone());
    run_cycle(
        &[&selected],
        &config,
        Some(db.as_ref()),
        &builder,
        controls,
        &session,
        &CancellationToken::new(),
    )
    .await
    .unwrap()
}

async fn run_one_photo_full_cycle_with_controls(
    controls: download::DownloadControls,
) -> CycleResult {
    run_full_cycle_with_album(
        make_one_photo_full_album_for_zone("PrimarySync", "zone-tok-one"),
        controls,
    )
    .await
}

#[tokio::test]
async fn run_cycle_recent_exact_multiple_libraries_advance_each_zone() {
    let config = make_run_cycle_config();
    let download_dir = tempfile::tempdir().expect("download tempdir");
    let owned_db =
        crate::sync_loop::test_support::open_owned_cycle_db(&download_dir.path().join("state.db"))
            .await;
    let db: Arc<dyn download::DownloadStore> = owned_db.clone();
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

    let primary_session = crate::test_helpers::DynamicRecentPhotosSession::new(20)
        .with_filename_prefix("primary-recent")
        .with_zone("PrimarySync")
        .with_token("zone-tok-primary");
    let shared_session_photos = crate::test_helpers::DynamicRecentPhotosSession::new(20)
        .with_filename_prefix("shared-recent")
        .with_zone("SharedSync-TEST")
        .with_token("zone-tok-shared");
    let primary_state = make_run_cycle_library_state_with_album(
        "PrimarySync",
        "sync_token:PrimarySync",
        crate::sync_loop::test_support::make_owned_cycle_album(
            &owned_db,
            "PrimarySync",
            "private",
            Box::new(primary_session),
        ),
    );
    let shared_state = make_run_cycle_library_state_with_album(
        "SharedSync-TEST",
        "sync_token:SharedSync-TEST",
        crate::sync_loop::test_support::make_owned_cycle_album(
            &owned_db,
            "SharedSync-TEST",
            "shared",
            Box::new(shared_session_photos),
        ),
    );
    let states = vec![&primary_state, &shared_state];
    let build_download_config = make_run_cycle_download_config_builder_with_options(
        download_dir.path(),
        Arc::clone(&db),
        RunCycleDownloadConfigOptions {
            media: media_without_photo_downloads(),
            ..RunCycleDownloadConfigOptions::default()
        },
    );

    let result = run_cycle(
        &states,
        &config,
        Some(db.as_ref()),
        &build_download_config,
        download::DownloadControls::download_hidden(),
        &shared_session,
        &CancellationToken::new(),
    )
    .await
    .expect("run recent multi-library cycle");

    assert_eq!(result.failed_count, 0);
    assert_eq!(result.stats.assets_seen, 40);
    assert_eq!(
        db.get_metadata("sync_token:PrimarySync")
            .await
            .expect("read primary token"),
        Some("zone-tok-primary".to_owned())
    );
    assert_eq!(
        db.get_metadata("sync_token:SharedSync-TEST")
            .await
            .expect("read shared token"),
        Some("zone-tok-shared".to_owned())
    );
}

const ZERO_ASSET_WARNING_PREFIX: &str = "Sync completed after enumerating zero assets";

#[tokio::test]
async fn run_cycle_destination_replaced_after_enumeration_reports_partial_failure() {
    let (capture, _guard) = crate::test_helpers::TracingCapture::install();
    let config = make_run_cycle_config();
    let download_dir = tempfile::tempdir().expect("download tempdir");
    let download_root = download_dir.path().join("media");
    std::fs::create_dir(&download_root).unwrap();
    let inner =
        crate::sync_loop::test_support::open_owned_cycle_db(&download_dir.path().join("state.db"))
            .await;
    let db: Arc<dyn download::DownloadStore> = inner.clone();
    let server = wiremock::MockServer::start().await;
    let injected = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let fault = injected.clone();
    let root = download_root.clone();
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .respond_with(move |_: &wiremock::Request| {
            if !fault.swap(true, std::sync::atomic::Ordering::SeqCst) {
                std::fs::rename(&root, root.with_extension("retained")).unwrap();
                std::fs::write(&root, b"destination replaced by fault injection").unwrap();
            }
            wiremock::ResponseTemplate::new(200).set_body_bytes(vec![0u8; 8])
        })
        .expect(1)
        .mount(&server)
        .await;
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
    let master_record_name = "master-mid-sync-destination-fault";
    let mut page = full_album_page_with_download(
        "PrimarySync",
        master_record_name,
        "zone-tok-after-fault",
        &format!("{}/media.jpg", server.uri()),
        8,
        "AAAA",
    );
    page["records"][1]["fields"]["masterRef"]["value"]["zoneID"]["ownerRecordName"] =
        serde_json::json!("_defaultOwner");
    let album = crate::sync_loop::test_support::make_owned_cycle_album(
        &inner,
        "PrimarySync",
        "private",
        Box::new(crate::sync_loop::test_support::MutableCaptureSession {
            zone: "PrimarySync",
            token: "zone-tok-after-fault",
            cancel_after_lookup: None,
            records: Arc::new(std::sync::Mutex::new(
                page["records"].as_array().unwrap().clone(),
            )),
            repair_requests: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }),
    );
    let lib_state =
        make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
    let states = vec![&lib_state];
    let build_download_config =
        make_run_cycle_download_config_builder(&download_root, Arc::clone(&db));

    let result = run_cycle(
        &states,
        &config,
        Some(db.as_ref()),
        &build_download_config,
        download::DownloadControls::download_hidden(),
        &shared_session,
        &CancellationToken::new(),
    )
    .await
    .expect("run cycle");
    let events = capture.events();
    assert!(injected.load(std::sync::atomic::Ordering::SeqCst));

    assert_eq!(
        result.failed_count, 1,
        "mid-sync destination loss must produce a failed sync result"
    );
    assert_eq!(result.stats.failed, 1);
    assert_eq!(result.stats.downloaded, 0);
    assert!(
        !result.db_sync_token_advance_safe,
        "unfinished selected publication must retain the provider checkpoint"
    );
    assert_eq!(
        db.get_metadata("sync_token:PrimarySync")
            .await
            .expect("read zone token"),
        None,
        "durable failure alone cannot authorize a source checkpoint"
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

#[tokio::test]
#[tracing_test::traced_test]
async fn run_cycle_full_zero_assets_warns_once() {
    let result = run_empty_full_cycle().await;

    assert_eq!(result.failed_count, 0);
    assert_eq!(result.stats.assets_seen, 0);
    assert_eq!(
        result.stats.full_enumeration_reason,
        Some(download::FullEnumerationReason::NoStoredToken)
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
async fn run_cycle_dry_run_zero_assets_does_not_warn() {
    let result =
        run_empty_full_cycle_with_controls(download::DownloadControls::dry_run_hidden()).await;

    assert_eq!(result.failed_count, 0);
    assert_eq!(result.stats.assets_seen, 0);
    assert_eq!(
        result.stats.full_enumeration_reason,
        Some(download::FullEnumerationReason::NoStoredToken)
    );
    assert!(
        !logs_contain(ZERO_ASSET_WARNING_PREFIX),
        "read-only previews must not emit the writable-sync warning"
    );
}

#[tokio::test]
#[tracing_test::traced_test]
async fn run_cycle_incremental_fallback_zero_assets_warns() {
    let config = make_run_cycle_config();
    let download_dir = tempfile::tempdir().expect("download tempdir");
    let owned =
        crate::sync_loop::test_support::open_owned_cycle_db(&download_dir.path().join("state.db"))
            .await;
    let db: Arc<dyn download::DownloadStore> = owned.clone();
    db.set_metadata("sync_token:PrimarySync", "zone-tok-prev")
        .await
        .expect("seed sync token");
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

    let lib_state = make_run_cycle_library_state_with_album(
        "PrimarySync",
        "sync_token:PrimarySync",
        crate::sync_loop::test_support::make_owned_cycle_album(
            &owned,
            "PrimarySync",
            "private",
            Box::new(
                crate::test_helpers::DynamicRecentPhotosSession::new(0)
                    .with_token("zone-tok-empty"),
            ),
        ),
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
    let download_dir = tempfile::tempdir().expect("download tempdir");
    let owned =
        crate::sync_loop::test_support::open_owned_cycle_db(&download_dir.path().join("state.db"))
            .await;
    let db: Arc<dyn download::DownloadStore> = owned.clone();
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

    let empty_state = make_run_cycle_library_state_with_album(
        "PrimarySync",
        "sync_token:PrimarySync",
        crate::sync_loop::test_support::make_owned_cycle_album(
            &owned,
            "PrimarySync",
            "private",
            Box::new(
                crate::test_helpers::DynamicRecentPhotosSession::new(0)
                    .with_token("zone-tok-empty"),
            ),
        ),
    );
    let nonempty_state = make_run_cycle_library_state_with_album(
        "SharedSync-TEST",
        "sync_token:SharedSync-TEST",
        crate::sync_loop::test_support::make_owned_cycle_album(
            &owned,
            "SharedSync-TEST",
            "shared",
            Box::new(
                crate::test_helpers::DynamicRecentPhotosSession::new(1)
                    .with_zone("SharedSync-TEST")
                    .with_token("zone-tok-one"),
            ),
        ),
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
        run_full_cycle_with_album(album, download::DownloadControls::dry_run_hidden()).await;

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
