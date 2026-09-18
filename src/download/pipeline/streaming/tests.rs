use std::fs;
use std::sync::Arc;
use std::time::Instant;

use futures_util::stream;
use indicatif::ProgressBar;
use reqwest::Client;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

use crate::download::filter::derive_expected_paths;
use crate::download::pipeline::outcome::build_download_outcome;
use crate::download::pipeline::streaming::{
    StreamRuntime, stream_and_download_from_stream, stream_and_download_from_stream_with_context,
};
use crate::download::pipeline::test_support::{FailingDownloadStore, build_zero_download_outcome};
use crate::download::{
    DownloadConfig, DownloadControls, DownloadOutcome, DownloadReporting, DownloadStore,
    preload_download_context,
};
use crate::icloud::photos::PhotoAsset;
use crate::retry::RetryConfig;
use crate::test_helpers::TestPhotoAsset;

#[tokio::test]
async fn test_producer_panic_propagates_as_error() {
    use crate::download::{DownloadConfig, SyncMode};
    use crate::icloud::photos::PhotoAsset;
    use crate::types::{
        AssetVersionSize, FileMatchPolicy, LivePhotoMode, LivePhotoMovFilenamePolicy, RawPolicy,
    };
    use rustc_hash::FxHashSet;

    let config = Arc::new(DownloadConfig {
        directory: std::sync::Arc::from(std::path::Path::new("/nonexistent/download_filter_tests")),
        folder_structure: "{:%Y/%m/%d}".to_string(),
        folder_structure_albums: Arc::from(crate::config::DEFAULT_FOLDER_STRUCTURE_ALBUMS),
        folder_structure_smart_folders: Arc::from(
            crate::config::DEFAULT_FOLDER_STRUCTURE_SMART_FOLDERS,
        ),
        resolution: crate::types::PhotoResolution::Original,
        media: crate::config::MediaSelection::all(),
        skip_created_before: None,
        skip_created_after: None,
        metadata: crate::config::MetadataConfig::default(),
        refresh_metadata: false,
        capture_timestamp_repair: crate::download::CaptureTimestampRepair::Preserve,
        repair_truncated: false,
        concurrent_downloads: 1,
        recent: None,
        recent_scope: crate::cli::RecentScope::Global,
        retry: RetryConfig::default(),
        live_photo_mode: LivePhotoMode::Both,
        live_resolution: AssetVersionSize::LiveOriginal,
        live_photo_mov_filename_policy: LivePhotoMovFilenamePolicy::Suffix,
        edited: false,
        alternative: false,
        raw_policy: RawPolicy::AsIs,
        file_match_policy: FileMatchPolicy::NameSizeDedupWithSuffix,
        force_resolution: false,
        keep_unicode_in_filenames: false,
        filename_exclude: std::sync::Arc::from(Vec::<glob::Pattern>::new()),
        temp_suffix: std::sync::Arc::from(".kei-tmp"),
        state_db: None,
        retry_only: false,
        max_download_attempts: 10,
        sync_mode: SyncMode::Full,
        enum_config_hash: None,
        album_name: None,
        exclude_asset_ids: Arc::new(FxHashSet::default()),
        asset_groupings: Arc::new(crate::download::AssetGroupings::default()),
        bandwidth_limiter: None,
        library: std::sync::Arc::from("PrimarySync"),
    });
    let client = reqwest::Client::new();
    let shutdown_token = CancellationToken::new();

    // Stream that panics on first poll — simulates a producer task panic
    let panicking_stream = futures_util::stream::poll_fn(
        |_cx| -> std::task::Poll<Option<anyhow::Result<PhotoAsset>>> {
            panic!("simulated producer panic");
        },
    );

    let err = stream_and_download_from_stream(
        &client,
        panicking_stream,
        &config,
        DownloadControls::download_hidden(),
        0,
        shutdown_token,
        StreamRuntime::new(None, None),
    )
    .await
    .expect_err("should propagate producer panic");
    assert!(
        err.to_string().contains("producer task crashed"),
        "Expected producer panic error, got: {err}"
    );
}

#[tokio::test]
async fn print_and_dry_run_preserve_downloaded_child_identity() {
    use base64::Engine as _;
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    let server = crate::start_wiremock_or_skip!();
    let body = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];
    let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&body));
    Mock::given(method("GET"))
        .and(path("/stream-mode-identity.jpg"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(body.clone())
                .insert_header("content-type", "image/jpeg"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let url = format!("{}/stream-mode-identity.jpg", server.uri());
    let make_asset = || {
        PhotoAsset::new(
            json!({
                "recordName": "STREAM_MODE_MASTER",
                "fields": {
                    "filenameEnc": {"value": "stream-mode.jpg", "type": "STRING"},
                    "itemType": {"value": "public.jpeg"},
                    "resOriginalRes": {"value": {
                        "size": body.len(),
                        "downloadURL": url,
                        "fileChecksum": checksum,
                    }},
                    "resOriginalFileType": {"value": "public.jpeg"},
                },
            }),
            json!({
                "recordName": "asset-STREAM_MODE_CHILD",
                "fields": {"assetDate": {"value": 1736899200000.0}},
            }),
        )
    };

    let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().expect("state db"));
    let dir = TempDir::new().expect("temp dir");
    let mut config = DownloadConfig::test_default();
    config.directory = Arc::from(dir.path());
    config.concurrent_downloads = 1;
    config.state_db = Some(db.clone());
    let config = Arc::new(config);

    let downloaded = stream_and_download_from_stream(
        &reqwest::Client::new(),
        stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(make_asset())]),
        &config,
        DownloadControls::download_hidden(),
        1,
        CancellationToken::new(),
        StreamRuntime::new(None, None),
    )
    .await
    .expect("normal sync should download the child");
    assert_eq!(downloaded.downloaded, 1);
    let rows = db.get_downloaded_page(0, 10).await.expect("downloaded row");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id.as_ref(), "asset-STREAM_MODE_CHILD");

    let printed = stream_and_download_from_stream(
        &reqwest::Client::new(),
        stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(make_asset())]),
        &config,
        DownloadControls::new(
            crate::download::DownloadRunMode::PrintFilenames,
            DownloadReporting::hidden(),
        ),
        1,
        CancellationToken::new(),
        StreamRuntime::new(None, None),
    )
    .await
    .expect("print-only sync should recognize the downloaded child");
    assert!(
        printed.printed_filenames.is_empty(),
        "print-only must not emit collision paths for downloaded child state: {:?}",
        printed.printed_filenames
    );

    let asset = make_asset();
    let base_path = derive_expected_paths(&asset, config.as_ref())
        .into_iter()
        .next()
        .expect("asset has an expected path")
        .path;
    let base_filename = base_path
        .file_name()
        .and_then(|name| name.to_str())
        .expect("expected path has a filename");
    let expected_dry_run_path =
        base_path.with_file_name(crate::download::paths::insert_asset_identity_suffix(
            base_filename,
            asset.asset_record_name(),
        ));
    let (capture, _guard) = crate::test_helpers::TracingCapture::install();
    let dry_run = stream_and_download_from_stream(
        &reqwest::Client::new(),
        stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(asset)]),
        &config,
        DownloadControls::dry_run_hidden(),
        1,
        CancellationToken::new(),
        StreamRuntime::new(None, None),
    )
    .await
    .expect("dry-run sync should plan the child identity");
    assert_eq!(dry_run.downloaded, 1);
    let dry_run_path = capture
        .events()
        .into_iter()
        .find(|event| event.message() == Some("[DRY RUN] Would download"))
        .and_then(|event| event.field("path").map(ToOwned::to_owned))
        .expect("dry-run path event");
    assert_eq!(dry_run_path, expected_dry_run_path.display().to_string());
}

#[tokio::test]
async fn dry_run_mode_uses_stream_pipeline_without_downloading() {
    use crate::download::DownloadConfig;
    use crate::icloud::photos::PhotoAsset;
    use futures_util::stream;

    let dir = TempDir::new().unwrap();
    let mut config = DownloadConfig::test_default();
    config.directory = std::sync::Arc::from(dir.path());
    let config = Arc::new(config);
    let asset = TestPhotoAsset::new("DRY_RUN_MODE")
        .orig_size(123)
        .orig_url("https://p01.icloud-content.com/dry-run.jpg")
        .orig_checksum("ck_dry_run")
        .build();
    let result = stream_and_download_from_stream(
        &reqwest::Client::new(),
        stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(asset)]),
        &config,
        DownloadControls::dry_run_hidden(),
        1,
        CancellationToken::new(),
        StreamRuntime::new(None, None),
    )
    .await
    .expect("dry run should scan through the real stream pipeline");

    assert_eq!(result.downloaded, 1);
    assert!(result.failed.is_empty());
    assert!(
        fs::read_dir(dir.path()).unwrap().next().is_none(),
        "dry-run mode must not create downloaded files"
    );
}

#[tokio::test]
async fn shared_bar_seeds_static_scanning_message_before_first_file() {
    use crate::download::{DownloadConfig, DownloadRunMode};
    use crate::icloud::photos::PhotoAsset;
    use crate::personality::Mode;
    use futures_util::stream;

    let dir = TempDir::new().unwrap();
    let mut config = DownloadConfig::test_default();
    config.directory = std::sync::Arc::from(dir.path());
    config.album_name = Some(std::sync::Arc::from("Trip"));
    let config = Arc::new(config);
    let pb = ProgressBar::hidden();
    let controls = DownloadControls::new(
        DownloadRunMode::Download,
        DownloadReporting::new(false, Mode::Friendly),
    );

    let result = stream_and_download_from_stream(
        &reqwest::Client::new(),
        stream::empty::<anyhow::Result<PhotoAsset>>(),
        &config,
        controls,
        0,
        CancellationToken::new(),
        StreamRuntime::new(Some(pb.clone()), None),
    )
    .await
    .expect("empty shared-bar pass should finish");

    assert_eq!(result.downloaded, 0);
    assert_eq!(pb.message(), "Trip \u{00b7} scanning...");
}

#[tokio::test]
async fn dry_run_mode_reports_enumeration_errors_without_downloading() {
    use crate::download::{DownloadConfig, DownloadOutcome};
    use crate::icloud::photos::PhotoAsset;
    use futures_util::stream;

    let dir = TempDir::new().unwrap();
    let mut config = DownloadConfig::test_default();
    config.directory = std::sync::Arc::from(dir.path());
    let config = Arc::new(config);
    let controls = DownloadControls::dry_run_hidden();
    let asset = TestPhotoAsset::new("DRY_RUN_PARTIAL")
        .orig_size(123)
        .orig_url("https://p01.icloud-content.com/dry-run-partial.jpg")
        .orig_checksum("ck_dry_run_partial")
        .build();
    let client = reqwest::Client::new();

    let streaming_result = stream_and_download_from_stream(
        &client,
        stream::iter(vec![
            Ok::<PhotoAsset, anyhow::Error>(asset),
            Err(anyhow::anyhow!("malformed page")),
        ]),
        &config,
        controls,
        2,
        CancellationToken::new(),
        StreamRuntime::new(None, None),
    )
    .await
    .expect("dry run should continue past enumeration errors");
    let (outcome, stats) = build_download_outcome(
        &client,
        &[],
        &config,
        controls,
        streaming_result,
        Instant::now(),
        CancellationToken::new(),
    )
    .await
    .expect("dry-run outcome should build");

    assert!(
        matches!(outcome, DownloadOutcome::PartialFailure { failed_count: 1 }),
        "dry-run enumeration errors must produce PartialFailure, got {outcome:?}"
    );
    assert_eq!(stats.downloaded, 1);
    assert_eq!(stats.enumeration_errors, 1);
    assert!(
        fs::read_dir(dir.path()).unwrap().next().is_none(),
        "dry-run mode must not create downloaded files"
    );
}

#[tokio::test]
async fn provider_session_failure_stops_all_stream_run_modes() {
    for controls in [
        DownloadControls::download_hidden(),
        DownloadControls::dry_run_hidden(),
        DownloadControls::new(
            crate::download::DownloadRunMode::PrintFilenames,
            DownloadReporting::hidden(),
        ),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let mut config = DownloadConfig::test_default();
        config.directory = Arc::from(directory.path());
        let config = Arc::new(config);
        let errors = (0..2).map(|_| {
            Err::<PhotoAsset, _>(anyhow::Error::new(
                crate::icloud::photos::session::HttpStatusError {
                    status: 421,
                    url: "https://example.invalid/private?token=secret".into(),
                    retry_after: None,
                    body: None,
                },
            ))
        });
        let result = stream_and_download_from_stream(
            &Client::new(),
            stream::iter(errors),
            &config,
            controls,
            0,
            CancellationToken::new(),
            StreamRuntime::new(None, None),
        )
        .await
        .unwrap();
        assert_eq!(result.provider_auth_errors, 1);
        assert_eq!(result.enumeration_errors, 1);
        assert!(!result.enumeration_complete);
        let (outcome, _) = build_zero_download_outcome(result, controls).await;
        assert!(matches!(
            outcome,
            DownloadOutcome::SessionExpired {
                auth_error_count: 1
            }
        ));
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }
}

#[tokio::test]
async fn stream_with_preloaded_download_context_does_not_reload_state_db() {
    let db = Arc::new(FailingDownloadStore::new(0));
    let dyn_db: Arc<dyn DownloadStore> = db.clone();
    let mut raw_config = DownloadConfig::test_default();
    raw_config.state_db = Some(dyn_db);
    let config = Arc::new(raw_config);
    let preloaded = preload_download_context(&config).await;
    assert_eq!(
        db.downloaded_state_load_count(),
        1,
        "preload should read downloaded state once"
    );

    let client = reqwest::Client::new();
    let stream = stream::empty::<anyhow::Result<PhotoAsset>>();
    stream_and_download_from_stream_with_context(
        &client,
        stream,
        &config,
        DownloadControls::download_hidden(),
        0,
        CancellationToken::new(),
        StreamRuntime::with_context(None, None, Some(preloaded)),
    )
    .await
    .expect("empty stream should complete");

    assert_eq!(
        db.downloaded_state_load_count(),
        1,
        "stream should reuse the preloaded context instead of reloading the DB"
    );
}
