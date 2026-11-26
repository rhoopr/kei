use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::Engine as _;
use rusqlite::Connection;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::commands::{AlbumPass, PassKind};
use crate::sync_cycle::{CycleResult, run_cycle};
use crate::sync_loop::test_support::{
    MutableCaptureSession, RunCycleDownloadConfigOptions, album_count_response, full_album_page,
    full_album_page_with_download, make_full_album_with_session, make_named_owned_cycle_album,
    make_run_cycle_config, make_run_cycle_download_config_builder,
    make_run_cycle_download_config_builder_with_options, make_run_cycle_library_state_with_album,
    make_run_cycle_library_state_with_passes, make_shared_session_for_run_cycle,
    media_without_photo_downloads, open_owned_cycle_db, run_full_cycle_with_album,
};
use crate::{config, download, retry, state};

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
    let media = include_bytes!("../../../../tests/data/media/pattern.jpg");
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .respond_with(move |_: &wiremock::Request| {
            if !fault.swap(true, std::sync::atomic::Ordering::SeqCst) {
                let rename = std::fs::rename(&root, root.with_extension("retained"));
                #[cfg(unix)]
                {
                    rename.unwrap();
                    std::fs::write(&root, b"destination replaced by fault injection").unwrap();
                }
                // Confined Windows parent handles deny delete sharing during transfer.
                #[cfg(windows)]
                assert!(matches!(rename.unwrap_err().raw_os_error(), Some(5 | 32)));
            }
            wiremock::ResponseTemplate::new(200).set_body_bytes(media.as_slice())
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
        media.len() as u64,
        &base64::engine::general_purpose::STANDARD.encode(Sha256::digest(media)),
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

    if cfg!(windows) {
        assert_eq!(result.failed_count, 0);
        assert_eq!(result.stats.downloaded, 1);
        let files = db.get_downloaded_page(0, 2).await.unwrap();
        assert_eq!(files.len(), 1);
        let path = files[0].local_path.as_ref().unwrap();
        assert!(path.starts_with(&download_root));
        assert_eq!(std::fs::read(path).unwrap(), media);
        return;
    }
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
                && event.field("path").is_some_and(|path| path.contains(&root))
        })
        .unwrap_or_else(|| panic!("missing structured download failure event: {events:?}"));
    assert!(
        !failure_event
            .fields
            .values()
            .any(|value| value.contains(&asset_record_name)),
        "download failure must identify the local path without exposing the provider child"
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

fn enumeration_capture_session(records: Vec<Value>, token: &'static str) -> MutableCaptureSession {
    MutableCaptureSession {
        zone: "PrimarySync",
        token,
        cancel_after_lookup: None,
        records: Arc::new(std::sync::Mutex::new(records)),
        repair_requests: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    }
}

fn enumeration_album_records(
    master: &str,
    filename: &str,
    url: &str,
    bytes: &[u8],
    checksum: &str,
) -> Vec<Value> {
    let mut page = full_album_page_with_download(
        "PrimarySync",
        master,
        "issue-1052-token",
        url,
        bytes.len() as u64,
        checksum,
    );
    page["records"][0]["fields"]["filenameEnc"] = json!({"value": filename, "type": "STRING"});
    page["records"].as_array().expect("album records").clone()
}

async fn open_migrated_cycle_db(path: &Path) -> Arc<state::SqliteStateDb> {
    let released = path.with_extension("released.db");
    Connection::open(&released)
        .expect("open released state fixture")
        .execute_batch(include_str!(
            "../../../../tests/data/released-v0240-schema.sql"
        ))
        .expect("load released state schema");
    let login: crate::auth::AccountLoginResponse =
        serde_json::from_value(json!({"dsInfo":{"dsid":"runner-fixture"}}))
            .expect("fixture login response");
    let owner =
        state::db::account::AccountOwner::authenticated("runner@example.invalid", "com", &login)
            .expect("fixture owner");
    state::db::account::adopt_legacy(&released, path, &owner, "runner@example.invalid", "com")
        .await
        .expect("adopt released state");
    let db = open_owned_cycle_db(path).await;
    {
        let conn = db
            .acquire_lock("issue 1052 migrated state")
            .expect("state lock");
        assert_eq!(
            conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i32>(0))
                .expect("schema version"),
            state::schema::SCHEMA_VERSION,
            "the fixture must exercise a migrated current-schema database"
        );
    }
    db
}

fn files_below(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, found: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("read media directory") {
            let path = entry.expect("media entry").path();
            if path.is_dir() {
                walk(&path, found);
            } else {
                found.push(path);
            }
        }
    }
    let mut found = Vec::new();
    walk(root, &mut found);
    found.sort();
    found
}

#[tokio::test]
async fn issue_1052_migrated_full_sync_reports_html_failure_after_later_album_progress() {
    let server = MockServer::start().await;
    let html_body = b"<!DOCTYPE html><html><body>Service Temporarily Unavailable</body></html>";
    let good_bytes = include_bytes!("../../../../tests/data/media/pattern.jpg");
    let html_checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(html_body));
    let good_checksum =
        base64::engine::general_purpose::STANDARD.encode(Sha256::digest(good_bytes));

    Mock::given(method("GET"))
        .and(path("/issue-1052-bad.jpg"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(html_body))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/issue-1052-good.jpg"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(good_bytes))
        .expect(1)
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().expect("issue 1052 tempdir");
    let database = dir.path().join("state.db");
    let media = dir.path().join("media");
    std::fs::create_dir(&media).expect("media directory");

    let bad_records = enumeration_album_records(
        "issue-1052-bad-master",
        "issue-1052-bad.jpg",
        &format!("{}/issue-1052-bad.jpg", server.uri()),
        html_body,
        &html_checksum,
    );
    let good_records = enumeration_album_records(
        "issue-1052-good-master",
        "issue-1052-good.jpg",
        &format!("{}/issue-1052-good.jpg", server.uri()),
        good_bytes,
        &good_checksum,
    );
    let db = open_migrated_cycle_db(&database).await;
    let passes = vec![
        AlbumPass {
            kind: PassKind::Album,
            album: make_named_owned_cycle_album(
                &db,
                "PrimarySync",
                "Bad Album",
                "private",
                Box::new(enumeration_capture_session(
                    bad_records,
                    "issue-1052-bad-token",
                )),
            ),
            exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
        },
        AlbumPass {
            kind: PassKind::Album,
            album: make_named_owned_cycle_album(
                &db,
                "PrimarySync",
                "Good Album",
                "private",
                Box::new(enumeration_capture_session(
                    good_records,
                    "issue-1052-good-token",
                )),
            ),
            exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
        },
    ];

    let config = make_run_cycle_config();
    let state =
        make_run_cycle_library_state_with_passes("PrimarySync", "sync_token:PrimarySync", passes);
    let result = {
        let store: Arc<dyn download::DownloadStore> = db.clone();
        let build_download_config = make_run_cycle_download_config_builder_with_options(
            &media,
            store.clone(),
            RunCycleDownloadConfigOptions {
                per_pass_paths: true,
                concurrent_downloads: Some(1),
                retry: Some(retry::RetryConfig {
                    max_retries: 0,
                    base_delay_secs: 0,
                    max_delay_secs: 0,
                }),
                ..RunCycleDownloadConfigOptions::default()
            },
        );
        let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
        tokio::time::timeout(
            std::time::Duration::from_secs(20),
            Box::pin(run_cycle(
                &[&state],
                &config,
                Some(store.as_ref()),
                &build_download_config,
                download::DownloadControls::download_hidden(),
                &shared_session,
                &CancellationToken::new(),
            )),
        )
        .await
        .expect("full enumeration must finish within the bounded timeout")
        .expect("full enumeration should return an explicit partial result")
    };
    drop(db);

    assert_eq!(
        result.stats.full_enumeration_reason,
        Some(download::FullEnumerationReason::NoStoredToken),
        "the migrated database must take the ordinary full-enumeration path"
    );
    assert_eq!(result.stats.assets_seen, 2);
    assert_eq!(
        result.stats.downloaded, 1,
        "the later album must still download"
    );
    assert_eq!(result.stats.failed, 1, "the HTML transfer must be reported");
    assert_eq!(
        result.failed_count, 1,
        "the cycle must be explicitly partial"
    );

    let requests = server
        .received_requests()
        .await
        .expect("recorded transfer requests");
    let bad_request = requests
        .iter()
        .position(|request| request.url.path() == "/issue-1052-bad.jpg")
        .expect("the bad album transfer should be attempted");
    let good_request = requests
        .iter()
        .position(|request| request.url.path() == "/issue-1052-good.jpg")
        .expect("the later album transfer should be attempted");
    assert!(
        bad_request < good_request,
        "the later album must run after the selected HTML transfer fails"
    );

    let files = files_below(&media);
    assert_eq!(
        files.len(),
        1,
        "the failed HTML transfer must not leave an incomplete file: {files:?}"
    );
    let good_files: Vec<_> = files
        .iter()
        .filter(|file| std::fs::read(file).expect("read media file") == good_bytes)
        .collect();
    assert_eq!(
        good_files.len(),
        1,
        "the valid later-pass file must be published exactly once: {files:?}"
    );
    assert!(
        files
            .iter()
            .all(|file| std::fs::read(file).expect("read media file") != html_body),
        "the failed HTML body must never be published as media: {files:?}"
    );

    let reopened = open_owned_cycle_db(&database).await;
    let failed = reopened.get_failed().await.expect("read failed assets");
    assert_eq!(failed.len(), 1, "the bad asset must remain durable");
    assert_eq!(failed[0].id.as_ref(), "asset-issue-1052-bad-master");
    assert_eq!(failed[0].status, state::AssetStatus::Failed);
    assert!(
        failed[0]
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains("HTML markup")),
        "durable failure should retain the HTML validation reason: {failed:?}"
    );
    assert!(
        failed[0].local_path.is_none(),
        "the incomplete bad asset must not claim a published path"
    );
}
