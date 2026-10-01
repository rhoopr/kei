use std::sync::Arc;
use std::time::Duration;

use reqwest::Client;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

use crate::download::filter::{DownloadTask, MetadataPayload};
use crate::download::finalize::STATE_DB_UNWRITABLE_THRESHOLD;
use crate::download::metadata_rewrite::MetadataFlags;
use crate::download::pipeline::pass::{PassConfig, run_download_pass};
use crate::download::pipeline::streaming::{StreamRuntime, stream_and_download_from_stream};
use crate::download::pipeline::test_support::FailingDownloadStore;
use crate::download::{DownloadControls, DownloadReporting, DownloadStore};
use crate::retry::RetryConfig;
use crate::state::VersionSizeKey;
use crate::test_helpers::TestPhotoAsset;

#[test]
fn pass_config_debug_keeps_runtime_handles_out_of_output() {
    let client = reqwest::Client::new();
    let retry_config = RetryConfig::default();
    let config = PassConfig {
        client: &client,
        retry_config: &retry_config,
        metadata: MetadataFlags::DATETIME | MetadataFlags::DESCRIPTION,
        mark_capture_repair_after_download: false,
        concurrency: 3,
        reporting: DownloadReporting::hidden(),
        temp_suffix: Arc::from(".part"),
        shutdown_token: CancellationToken::new(),
        state_db: None,
        rate_limit_counter: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        bandwidth_limiter: None,
        library: Arc::from("PrimarySync"),
    };

    let rendered = format!("{config:?}");
    assert!(rendered.contains("metadata"));
    assert!(rendered.contains("concurrency: 3"));
    assert!(rendered.contains("temp_suffix: \".part\""));
    assert!(!rendered.contains("client"));
    assert!(!rendered.contains("retry_config"));
    assert!(!rendered.contains("shutdown_token"));
}

// These tests need a larger stack due to large async futures from reqwest
// and stream combinators. We spawn them on a thread with 8 MiB stack.
#[test]
fn test_run_download_pass_skips_all_tasks_when_cancelled() {
    std::thread::Builder::new()
        .stack_size(8 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let dir = TempDir::new().unwrap();
                    let token = CancellationToken::new();
                    token.cancel();

                    let tasks = vec![
                        DownloadTask {
                            url: "https://p01.icloud-content.com/a".into(),
                            download_path: dir.path().join("a.jpg"),
                            publication: crate::download::file::FinalPublication::NoReplace,
                            checksum: "aaa".into(),
                            created_local: chrono::Local::now().fixed_offset(),
                            size: 1000,
                            asset_id: "ASSET_A".into(),
                            asset_record_name: "ASSET_A".into(),
                            library: "PrimarySync".into(),
                            metadata: Arc::new(MetadataPayload::default()),
                            version_size: VersionSizeKey::Original,
                            media_type: crate::state::MediaType::Photo,
                        },
                        DownloadTask {
                            url: "https://p01.icloud-content.com/b".into(),
                            download_path: dir.path().join("b.jpg"),
                            publication: crate::download::file::FinalPublication::NoReplace,
                            checksum: "bbb".into(),
                            created_local: chrono::Local::now().fixed_offset(),
                            size: 2000,
                            asset_id: "ASSET_B".into(),
                            asset_record_name: "ASSET_B".into(),
                            library: "PrimarySync".into(),
                            metadata: Arc::new(MetadataPayload::default()),
                            version_size: VersionSizeKey::Original,
                            media_type: crate::state::MediaType::Photo,
                        },
                    ];

                    let client = Client::new();
                    let retry = RetryConfig::default();

                    let pass_config = PassConfig {
                        client: &client,
                        retry_config: &retry,
                        metadata: MetadataFlags::default(),
                        mark_capture_repair_after_download: false,
                        concurrency: 1,
                        reporting: DownloadReporting::hidden(),
                        temp_suffix: std::sync::Arc::from(".kei-tmp"),
                        shutdown_token: token,
                        state_db: None,
                        rate_limit_counter: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                        bandwidth_limiter: None,
                        library: std::sync::Arc::from("PrimarySync"),
                    };
                    let result = run_download_pass(pass_config, tasks).await;
                    assert!(result.failed.is_empty());
                });
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn test_run_download_pass_processes_tasks_when_not_cancelled() {
    std::thread::Builder::new()
        .stack_size(8 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let dir = TempDir::new().unwrap();
                    let token = CancellationToken::new();

                    let tasks = vec![DownloadTask {
                        url: "https://0.0.0.0:1/nonexistent".into(),
                        download_path: dir.path().join("c.jpg"),
                        publication: crate::download::file::FinalPublication::NoReplace,
                        checksum: "ccc".into(),
                        created_local: chrono::Local::now().fixed_offset(),
                        size: 500,
                        asset_id: "ASSET_C".into(),
                        asset_record_name: "ASSET_C".into(),
                        library: "PrimarySync".into(),
                        metadata: Arc::new(MetadataPayload::default()),
                        version_size: VersionSizeKey::Original,
                        media_type: crate::state::MediaType::Photo,
                    }];

                    let client = Client::new();
                    let retry = RetryConfig {
                        max_retries: 0,
                        base_delay_secs: 0,
                        max_delay_secs: 0,
                    };

                    let pass_config = PassConfig {
                        client: &client,
                        retry_config: &retry,
                        metadata: MetadataFlags::default(),
                        mark_capture_repair_after_download: false,
                        concurrency: 1,
                        reporting: DownloadReporting::hidden(),
                        temp_suffix: std::sync::Arc::from(".kei-tmp"),
                        shutdown_token: token,
                        state_db: None,
                        rate_limit_counter: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                        bandwidth_limiter: None,
                        library: std::sync::Arc::from("PrimarySync"),
                    };
                    let result = run_download_pass(pass_config, tasks).await;
                    assert_eq!(result.failed.len(), 1);
                });
        })
        .unwrap()
        .join()
        .unwrap();
}

#[tokio::test]
async fn download_pass_invalid_unknown_media_marks_failed_not_downloaded() {
    use base64::Engine as _;
    use wiremock::matchers::method;
    use wiremock::{Mock, ResponseTemplate};

    let server = crate::start_wiremock_or_skip!();
    let body = b"not media bytes";
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body.to_vec()))
        .mount(&server)
        .await;

    let dir = TempDir::new().unwrap();
    let db = Arc::new(FailingDownloadStore::with_mark_failed_tracking());
    let state_db: Arc<dyn DownloadStore> = db.clone();
    let client = Client::new();
    let retry = RetryConfig {
        max_retries: 0,
        base_delay_secs: 0,
        max_delay_secs: 0,
    };
    let checksum = base64::engine::general_purpose::STANDARD.encode([0x42u8; 32]);
    let download_path = dir.path().join("unknown_header.jpg");
    let part_path =
        crate::download::file::temp_download_path(&download_path, &checksum, ".kei-tmp")
            .expect("valid temp path");
    let task = DownloadTask {
        url: format!("{}/photo.jpg", server.uri()).into(),
        download_path: download_path.clone(),
        publication: crate::download::file::FinalPublication::NoReplace,
        checksum: checksum.into(),
        asset_id: "UNKNOWN_MEDIA".into(),
        asset_record_name: "UNKNOWN_MEDIA".into(),
        library: "PrimarySync".into(),
        metadata: Arc::new(MetadataPayload::default()),
        size: body.len() as u64,
        created_local: chrono::Local::now().fixed_offset(),
        version_size: VersionSizeKey::Original,
        media_type: crate::state::MediaType::Photo,
    };

    let result = run_download_pass(
        PassConfig {
            client: &client,
            retry_config: &retry,
            metadata: MetadataFlags::default(),
            mark_capture_repair_after_download: false,
            concurrency: 1,
            reporting: DownloadReporting::hidden(),
            temp_suffix: std::sync::Arc::from(".kei-tmp"),
            shutdown_token: CancellationToken::new(),
            state_db: Some(state_db),
            rate_limit_counter: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            bandwidth_limiter: None,
            library: std::sync::Arc::from("PrimarySync"),
        },
        vec![task],
    )
    .await;

    assert_eq!(result.failed.len(), 1);
    assert_eq!(db.call_count(), 0, "invalid media must not mark_downloaded");
    assert_eq!(
        db.failed_call_count(),
        1,
        "invalid media should be recorded failed"
    );
    assert!(
        !download_path.exists(),
        "invalid media must not publish final path"
    );
    assert!(!part_path.exists(), "invalid media .part should be removed");
    let outcome = crate::download::DownloadOutcome::PartialFailure {
        failed_count: result.failed.len(),
    };
    assert!(
        !crate::sync_cycle::should_store_sync_token(&outcome, false),
        "partial media-download failures must block sync-token advancement"
    );
}

#[tokio::test(start_paused = true)]
async fn download_pass_opens_state_write_circuit_breaker_mid_run() {
    use base64::Engine as _;
    use wiremock::matchers::method;
    use wiremock::{Mock, ResponseTemplate};

    let server = crate::start_wiremock_or_skip!();
    let jpeg_body = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(jpeg_body.clone())
                .insert_header("content-type", "image/jpeg"),
        )
        .mount(&server)
        .await;

    let dir = TempDir::new().unwrap();
    let db = Arc::new(FailingDownloadStore::new(usize::MAX / 2));
    let state_db: Arc<dyn DownloadStore> = db.clone();
    let client = Client::new();
    let retry = RetryConfig {
        max_retries: 0,
        base_delay_secs: 0,
        max_delay_secs: 0,
    };

    let checksum = base64::engine::general_purpose::STANDARD.encode([0x42u8; 32]);
    let tasks: Vec<DownloadTask> = (0..STATE_DB_UNWRITABLE_THRESHOLD + 3)
        .map(|i| DownloadTask {
            url: format!("{}/photo_{i}.jpg", server.uri()).into(),
            download_path: dir.path().join(format!("photo_{i}.jpg")),
            publication: crate::download::file::FinalPublication::NoReplace,
            checksum: checksum.clone().into(),
            asset_id: format!("CIRCUIT_{i}").into(),
            asset_record_name: format!("CIRCUIT_{i}").into(),
            library: "PrimarySync".into(),
            metadata: Arc::new(MetadataPayload::default()),
            size: jpeg_body.len() as u64,
            created_local: chrono::Local::now().fixed_offset(),
            version_size: VersionSizeKey::Original,
            media_type: crate::state::MediaType::Photo,
        })
        .collect();

    let result = run_download_pass(
        PassConfig {
            client: &client,
            retry_config: &retry,
            metadata: MetadataFlags::default(),
            mark_capture_repair_after_download: false,
            concurrency: 1,
            reporting: DownloadReporting::hidden(),
            temp_suffix: std::sync::Arc::from(".kei-tmp"),
            shutdown_token: CancellationToken::new(),
            state_db: Some(state_db),
            rate_limit_counter: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            bandwidth_limiter: None,
            library: std::sync::Arc::from("PrimarySync"),
        },
        tasks,
    )
    .await;

    assert_eq!(result.state_write_failures, STATE_DB_UNWRITABLE_THRESHOLD);
    assert_eq!(db.success_count(), 0);
    assert!(
        db.call_count() > STATE_DB_UNWRITABLE_THRESHOLD,
        "deferred writes must be retried before opening the circuit"
    );
    for i in 0..STATE_DB_UNWRITABLE_THRESHOLD {
        let landed = dir.path().join(format!("photo_{i}.jpg"));
        assert!(
            landed.exists(),
            "state write failures must not remove an already-published file: {}",
            landed.display()
        );
    }
}

/// When a CancellationToken is already cancelled as the next asset is
/// yielded, the pass must stop before planning or downloading that asset.
#[tokio::test]
async fn shutdown_cancellation_exits_download_pass_promptly() {
    use crate::download::{DownloadConfig, SyncMode};
    use crate::icloud::photos::PhotoAsset;
    use crate::types::{
        AssetVersionSize, FileMatchPolicy, LivePhotoMode, LivePhotoMovFilenamePolicy, RawPolicy,
    };
    use rustc_hash::FxHashSet;

    let asset_stream = futures_util::stream::repeat_with(|| {
        Ok::<PhotoAsset, anyhow::Error>(
            TestPhotoAsset::new("SHUTDOWN_ALREADY_CANCELLED")
                .orig_size(100)
                .orig_url("http://127.0.0.1:1/photo.jpg")
                .orig_checksum("ck_shutdown")
                .build(),
        )
    });

    let dir = TempDir::new().unwrap();

    let config = Arc::new(DownloadConfig {
        directory: std::sync::Arc::from(dir.path()),
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
        concurrent_downloads: 10,
        recent: None,
        recent_scope: crate::cli::RecentScope::Global,
        retry: crate::retry::RetryConfig {
            max_retries: 0,
            base_delay_secs: 0,
            max_delay_secs: 0,
        },
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

    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_millis(50))
        .build()
        .expect("client");

    let shutdown_token = CancellationToken::new();
    shutdown_token.cancel();

    let result = stream_and_download_from_stream(
        &client,
        asset_stream,
        &config,
        DownloadControls::download_hidden(),
        10_000,
        shutdown_token,
        StreamRuntime::new(None, None),
    )
    .await
    .expect("cancelled pass should return a streaming result");

    assert_eq!(result.downloaded, 0, "cancelled pass must not download");
    assert!(
        !result.enumeration_complete,
        "cancelled enumeration must not be considered complete"
    );
}
