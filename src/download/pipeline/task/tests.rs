use crate::download::pipeline::{
    StreamPipelineShared,
    consumer::{StreamConsumerSettings, consume_stream_download_tasks},
};
use crate::download::planner::{self, TaskPlanner};
use crate::test_helpers::TestPhotoAsset;
use futures_util::stream;
use indicatif::ProgressBar;
use std::fs;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::time::{Duration, UNIX_EPOCH};
use tokio::sync::mpsc;

use anyhow::Result;
use base64::Engine as _;
use reqwest::Client;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

#[cfg(any(unix, feature = "xmp"))]
use crate::download::DownloadStore;
use crate::download::error::DownloadError;
use crate::download::filter::{DownloadTask, MetadataPayload};
use crate::download::metadata_rewrite::MetadataFlags;
use crate::download::pipeline::pass::{PassConfig, run_download_pass};
use crate::download::pipeline::streaming::{StreamRuntime, stream_and_download_from_stream};
use crate::download::pipeline::task::{
    DownloadSingleContext, DownloadTaskErrorClass, classify_download_task_error,
    download_single_task, set_file_mtime,
};
#[cfg(any(unix, feature = "xmp"))]
use crate::download::pipeline::test_support::MINIMAL_JPEG;
#[cfg(feature = "xmp")]
use crate::download::pipeline::test_support::sidecar_path_for;
use crate::download::{DownloadConfig, DownloadControls, DownloadReporting};
use crate::retry::RetryConfig;
#[cfg(any(unix, feature = "xmp"))]
use crate::state::AssetRecord;
use crate::state::VersionSizeKey;

fn classify_download_task_error_for(err: DownloadError) -> DownloadTaskErrorClass {
    let err = anyhow::Error::new(err);
    classify_download_task_error(&err)
}

struct StaticDownloadClient {
    body: Vec<u8>,
}

#[async_trait::async_trait]
impl crate::download::file::DownloadClient for StaticDownloadClient {
    async fn fetch(
        &self,
        _: &str,
        _: Option<u64>,
    ) -> Result<crate::download::file::DownloadResponse, Box<dyn std::error::Error + Send + Sync>>
    {
        let body = bytes::Bytes::copy_from_slice(&self.body);
        Ok(crate::download::file::DownloadResponse {
            status: 200,
            content_length: Some(body.len() as u64),
            content_range: None,
            content_type: Some("image/jpeg".to_string()),
            stream: Box::pin(futures_util::stream::once(async move { Ok(body) })),
        })
    }
}

#[test]
fn classify_download_task_error_detects_interrupted_download() {
    assert_eq!(
        classify_download_task_error_for(DownloadError::Interrupted {
            path: "photo.jpg".into(),
            bytes_written: 12,
        }),
        DownloadTaskErrorClass::Interrupted
    );
}

#[test]
fn classify_download_task_error_detects_session_expiry_and_expired_url() {
    assert_eq!(
        classify_download_task_error_for(DownloadError::HttpStatus {
            status: 401,
            path: "photo.jpg".into(),
        }),
        DownloadTaskErrorClass::SessionExpired
    );
    assert_eq!(
        classify_download_task_error_for(DownloadError::HttpStatus {
            status: 403,
            path: "photo.jpg".into(),
        }),
        DownloadTaskErrorClass::SessionExpired
    );
    assert_eq!(
        classify_download_task_error_for(DownloadError::HttpStatus {
            status: 410,
            path: "photo.jpg".into(),
        }),
        DownloadTaskErrorClass::ExpiredUrl
    );
}

#[test]
fn classify_download_task_error_treats_ordinary_errors_as_other() {
    assert_eq!(
        classify_download_task_error_for(DownloadError::InvalidContent {
            path: "photo.jpg".into(),
            reason: "not a photo".into(),
        }),
        DownloadTaskErrorClass::Other
    );

    let plain = anyhow::anyhow!("plain failure");
    assert_eq!(
        classify_download_task_error(&plain),
        DownloadTaskErrorClass::Other
    );
}

#[test]
fn classify_download_task_error_detects_context_wrapped_download_errors() {
    let interrupted = anyhow::Error::new(DownloadError::Interrupted {
        path: "photo.jpg".into(),
        bytes_written: 12,
    })
    .context("worker");
    assert_eq!(
        classify_download_task_error(&interrupted),
        DownloadTaskErrorClass::Interrupted
    );

    let session_expired = anyhow::Error::new(DownloadError::HttpStatus {
        status: 403,
        path: "photo.jpg".into(),
    })
    .context("worker");
    assert_eq!(
        classify_download_task_error(&session_expired),
        DownloadTaskErrorClass::SessionExpired
    );

    let expired_url = anyhow::Error::new(DownloadError::HttpStatus {
        status: 410,
        path: "photo.jpg".into(),
    })
    .context("worker");
    assert_eq!(
        classify_download_task_error(&expired_url),
        DownloadTaskErrorClass::ExpiredUrl
    );
}

#[test]
fn test_set_file_mtime_positive_timestamp() {
    let dir = TempDir::new().unwrap();
    let p = dir.path().join("pos.txt");
    fs::write(&p, b"test").unwrap();
    set_file_mtime(&p, 1_700_000_000).unwrap();
    let meta = fs::metadata(&p).unwrap();
    let mtime = meta.modified().unwrap();
    assert_eq!(mtime, UNIX_EPOCH + Duration::from_secs(1_700_000_000));
}

#[test]
fn test_set_file_mtime_zero_timestamp() {
    let dir = TempDir::new().unwrap();
    let p = dir.path().join("zero.txt");
    fs::write(&p, b"test").unwrap();
    set_file_mtime(&p, 0).unwrap();
    let meta = fs::metadata(&p).unwrap();
    let mtime = meta.modified().unwrap();
    assert_eq!(mtime, UNIX_EPOCH);
}

#[test]
fn test_set_file_mtime_negative_timestamp() {
    let dir = TempDir::new().unwrap();
    let p = dir.path().join("neg.txt");
    fs::write(&p, b"test").unwrap();
    // Should not panic — clamps or uses pre-epoch time
    set_file_mtime(&p, -86400).unwrap();
}

#[test]
fn test_set_file_mtime_nonexistent_file() {
    let dir = TempDir::new().unwrap();
    let p = dir.path().join("nonexistent_file.txt");
    assert!(set_file_mtime(&p, 0).is_err());
}

#[cfg(any(unix, feature = "xmp"))]
#[tokio::test]
async fn different_byte_destination_race_keeps_loser_failed_without_metadata_write() {
    use crate::state::{MediaType, SqliteStateDb};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    let winner_body = MINIMAL_JPEG.to_vec();
    let mut loser_body = MINIMAL_JPEG.to_vec();
    *loser_body
        .get_mut(17)
        .expect("minimal JPEG must contain a Y-density value") = 2;
    let server = crate::start_wiremock_or_skip!();
    Mock::given(method("GET"))
        .and(path("/WINNER.jpg"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(winner_body.clone())
                .insert_header("content-type", "image/jpeg"),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/LOSER.jpg"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(loser_body.clone())
                .insert_header("content-type", "image/jpeg"),
        )
        .mount(&server)
        .await;

    let dir = TempDir::new().unwrap();
    let download_path = dir.path().join("shared.jpg");
    let winner_db = Arc::new(
        SqliteStateDb::open(&dir.path().join("winner.db"))
            .await
            .unwrap(),
    );
    let loser_path = dir.path().join("loser.db");
    let loser_db = Arc::new(SqliteStateDb::open(&loser_path).await.unwrap());
    let retry = RetryConfig {
        max_retries: 0,
        base_delay_secs: 0,
        max_delay_secs: 0,
    };
    let metadata = MetadataFlags::from(&crate::config::MetadataConfig {
        #[cfg(feature = "xmp")]
        xmp_sidecar: true,
        ..crate::config::MetadataConfig::default()
    });
    let make_task = |asset_id: &str, checksum_byte: u8, rating: u8| DownloadTask {
        require_created_entry: true,
        url: format!("{}/{asset_id}.jpg", server.uri()).into(),
        download_path: download_path.clone(),
        replacement_fingerprint: None,
        publication_binding: None,
        pending_cross_parent_root: None,
        superseded_requested_path: None,
        checksum: base64::engine::general_purpose::STANDARD
            .encode([checksum_byte; 32])
            .into(),
        asset_id: asset_id.into(),
        asset_record_name: asset_id.into(),
        library: "PrimarySync".into(),
        metadata: Arc::new(MetadataPayload {
            rating: Some(rating),
            ..MetadataPayload::default()
        }),
        size: winner_body.len() as u64,
        created_local: chrono::Local::now().fixed_offset(),
        version_size: VersionSizeKey::Original,
        media_type: MediaType::Photo,
    };
    let winner_task = make_task("WINNER", 0x41, 1);
    let loser_task = make_task("LOSER", 0x42, 5);
    let loser_part_path =
        crate::download::file::temp_download_path(&download_path, &loser_task.checksum, ".kei-tmp")
            .expect("valid temporary path");

    for (db, task) in [(&winner_db, &winner_task), (&loser_db, &loser_task)] {
        db.upsert_seen(&AssetRecord::new_pending(
            Arc::from("PrimarySync"),
            task.asset_id.to_string(),
            task.version_size,
            task.checksum.to_string(),
            "shared.jpg".to_string(),
            chrono::Utc::now(),
            None,
            task.size,
            MediaType::Photo,
        ))
        .await
        .unwrap();
    }

    let client = Client::new();
    let winner_store: Arc<dyn DownloadStore> = winner_db.clone();
    let winner_result = run_download_pass(
        PassConfig {
            prior_auth_errors: 0,
            url_obtained_at: Default::default(),
            client: &client,
            retry_config: &retry,
            metadata,
            mark_capture_repair_after_download: false,
            concurrency: 1,
            reporting: DownloadReporting::hidden(),
            temp_suffix: Arc::from(".kei-tmp"),
            shutdown_token: CancellationToken::new(),
            state_db: Some(winner_store),
            rate_limit_counter: Arc::new(AtomicUsize::new(0)),
            bandwidth_limiter: None,
            library: Arc::from("PrimarySync"),
            selection_run: None,
            selection_pass: None,
        },
        vec![winner_task],
    )
    .await;
    assert_eq!(winner_result.downloaded, 1);
    assert!(winner_result.failed.is_empty());

    #[cfg(feature = "xmp")]
    let sidecar_path = sidecar_path_for(&download_path);
    #[cfg(feature = "xmp")]
    let winner_sidecar = fs::read(&sidecar_path).expect("winner sidecar must be written");
    let loser_store: Arc<dyn DownloadStore> = loser_db.clone();
    let loser_result = run_download_pass(
        PassConfig {
            prior_auth_errors: 0,
            url_obtained_at: Default::default(),
            client: &client,
            retry_config: &retry,
            metadata,
            mark_capture_repair_after_download: false,
            concurrency: 1,
            reporting: DownloadReporting::hidden(),
            temp_suffix: Arc::from(".kei-tmp"),
            shutdown_token: CancellationToken::new(),
            state_db: Some(loser_store),
            rate_limit_counter: Arc::new(AtomicUsize::new(0)),
            bandwidth_limiter: None,
            library: Arc::from("PrimarySync"),
            selection_run: None,
            selection_pass: None,
        },
        vec![loser_task],
    )
    .await;

    assert_eq!(loser_result.downloaded, 0);
    assert_eq!(loser_result.failed.len(), 1);
    assert_eq!(fs::read(&download_path).unwrap(), winner_body);
    #[cfg(feature = "xmp")]
    assert_eq!(fs::read(&sidecar_path).unwrap(), winner_sidecar);
    assert_eq!(fs::read(&loser_part_path).unwrap(), loser_body);
    assert!(
        loser_db
            .get_downloaded_page(0, 10)
            .await
            .unwrap()
            .is_empty()
    );
    drop(loser_db);
    let reopened = SqliteStateDb::open(&loser_path).await.unwrap();
    assert!(
        reopened
            .get_downloaded_page(0, 10)
            .await
            .unwrap()
            .is_empty()
    );
    let failed = reopened.get_failed().await.unwrap();
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].id.as_ref(), "LOSER");
    let error = failed[0].last_error.as_deref().unwrap();
    assert!(error.contains("Destination already exists"));
    assert!(
        error.contains("cannot establish publication ownership"),
        "create-only collision must remain durable failed work"
    );
    assert_ne!(winner_body, loser_body);
    assert_eq!(fs::read(&download_path).unwrap(), winner_body);
    assert_eq!(fs::read(&loser_part_path).unwrap(), loser_body);
}

#[tokio::test]
async fn full_expired_url_cleanup_recovers_queued_tasks_but_respects_shutdown() {
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};
    for user_shutdown in [false, true] {
        let server = crate::start_wiremock_or_skip!();
        let body = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];
        let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&body));
        Mock::given(method("GET"))
            .and(path("/expired.jpg"))
            .respond_with(ResponseTemplate::new(410))
            .expect(if user_shutdown { 0 } else { 2 })
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/fresh.jpg"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body.clone()))
            .expect(if user_shutdown { 0 } else { 2 })
            .mount(&server)
            .await;
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("state.db");
        let db = Arc::new(crate::state::SqliteStateDb::open(&db_path).await.unwrap());
        let mut config = DownloadConfig::test_default();
        config.directory = Arc::from(dir.path().join("media"));
        config.state_db = Some(db.clone());
        let config = Arc::new(config);
        let assets: Vec<_> = ["FIRST", "QUEUED"]
            .into_iter()
            .map(|id| {
                TestPhotoAsset::new(id)
                    .filename(&format!("{id}.jpg"))
                    .orig_size(body.len() as u64)
                    .orig_url(&format!("{}/expired.jpg", server.uri()))
                    .orig_checksum(&checksum)
                    .build()
            })
            .collect();
        let mut planner = TaskPlanner::for_download(Some(db.as_ref())).await.unwrap();
        let (tx, rx) = mpsc::channel(assets.len());
        for asset in &assets {
            let plan = planner.plan_download_asset(asset, &config).await.unwrap();
            assert_eq!(plan.tasks.len(), 1);
            for task in plan.tasks {
                planner::upsert_seen_for_task(db.as_ref(), &config, asset, &task)
                    .await
                    .unwrap();
                tx.send(task).await.unwrap();
            }
        }
        drop(tx);
        let shutdown = CancellationToken::new();
        if user_shutdown {
            shutdown.cancel();
        }
        let client = Client::new();
        let result = consume_stream_download_tasks(
            rx,
            client.clone(),
            StreamPipelineShared {
                config: config.clone(),
                state_db: config.state_db.clone(),
                pb: ProgressBar::hidden(),
                pipeline_shutdown: shutdown.child_token(),
            },
            StreamConsumerSettings {
                retry_config: config.retry,
                metadata_flags: MetadataFlags::default(),
                concurrency: 1,
                mode: crate::personality::Mode::Off,
                bytes_counter: Arc::new(AtomicU64::new(0)),
            },
        )
        .await;
        assert_eq!(result.url_expired_abort, !user_shutdown);
        assert_eq!(result.failed.len(), if user_shutdown { 0 } else { 2 });
        assert_eq!(db.get_summary().await.unwrap().pending, 2);
        if user_shutdown {
            server.verify().await;
            continue;
        }
        assert!(
            !shutdown.is_cancelled(),
            "URL expiry must not cancel the user token"
        );
        let fresh = result
            .failed
            .into_iter()
            .map(|task| DownloadTask {
                require_created_entry: true,
                url: format!("{}/fresh.jpg", server.uri()).into(),
                ..task
            })
            .collect();
        let recovered = run_download_pass(
            PassConfig {
                prior_auth_errors: 0,
                url_obtained_at: Default::default(),
                client: &client,
                retry_config: &config.retry,
                metadata: MetadataFlags::default(),
                mark_capture_repair_after_download: false,
                concurrency: 1,
                reporting: DownloadReporting::hidden(),
                temp_suffix: config.temp_suffix.clone(),
                shutdown_token: shutdown.clone(),
                state_db: config.state_db.clone(),
                rate_limit_counter: Arc::new(AtomicUsize::new(0)),
                bandwidth_limiter: None,
                library: config.library.clone(),
                selection_run: None,
                selection_pass: None,
            },
            fresh,
        )
        .await;
        assert_eq!(recovered.downloaded, 2);
        assert!(recovered.failed.is_empty());
        let reopened = crate::state::SqliteStateDb::open(&db_path).await.unwrap();
        assert_eq!(reopened.get_summary().await.unwrap().downloaded, 2);
        for row in reopened.get_downloaded_page(0, 10).await.unwrap() {
            assert_eq!(fs::read(row.local_path.as_ref().unwrap()).unwrap(), body);
        }
        let stable = stream_and_download_from_stream(
            &client,
            stream::iter(assets.into_iter().map(Ok)),
            &config,
            DownloadControls::download_hidden(),
            2,
            shutdown,
            StreamRuntime::new(None, None),
        )
        .await
        .unwrap();
        assert_eq!(stable.downloaded, 0);
        assert!(stable.failed.is_empty());
        server.verify().await;
    }
}
#[tokio::test]
async fn unscoped_publication_proof_survives_finalization_and_other_transfer_interruption() {
    use crate::state::db::TempFileOwnershipStore;

    let jpeg_body = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];
    let client = StaticDownloadClient {
        body: jpeg_body.clone(),
    };

    let dir = TempDir::new().unwrap();
    let db = crate::state::SqliteStateDb::open_in_memory().unwrap();
    let retry = RetryConfig {
        max_retries: 0,
        base_delay_secs: 0,
        max_delay_secs: 0,
    };
    let make_task = |name: &str, checksum_byte: u8| DownloadTask {
        require_created_entry: true,
        url: format!("https://example.invalid/{name}.jpg").into(),
        download_path: dir.path().join(format!("{name}.jpg")),
        replacement_fingerprint: None,
        publication_binding: None,
        pending_cross_parent_root: None,
        superseded_requested_path: None,
        checksum: base64::engine::general_purpose::STANDARD
            .encode([checksum_byte; 32])
            .into(),
        asset_id: name.into(),
        asset_record_name: name.into(),
        library: "PrimarySync".into(),
        metadata: Arc::new(MetadataPayload::default()),
        size: jpeg_body.len() as u64,
        created_local: chrono::Local::now().fixed_offset(),
        version_size: VersionSizeKey::Original,
        media_type: crate::state::MediaType::Photo,
    };
    let published = make_task("published", 0x41);
    let running = CancellationToken::new();
    db.upsert_seen(&crate::state::AssetRecord::new_pending(
        published.library.clone(),
        published.asset_id.to_string(),
        published.version_size,
        published.checksum.to_string(),
        "published.jpg".into(),
        published.created_local.with_timezone(&chrono::Utc),
        None,
        published.size,
        published.media_type,
    ))
    .await
    .unwrap();

    let (exif_ok, local_checksum, source_checksum, _, _, retained) = download_single_task(
        &client,
        &published,
        &retry,
        MetadataFlags::default(),
        DownloadSingleContext {
            temp_suffix: ".kei-tmp",
            state_db: Some(&db),
            rate_limit_counter: None,
            bandwidth_limiter: None,
            shutdown_token: &running,
            mode: crate::personality::Mode::Off,
        },
    )
    .await
    .unwrap();
    assert!(published.download_path.exists());
    assert_eq!(
        db.get_prepared_publications(
            &published.library,
            &published.asset_id,
            published.version_size
        )
        .await
        .unwrap()
        .len(),
        1,
        "published bytes retain their proof until the durable receipt commits"
    );
    assert!(matches!(
        crate::download::finalize::finalize_downloaded_with_proof(
            &db,
            &published.library,
            &published,
            local_checksum,
            source_checksum,
            (exif_ok, 0),
            false,
            retained,
        )
        .await,
        crate::download::finalize::DownloadedFinalization::Persisted
    ));
    assert_eq!(
        db.get_prepared_publications(
            &published.library,
            &published.asset_id,
            published.version_size
        )
        .await
        .unwrap()
        .len(),
        1,
        "an unscoped legacy proof cannot retire without a scoped successor receipt"
    );

    let interrupted = make_task("interrupted", 0x42);
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let error = download_single_task(
        &client,
        &interrupted,
        &retry,
        MetadataFlags::default(),
        DownloadSingleContext {
            temp_suffix: ".kei-tmp",
            state_db: Some(&db),
            rate_limit_counter: None,
            bandwidth_limiter: None,
            shutdown_token: &cancelled,
            mode: crate::personality::Mode::Off,
        },
    )
    .await
    .unwrap_err();
    assert_eq!(
        classify_download_task_error(&error),
        DownloadTaskErrorClass::Interrupted
    );
    assert_eq!(
        db.get_prepared_publications(
            &published.library,
            &published.asset_id,
            published.version_size
        )
        .await
        .unwrap()
        .len(),
        1,
        "cancelled transfer preserves the unrelated published proof"
    );
    assert!(
        db.get_owned_temp_files_before(i64::MAX)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn truncated_task_retires_ownership_after_settled_prefix_and_resumes() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let exercise = async {
        let body = [0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10, 0x4a, 0x46];
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let serve = tokio::spawn(async move {
            for resumed in [false, true] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    assert!(
                        request.len() < 4096,
                        "fixture request headers exceeded bound"
                    );
                    let mut byte = [0];
                    assert_eq!(socket.read(&mut byte).await.unwrap(), 1);
                    request.extend_from_slice(&byte);
                }
                let request = String::from_utf8(request).unwrap().to_ascii_lowercase();
                assert!(request.starts_with("get /settled.jpg http/1.1\r\n"));
                assert_eq!(request.contains("\r\nrange: bytes=4-\r\n"), resumed);
                let headers = if resumed {
                    "HTTP/1.1 206 Partial Content\r\nContent-Length: 4\r\nContent-Range: bytes 4-7/8\r\nContent-Type: image/jpeg\r\nConnection: close\r\n\r\n"
                } else {
                    "HTTP/1.1 200 OK\r\nContent-Length: 8\r\nContent-Type: image/jpeg\r\nConnection: close\r\n\r\n"
                };
                socket.write_all(headers.as_bytes()).await.unwrap();
                socket
                    .write_all(if resumed { &body[4..] } else { &body[..4] })
                    .await
                    .unwrap();
                socket.shutdown().await.unwrap();
            }
        });
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("state.db");
        let db = Arc::new(crate::state::SqliteStateDb::open(&db_path).await.unwrap());
        let mut config = DownloadConfig::test_default();
        config.directory = Arc::from(dir.path().join("media"));
        config.state_db = Some(db.clone());
        config.retry = RetryConfig {
            max_retries: 0,
            base_delay_secs: 0,
            max_delay_secs: 0,
        };
        let config = Arc::new(config);
        let checksum = base64::engine::general_purpose::STANDARD.encode([0x74; 32]);
        let asset = TestPhotoAsset::new("SETTLED")
            .filename("settled.jpg")
            .orig_size(body.len() as u64)
            .orig_url(&format!("http://{address}/settled.jpg"))
            .orig_checksum(&checksum)
            .build();
        let mut planner = TaskPlanner::for_download(Some(db.as_ref())).await.unwrap();
        let plan = planner.plan_download_asset(&asset, &config).await.unwrap();
        assert_eq!(plan.tasks.len(), 1);
        let task = &plan.tasks[0];
        let final_path = task.download_path.clone();
        let part = crate::download::file::temp_download_path(
            &final_path,
            &task.checksum,
            &config.temp_suffix,
        )
        .unwrap();
        let client = Client::builder().no_proxy().build().unwrap();
        for cycle in 0..3 {
            let result = stream_and_download_from_stream(
                &client,
                stream::iter(vec![Ok(asset.clone())]),
                &config,
                DownloadControls::download_hidden(),
                1,
                CancellationToken::new(),
                StreamRuntime::new(None, None),
            )
            .await
            .unwrap();
            let reopened = crate::state::SqliteStateDb::open(&db_path).await.unwrap();
            assert!(
                reopened
                    .get_owned_temp_files_before(i64::MAX)
                    .await
                    .unwrap()
                    .is_empty(),
                "a stopped or completed transfer must retire its temporary claim"
            );
            if cycle == 0 {
                assert_eq!(result.downloaded, 0);
                assert_eq!(result.failed.len(), 1);
                assert!(!final_path.exists());
                assert_eq!(fs::read(&part).unwrap(), body[..4]);
                assert!(
                    reopened
                        .get_downloaded_page(0, 10)
                        .await
                        .unwrap()
                        .is_empty()
                );
                assert_eq!(reopened.get_failed().await.unwrap().len(), 1);
            } else {
                assert_eq!(result.downloaded, usize::from(cycle == 1));
                assert!(result.failed.is_empty());
                assert_eq!(fs::read(&final_path).unwrap(), body);
                assert!(!part.exists());
                assert_eq!(reopened.get_downloaded_page(0, 10).await.unwrap().len(), 1);
                assert!(reopened.get_failed().await.unwrap().is_empty());
            }
        }
        serve.await.unwrap();
    };
    tokio::time::timeout(Duration::from_secs(10), exercise)
        .await
        .expect("offline truncation, resume and unchanged cycles must be bounded");
}

#[cfg(feature = "xmp")]
#[derive(Clone)]
struct GpsPhotosSession {
    records: Arc<Vec<serde_json::Value>>,
    next_token: Arc<std::sync::atomic::AtomicUsize>,
}

#[cfg(feature = "xmp")]
#[async_trait::async_trait]
impl crate::icloud::photos::PhotosSession for GpsPhotosSession {
    async fn post(
        &self,
        url: &str,
        body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<serde_json::Value> {
        use std::sync::atomic::Ordering;

        let request: serde_json::Value = serde_json::from_str(&body).map_err(|error| {
            anyhow::anyhow!("invalid Photos request body: {error}; body={body}")
        })?;
        if url.contains("/internal/records/query/batch") {
            anyhow::ensure!(
                request["batch"].is_array(),
                "unexpected count request body: {body}"
            );
            return Ok(serde_json::json!({
                "batch": [{"records": [{"fields": {"itemCount": {"value": 1}}}]}]
            }));
        }
        if url.contains("/records/query?") {
            let zone = request["query"]["zoneID"]["zoneName"]
                .as_str()
                .or_else(|| request["zoneID"]["zoneName"].as_str())
                .ok_or_else(|| anyhow::anyhow!("records/query missing zone: {body}"))?;
            anyhow::ensure!(zone == "PrimarySync", "records/query wrong zone: {body}");
            let offset = request["query"]["filterBy"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|filter| filter["fieldName"] == "startRank")
                .and_then(|filter| filter["fieldValue"]["value"].as_u64())
                .unwrap_or(0);
            let records = if offset == 0 {
                self.records.as_ref().clone()
            } else {
                Vec::new()
            };
            return Ok(serde_json::json!({
                "records": records,
                "syncToken": "full-token"
            }));
        }
        if url.contains("/changes/zone?") {
            let zone = request["zones"][0]["zoneID"]["zoneName"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("changes/zone missing zone: {body}"))?;
            anyhow::ensure!(zone == "PrimarySync", "changes/zone wrong zone: {body}");
            anyhow::ensure!(
                request["zones"][0]["syncToken"].is_string(),
                "changes/zone missing requested token: {body}"
            );
            let token = self.next_token.fetch_add(1, Ordering::SeqCst);
            return Ok(serde_json::json!({
                "zones": [{
                    "zoneID": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"},
                    "syncToken": format!("delta-token-{token}"),
                    "moreComing": false,
                    "records": []
                }]
            }));
        }
        if url.contains("/records/lookup?") {
            let zone = request["zoneID"]["zoneName"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("records/lookup missing zone: {body}"))?;
            anyhow::ensure!(zone == "PrimarySync", "records/lookup wrong zone: {body}");
            let requested: Vec<&str> = request["records"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|record| record["recordName"].as_str())
                .collect();
            anyhow::ensure!(!requested.is_empty(), "records/lookup no names: {body}");
            let selected: Vec<serde_json::Value> = self
                .records
                .iter()
                .filter(|record| {
                    requested.contains(&record["recordName"].as_str().unwrap_or_default())
                })
                .cloned()
                .collect();
            return Ok(serde_json::json!({"records": selected}));
        }
        anyhow::bail!("unexpected PhotosSession route: {url}; body={body}")
    }

    fn clone_box(&self) -> Box<dyn crate::icloud::photos::PhotosSession> {
        Box::new(self.clone())
    }
}

#[cfg(feature = "xmp")]
fn gps_provider_checksum(source: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    data_encoding::BASE64.encode(&Sha256::digest(source))
}

#[cfg(feature = "xmp")]
fn gps_records(source: &[u8], url: &str) -> Vec<serde_json::Value> {
    use serde_json::json;

    vec![
        json!({
            "recordName": "GPS_MASTER",
            "recordType": "CPLMaster",
            "fields": {
                "filenameEnc": {
                    "value": data_encoding::BASE64.encode(b"source.jpg"),
                    "type": "STRING"
                },
                "resOriginalRes": {
                    "value": {
                        "downloadURL": url,
                        "size": source.len(),
                        "fileChecksum": gps_provider_checksum(source)
                    }
                },
                "resOriginalWidth": {"value": 100, "type": "INT64"},
                "resOriginalHeight": {"value": 100, "type": "INT64"},
                "resOriginalFileType": {"value": "public.jpeg"},
                "itemType": {"value": "public.jpeg"},
                "adjustmentRenderType": {"value": 0, "type": "INT64"}
            },
            "recordChangeTag": "ct-master"
        }),
        json!({
            "recordName": "asset-GPS_CHILD",
            "recordType": "CPLAsset",
            "fields": {
                "masterRef": {
                    "value": {
                        "recordName": "GPS_MASTER",
                        "zoneID": {"zoneName": "PrimarySync"}
                    },
                    "type": "REFERENCE"
                },
                "assetDate": {"value": 1736899200000.0, "type": "TIMESTAMP"},
                "addedDate": {"value": 1736899200000.0, "type": "TIMESTAMP"},
                "locationLatitude": {"value": 1.5},
                "locationLongitude": {"value": 2.5}
            },
            "recordChangeTag": "ct-asset"
        }),
    ]
}

#[cfg(feature = "xmp")]
fn gps_pass(
    db: Arc<crate::state::SqliteStateDb>,
    owner: crate::state::db::account::AccountOwner,
    session: GpsPhotosSession,
) -> crate::commands::AlbumPass {
    use serde_json::json;

    let mut album = crate::icloud::photos::PhotoAlbum::new(
        crate::icloud::photos::PhotoAlbumConfig {
            params: Arc::new(std::collections::HashMap::new()),
            service_endpoint: Arc::from("https://example.com"),
            name: Arc::from(""),
            list_type: Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
            obj_type: Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
            query_filter: None,
            page_size: 100,
            zone_id: Arc::new(json!({
                "zoneName": "PrimarySync",
                "ownerRecordName": "_defaultOwner"
            })),
            retry_config: crate::retry::RetryConfig::default(),
            container_id: None,
            cross_zone_sources: Vec::new(),
        },
        Box::new(session),
    );
    album.set_shadow_capture(
        crate::icloud::photos::inbox::ShadowCapture::new(db, owner, "com"),
        Arc::from("private"),
    );
    crate::commands::AlbumPass {
        kind: crate::commands::PassKind::Unfiled,
        album,
        exclude_ids: Arc::default(),
    }
}

#[cfg(feature = "xmp")]
fn gps_plain_pass(session: GpsPhotosSession) -> crate::commands::AlbumPass {
    use serde_json::json;

    let album = crate::icloud::photos::PhotoAlbum::new(
        crate::icloud::photos::PhotoAlbumConfig {
            params: Arc::new(std::collections::HashMap::new()),
            service_endpoint: Arc::from("https://example.com"),
            name: Arc::from(""),
            list_type: Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
            obj_type: Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
            query_filter: None,
            page_size: 100,
            zone_id: Arc::new(json!({
                "zoneName": "PrimarySync",
                "ownerRecordName": "_defaultOwner"
            })),
            retry_config: crate::retry::RetryConfig::default(),
            container_id: None,
            cross_zone_sources: Vec::new(),
        },
        Box::new(session),
    );
    crate::commands::AlbumPass {
        kind: crate::commands::PassKind::Unfiled,
        album,
        exclude_ids: Arc::default(),
    }
}

#[cfg(feature = "xmp")]
fn gps_config(
    directory: &std::path::Path,
    db: Arc<crate::state::SqliteStateDb>,
    pass: &crate::commands::AlbumPass,
    embed: bool,
    refresh: bool,
    sync_mode: crate::download::SyncMode,
) -> Arc<crate::download::DownloadConfig> {
    let mut config = crate::download::DownloadConfig::test_default();
    config.directory = Arc::from(directory);
    config.library = Arc::from("PrimarySync");
    config.state_db = Some(db);
    config.metadata.set_exif_gps = embed;
    config.metadata.xmp_sidecar = true;
    config.refresh_metadata = refresh;
    config.sync_mode = sync_mode;
    config.concurrent_downloads = 1;
    Arc::new(config.with_pass(pass))
}

#[cfg(feature = "xmp")]
async fn assert_bound_receipt(
    db: &crate::state::SqliteStateDb,
    binding: &crate::state::db::PublicationBinding,
    provider_checksum: &str,
    source_size: u64,
    source_checksum: &str,
    local_checksum: &str,
    media: &std::path::Path,
) -> crate::state::db::download_receipts::DownloadReceipt {
    let receipts = db
        .bound_download_receipts(
            binding.clone(),
            "original".into(),
            provider_checksum.to_owned(),
            source_size,
        )
        .await
        .unwrap();
    assert_eq!(receipts.len(), 1);
    let receipt = receipts.into_iter().next().unwrap();
    assert_eq!(receipt.account_key, binding.account_key);
    assert_eq!(receipt.provider_key, binding.provider_key);
    assert_eq!(receipt.scope, binding.scope);
    assert_eq!(receipt.library, binding.library);
    assert_eq!(receipt.child, binding.child);
    assert_eq!(receipt.master, binding.master);
    assert_eq!(receipt.state_id, binding.state_id);
    assert_eq!(receipt.origin_pass, binding.origin_pass);
    assert_eq!(receipt.version_size, "original");
    assert_eq!(receipt.provider_checksum, provider_checksum);
    assert_eq!(receipt.provider_size, source_size);
    assert_eq!(receipt.native_path.to_path(), media);
    assert_eq!(receipt.source_checksum, source_checksum);
    assert_eq!(receipt.local_checksum, local_checksum);
    receipt
}

#[cfg(feature = "xmp")]
struct GpsProof<'a> {
    binding: &'a crate::state::db::PublicationBinding,
    pass_key: &'a str,
    provider_checksum: &'a str,
    source_size: u64,
    source_checksum: &'a str,
    local_checksum: &'a str,
    media: &'a std::path::Path,
    metadata_flags: u8,
}

#[cfg(feature = "xmp")]
async fn assert_generation(
    db: &crate::state::SqliteStateDb,
    binding: &crate::state::db::PublicationBinding,
    pass_key: &str,
    metadata_flags: u8,
    explicit_refresh: Option<bool>,
) -> serde_json::Value {
    let conn = db.acquire_lock("gps selection generation").unwrap();
    let rows: Vec<String> = conn
        .prepare("SELECT json_object('id',id,'spec',CAST(specification AS TEXT)) FROM provider_active_generations ORDER BY created_at DESC, id DESC")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(!rows.is_empty(), "no selection generation");
    let latest: serde_json::Value = serde_json::from_str(&rows[0]).unwrap();
    let spec: serde_json::Value = serde_json::from_str(latest["spec"].as_str().unwrap()).unwrap();
    assert_eq!(spec["format"], 2);
    assert_eq!(spec["metadata_enabled"], true);
    assert_eq!(spec["metadata_flags"], metadata_flags);
    assert_eq!(spec["scope"].as_str(), Some(binding.scope.as_str()));
    assert!(
        spec["profile"]["passes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|pass| pass["key"].as_str() == Some(pass_key)),
        "generation lost the captured pass key: {spec:?}"
    );
    match explicit_refresh {
        Some(expected) => assert_eq!(
            spec["profile"]["explicit_metadata_refresh"],
            i64::from(expected)
        ),
        None => assert!(spec["profile"]["explicit_metadata_refresh"].is_null()),
    }
    serde_json::json!({"generations": rows.len()})
}

#[cfg(feature = "xmp")]
async fn assert_refresh_destination(
    db: &crate::state::SqliteStateDb,
    proof: &GpsProof<'_>,
) -> serde_json::Value {
    let binding = proof.binding;
    let pass_key = proof.pass_key;
    let provider_checksum = proof.provider_checksum;
    let source_size = proof.source_size;
    let source_checksum = proof.source_checksum;
    let local_checksum = proof.local_checksum;
    let media = proof.media;
    let metadata_flags = proof.metadata_flags;
    let path_key = crate::state::db::provider_selection::SelectionPath::from_path(media)
        .key()
        .unwrap();
    let conn = db.acquire_lock("gps refresh destination").unwrap();
    let row: String = conn
        .query_row(
            "SELECT json_object('generation',d.generation,'pass',d.pass_key,'child',d.child,'master',d.master,'state',d.asset_id,'library',d.library,'version',d.version_size,'path',d.path,'checksum',d.checksum,'size',d.size_bytes,'admitted',d.admitted,'verified_media',d.verified_media,'verified_metadata',d.verified_metadata,'source',d.source_checksum,'local',d.local_checksum,'account',g.account_key,'provider',g.provider_key,'scope',g.scope,'metadata_enabled',g.metadata_enabled,'metadata_flags',json_extract(CAST(g.specification AS TEXT),'$.metadata_flags'),'format',json_extract(CAST(g.specification AS TEXT),'$.format'),'explicit_refresh',json_extract(CAST(g.specification AS TEXT),'$.profile.explicit_metadata_refresh')) FROM provider_active_destinations d JOIN provider_active_generations g ON g.id=d.generation WHERE d.library='PrimarySync' AND d.child=?1 AND d.master=?2 AND d.asset_id=?3 AND d.version_size='original' AND d.pass_key=?4 AND d.path=?5 AND d.checksum=?6 AND d.size_bytes=?7 AND d.admitted=1 ORDER BY d.generation DESC LIMIT 1",
            rusqlite::params![
                binding.child,
                binding.master,
                binding.state_id,
                pass_key,
                path_key,
                provider_checksum,
                i64::try_from(source_size).unwrap(),
            ],
            |row| row.get(0),
        )
        .unwrap();
    let value: serde_json::Value = serde_json::from_str(&row).unwrap();
    assert_eq!(value["pass"], pass_key);
    assert_eq!(value["child"], binding.child);
    assert_eq!(value["master"], binding.master);
    assert_eq!(value["state"], binding.state_id);
    assert_eq!(value["library"], "PrimarySync");
    assert_eq!(value["version"], "original");
    assert_eq!(value["path"], path_key);
    assert_eq!(value["checksum"], provider_checksum);
    assert_eq!(value["size"], source_size);
    assert_eq!(value["admitted"], 1);
    assert_eq!(value["verified_media"], 1);
    assert_eq!(value["verified_metadata"], 1);
    assert_eq!(value["source"], source_checksum);
    assert_eq!(value["local"], local_checksum);
    assert_eq!(value["account"], binding.account_key);
    assert_eq!(value["provider"], binding.provider_key);
    assert_eq!(value["scope"], binding.scope);
    assert_eq!(value["metadata_enabled"], 1);
    assert_eq!(value["metadata_flags"], metadata_flags);
    assert_eq!(value["format"], 2);
    assert_eq!(value["explicit_refresh"], 1);
    let generation = value["generation"].as_str().unwrap();
    let decisions: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM provider_active_decisions WHERE generation=?1 AND pass_key=?2 AND child=?3 AND admission='admitted' AND outcome='selected'",
            rusqlite::params![generation, pass_key, binding.child],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(decisions, 1);
    value
}

#[cfg(feature = "xmp")]
async fn preserved_path_count(db: &crate::state::SqliteStateDb) -> i64 {
    db.acquire_lock("gps preserved path count")
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM asset_metadata_paths WHERE preserved_path_key IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .unwrap()
}

#[cfg(feature = "xmp")]
async fn gps_bound_case(native_location: bool, embed: bool) {
    use crate::download::pipeline::test_support::sidecar_path_for;
    use crate::state::db::PublicationBinding;
    use crate::state::db::account::AccountOwner;
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::method;
    use wiremock::{Mock, ResponseTemplate};
    use xmp_toolkit::{XmpMeta, xmp_ns};

    let source = if native_location {
        crate::test_helpers::minimal_jpeg_with_source_gps_and_location()
    } else {
        crate::test_helpers::minimal_jpeg_with_source_gps()
    };
    let original_checksum = data_encoding::HEXLOWER.encode(&Sha256::digest(&source));
    let provider_checksum = gps_provider_checksum(&source);
    let server = crate::start_wiremock_or_skip!();
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(source.clone())
                .insert_header("content-type", "image/jpeg"),
        )
        .expect(1)
        .mount(&server)
        .await;
    let dir = TempDir::new().unwrap();
    let media_dir = dir.path().join("downloads");
    let db_path = dir.path().join("state.db");
    let owner = AccountOwner::authenticated(
        "writer@example.invalid",
        "com",
        &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-gps-normal"}})).unwrap(),
    )
    .unwrap();
    let records = Arc::new(gps_records(
        &source,
        &format!("{}/source.jpg", server.uri()),
    ));
    let session = GpsPhotosSession {
        records: Arc::clone(&records),
        next_token: Arc::new(std::sync::atomic::AtomicUsize::new(1)),
    };
    let db = Arc::new(
        crate::state::SqliteStateDb::open_owned(&db_path, &owner)
            .await
            .unwrap(),
    );
    let pass = gps_pass(db.clone(), owner.clone(), session.clone());
    let config = gps_config(
        &media_dir,
        db.clone(),
        &pass,
        embed,
        false,
        crate::download::SyncMode::Full,
    );
    let pass_key = config.primary_layout_pass.clone().unwrap().to_string();
    let initial = crate::download::download_photos_with_sync(
        &Client::new(),
        std::slice::from_ref(&pass),
        config.clone(),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(initial.stats.downloaded, 1, "{initial:?}");
    assert_eq!(initial.stats.failed, 0, "{initial:?}");
    assert_eq!(initial.stats.exif_failures, 0, "{initial:?}");
    assert_eq!(initial.stats.state_write_failures, 0, "{initial:?}");
    assert!(initial.full_enumeration_ran, "{initial:?}");
    let initial_token = initial.sync_token.clone().expect("initial full boundary");
    let asset = crate::icloud::photos::PhotoAsset::new(records[0].clone(), records[1].clone())
        .with_source_zone(Arc::from("PrimarySync"))
        .with_state_record_name(Arc::from("asset-GPS_CHILD"));
    let rows = db.get_downloaded_page(0, 10).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].download_checksum.as_deref(),
        Some(original_checksum.as_str())
    );
    let media = rows[0].local_path.as_ref().unwrap().clone();
    let sidecar = sidecar_path_for(&media);
    let media_bytes = fs::read(&media).unwrap();
    let sidecar_bytes = fs::read(&sidecar).unwrap();
    if native_location {
        assert_eq!(media_bytes, source);
    } else {
        assert_ne!(media_bytes, source);
    }
    let local_checksum = data_encoding::HEXLOWER.encode(&Sha256::digest(&media_bytes));
    assert_eq!(original_checksum == local_checksum, native_location);
    let xmp: XmpMeta = fs::read_to_string(&sidecar).unwrap().parse().unwrap();
    assert_eq!(
        xmp.contains_property(xmp_ns::EXIF, "GPSHPositioningError"),
        native_location
    );
    let native = crate::download::metadata::read_source_gps(&media).unwrap();
    assert!(native.horizontal_positioning_error.is_some());
    let (capture, scope, _zone) = pass
        .album
        .publication_scope("PrimarySync")
        .unwrap()
        .unwrap();
    drop(capture);
    let (account_key, provider_key) = owner.authenticated_keys().unwrap();
    let binding = PublicationBinding {
        account_key: account_key.to_owned(),
        provider_key: provider_key.to_owned(),
        scope,
        library: "PrimarySync".into(),
        child: asset.asset_record_name().into(),
        master: asset.id().into(),
        state_id: asset.state_id().into(),
        origin_pass: pass_key.clone(),
    };
    let metadata_flags = crate::download::pipeline::MetadataFlags::from(config.as_ref()).bits();
    let proof = GpsProof {
        binding: &binding,
        pass_key: &pass_key,
        provider_checksum: &provider_checksum,
        source_size: source.len() as u64,
        source_checksum: &original_checksum,
        local_checksum: &local_checksum,
        media: &media,
        metadata_flags,
    };
    let receipt = assert_bound_receipt(
        &db,
        &binding,
        &provider_checksum,
        source.len() as u64,
        &original_checksum,
        &local_checksum,
        &media,
    )
    .await;
    assert_generation(&db, &binding, &pass_key, metadata_flags, None).await;
    assert_eq!(preserved_path_count(&db).await, 0);
    db.record_metadata_write_failure("PrimarySync", asset.state_id(), "original")
        .await
        .unwrap();
    assert_eq!(db.get_pending_metadata_rewrites(10).await.unwrap().len(), 1);
    drop(pass);
    drop(config);
    drop(db);

    let db = Arc::new(
        crate::state::SqliteStateDb::open_owned(&db_path, &owner)
            .await
            .unwrap(),
    );
    let pass = gps_pass(db.clone(), owner.clone(), session.clone());
    let config = gps_config(
        &media_dir,
        db.clone(),
        &pass,
        embed,
        false,
        crate::download::SyncMode::Incremental {
            zone_sync_token: initial_token.clone(),
        },
    );
    assert_eq!(
        config.primary_layout_pass.as_deref(),
        Some(pass_key.as_str())
    );
    let delta = crate::download::download_photos_with_sync(
        &Client::new(),
        std::slice::from_ref(&pass),
        config.clone(),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(delta.stats.downloaded, 0, "{delta:?}");
    assert_eq!(delta.stats.failed, 0, "{delta:?}");
    assert_eq!(delta.stats.exif_failures, 0, "{delta:?}");
    assert_eq!(delta.stats.state_write_failures, 0, "{delta:?}");
    assert!(!delta.full_enumeration_ran, "{delta:?}");
    assert!(delta.checkpoint.completed_delta_replay, "{delta:?}");
    assert!(delta.sync_token.is_some(), "{delta:?}");
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    assert_eq!(db.get_pending_metadata_rewrites(10).await.unwrap().len(), 1);
    assert_eq!(fs::read(&media).unwrap(), media_bytes);
    assert_eq!(fs::read(&sidecar).unwrap(), sidecar_bytes);
    let current: XmpMeta = fs::read_to_string(&sidecar).unwrap().parse().unwrap();
    assert_eq!(
        current.contains_property(xmp_ns::EXIF, "GPSHPositioningError"),
        native_location
    );
    assert_bound_receipt(
        &db,
        &binding,
        &provider_checksum,
        source.len() as u64,
        &original_checksum,
        &local_checksum,
        &media,
    )
    .await;
    assert_generation(&db, &binding, &pass_key, metadata_flags, None).await;
    assert_eq!(preserved_path_count(&db).await, 0);
    drop(pass);
    drop(config);
    drop(db);

    let db = Arc::new(
        crate::state::SqliteStateDb::open_owned(&db_path, &owner)
            .await
            .unwrap(),
    );
    let pass = gps_pass(db.clone(), owner.clone(), session.clone());
    let config = gps_config(
        &media_dir,
        db.clone(),
        &pass,
        embed,
        true,
        crate::download::SyncMode::Full,
    );
    assert_eq!(
        config.primary_layout_pass.as_deref(),
        Some(pass_key.as_str())
    );
    let refresh = crate::download::download_photos_with_sync(
        &Client::new(),
        std::slice::from_ref(&pass),
        config.clone(),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(refresh.stats.downloaded, 0, "{refresh:?}");
    assert_eq!(refresh.stats.failed, 0, "{refresh:?}");
    assert_eq!(refresh.stats.exif_failures, 0, "{refresh:?}");
    assert_eq!(refresh.stats.state_write_failures, 0, "{refresh:?}");
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    assert!(
        db.get_pending_metadata_rewrites(10)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(fs::read(&media).unwrap(), media_bytes);
    assert_eq!(fs::read(&sidecar).unwrap(), sidecar_bytes);
    let current: XmpMeta = fs::read_to_string(&sidecar).unwrap().parse().unwrap();
    assert_eq!(
        current.contains_property(xmp_ns::EXIF, "GPSHPositioningError"),
        native_location
    );
    let refreshed_receipt = assert_bound_receipt(
        &db,
        &binding,
        &provider_checksum,
        source.len() as u64,
        &original_checksum,
        &local_checksum,
        &media,
    )
    .await;
    assert_eq!(refreshed_receipt, receipt);
    let refresh_generation =
        assert_generation(&db, &binding, &pass_key, metadata_flags, Some(true)).await;
    let refresh_destination = assert_refresh_destination(&db, &proof).await;
    assert_eq!(preserved_path_count(&db).await, 0);
    let refresh_token = refresh.sync_token.clone().expect("refresh boundary");
    eprintln!(
        "GPS_NORMAL_ONE_CASE pass_key={pass_key} initial_token={initial_token:?} delta_token={:?} refresh_token={refresh_token:?} receipt={receipt:?} refresh_generation={refresh_generation} refresh_destination={refresh_destination}",
        delta.sync_token
    );
    drop(pass);
    drop(config);
    drop(db);

    let mut quiet_token = refresh_token;
    for quiet in 0..2 {
        let db = Arc::new(
            crate::state::SqliteStateDb::open_owned(&db_path, &owner)
                .await
                .unwrap(),
        );
        let pass = gps_pass(db.clone(), owner.clone(), session.clone());
        let config = gps_config(
            &media_dir,
            db.clone(),
            &pass,
            embed,
            false,
            crate::download::SyncMode::Incremental {
                zone_sync_token: quiet_token.clone(),
            },
        );
        assert_eq!(
            config.primary_layout_pass.as_deref(),
            Some(pass_key.as_str())
        );
        let quiet_result = crate::download::download_photos_with_sync(
            &Client::new(),
            std::slice::from_ref(&pass),
            config.clone(),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            quiet_result.stats.downloaded, 0,
            "quiet {quiet}: {quiet_result:?}"
        );
        assert_eq!(
            quiet_result.stats.failed, 0,
            "quiet {quiet}: {quiet_result:?}"
        );
        assert_eq!(
            quiet_result.stats.exif_failures, 0,
            "quiet {quiet}: {quiet_result:?}"
        );
        assert!(
            !quiet_result.full_enumeration_ran,
            "quiet {quiet}: {quiet_result:?}"
        );
        assert!(
            quiet_result.checkpoint.completed_delta_replay,
            "quiet {quiet}: {quiet_result:?}"
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
        assert!(
            db.get_pending_metadata_rewrites(10)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(fs::read(&media).unwrap(), media_bytes);
        assert_eq!(fs::read(&sidecar).unwrap(), sidecar_bytes);
        let current: XmpMeta = fs::read_to_string(&sidecar).unwrap().parse().unwrap();
        assert_eq!(
            current.contains_property(xmp_ns::EXIF, "GPSHPositioningError"),
            native_location
        );
        assert_bound_receipt(
            &db,
            &binding,
            &provider_checksum,
            source.len() as u64,
            &original_checksum,
            &local_checksum,
            &media,
        )
        .await;
        assert_eq!(preserved_path_count(&db).await, 0);
        quiet_token = quiet_result
            .sync_token
            .clone()
            .unwrap_or_else(|| quiet_token.clone());
        drop(pass);
        drop(config);
        drop(db);
    }
    server.verify().await;
}

#[cfg(feature = "xmp")]
async fn gps_unbound_case() {
    use crate::download::pipeline::test_support::sidecar_path_for;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::method;
    use wiremock::{Mock, ResponseTemplate};
    use xmp_toolkit::{XmpMeta, xmp_ns};

    let source = crate::test_helpers::minimal_jpeg_with_source_gps();
    let original_checksum = data_encoding::HEXLOWER.encode(&Sha256::digest(&source));
    let server = crate::start_wiremock_or_skip!();
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(source.clone())
                .insert_header("content-type", "image/jpeg"),
        )
        .expect(1)
        .mount(&server)
        .await;
    let dir = TempDir::new().unwrap();
    let media_dir = dir.path().join("downloads");
    let db_path = dir.path().join("state.db");
    let records = Arc::new(gps_records(
        &source,
        &format!("{}/source.jpg", server.uri()),
    ));
    let session = GpsPhotosSession {
        records: Arc::clone(&records),
        next_token: Arc::new(std::sync::atomic::AtomicUsize::new(1)),
    };
    let db = Arc::new(crate::state::SqliteStateDb::open(&db_path).await.unwrap());
    let pass = gps_plain_pass(session.clone());
    let mut config = crate::download::DownloadConfig::test_default();
    config.directory = Arc::from(media_dir.as_path());
    config.library = Arc::from("PrimarySync");
    config.state_db = Some(db.clone());
    config.metadata.set_exif_gps = true;
    config.metadata.xmp_sidecar = true;
    config.refresh_metadata = false;
    config.sync_mode = crate::download::SyncMode::Full;
    config.concurrent_downloads = 1;
    let config = Arc::new(config);
    let initial = crate::download::download_photos_with_sync(
        &Client::new(),
        std::slice::from_ref(&pass),
        config.clone(),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(initial.stats.downloaded, 1, "{initial:?}");
    assert_eq!(initial.stats.failed, 0, "{initial:?}");
    assert_eq!(initial.stats.exif_failures, 0, "{initial:?}");
    let rows = db.get_downloaded_page(0, 10).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].download_checksum.as_deref(),
        Some(original_checksum.as_str())
    );
    let media = rows[0].local_path.as_ref().unwrap().clone();
    let sidecar = sidecar_path_for(&media);
    let media_bytes = fs::read(&media).unwrap();
    let sidecar_bytes = fs::read(&sidecar).unwrap();
    assert_ne!(media_bytes, source);
    let receipts: i64 = db
        .acquire_lock("unbound transfer cannot mint a scoped receipt")
        .unwrap()
        .query_row("SELECT COUNT(*) FROM download_receipts", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(receipts, 0);
    let asset = crate::icloud::photos::PhotoAsset::new(records[0].clone(), records[1].clone())
        .with_state_record_name(Arc::from("asset-GPS_CHILD"));
    db.record_metadata_write_failure("PrimarySync", asset.state_id(), "original")
        .await
        .unwrap();
    drop(pass);
    drop(config);
    drop(db);

    let db = Arc::new(crate::state::SqliteStateDb::open(&db_path).await.unwrap());
    let pass = gps_plain_pass(session.clone());
    let mut config = crate::download::DownloadConfig::test_default();
    config.directory = Arc::from(media_dir.as_path());
    config.library = Arc::from("PrimarySync");
    config.state_db = Some(db.clone());
    config.metadata.set_exif_gps = true;
    config.metadata.xmp_sidecar = true;
    config.refresh_metadata = false;
    config.sync_mode = crate::download::SyncMode::Full;
    config.concurrent_downloads = 1;
    let config = Arc::new(config);
    let refused = crate::download::download_photos_with_sync(
        &Client::new(),
        std::slice::from_ref(&pass),
        config,
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(refused.stats.downloaded, 0, "{refused:?}");
    assert_eq!(refused.stats.skipped.uncertain, 1, "{refused:?}");
    let refused_rows = db.get_downloaded_page(0, 10).await.unwrap();
    assert_eq!(refused_rows.len(), 1);
    assert_eq!(
        refused_rows[0].download_checksum.as_deref(),
        Some(original_checksum.as_str())
    );
    assert_eq!(
        db.get_pending_metadata_rewrites(10).await.unwrap().len(),
        1,
        "the real metadata marker remains the visible debt"
    );
    assert!(
        db.get_pending().await.unwrap().is_empty(),
        "the downloaded row is retained; its debt is the metadata marker"
    );
    assert_eq!(fs::read(&media).unwrap(), media_bytes);
    assert_eq!(fs::read(&sidecar).unwrap(), sidecar_bytes);
    let current: XmpMeta = fs::read_to_string(&sidecar).unwrap().parse().unwrap();
    assert!(!current.contains_property(xmp_ns::EXIF, "GPSHPositioningError"));
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    server.verify().await;
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn contract_xmp_gps_accuracy_requires_matching_location_download_and_reopen() {
    for (native_location, embed) in [(false, true), (true, false), (true, true)] {
        gps_bound_case(native_location, embed).await;
    }
    gps_unbound_case().await;
}

#[cfg(unix)]
#[tokio::test]
async fn resumed_task_rejects_symlink_and_preserves_regular_resume() {
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, ResponseTemplate};

    #[derive(Clone, Copy)]
    enum ResumeLeaf {
        Symlink,
        SwappedSymlink,
        Regular,
    }
    for (leaf, embed) in [
        ResumeLeaf::Symlink,
        ResumeLeaf::SwappedSymlink,
        ResumeLeaf::Regular,
    ]
    .into_iter()
    .flat_map(|leaf| [false, true].map(|embed| (leaf, embed)))
    {
        let rejected = !matches!(leaf, ResumeLeaf::Regular);
        let server = crate::start_wiremock_or_skip!();
        let body = crate::download::pipeline::test_support::MINIMAL_JPEG.to_vec();
        let prefix = &body[..2];
        let dir = TempDir::new().unwrap();
        let external = TempDir::new().unwrap();
        let sentinel = external.path().join("sentinel.jpg");
        fs::write(&sentinel, prefix).unwrap();
        let db_path = dir.path().join("state.db");
        let db = Arc::new(crate::state::SqliteStateDb::open(&db_path).await.unwrap());
        let mut config = DownloadConfig::test_default();
        config.directory = Arc::from(dir.path().join("media"));
        config.state_db = Some(db.clone());
        config.retry.max_retries = 0;
        config.metadata.set_exif_datetime = embed;
        let config = Arc::new(config);
        let checksum = base64::engine::general_purpose::STANDARD.encode([0x73; 32]);
        let asset = TestPhotoAsset::new("RESUME")
            .filename("resume.jpg")
            .orig_size(body.len() as u64)
            .orig_url(&format!("{}/resume.jpg", server.uri()))
            .orig_checksum(&checksum)
            .build();
        let mut planner = TaskPlanner::for_download(Some(db.as_ref())).await.unwrap();
        let plan = planner.plan_download_asset(&asset, &config).await.unwrap();
        assert_eq!(plan.tasks.len(), 1);
        let task = &plan.tasks[0];
        let final_path = task.download_path.clone();
        let part = crate::download::file::temp_download_path(
            &final_path,
            &task.checksum,
            &config.temp_suffix,
        )
        .unwrap();
        fs::create_dir_all(part.parent().unwrap()).unwrap();
        if matches!(leaf, ResumeLeaf::Symlink) {
            std::os::unix::fs::symlink(&sentinel, &part).unwrap();
        } else {
            fs::write(&part, prefix).unwrap();
        }
        let swap_part = part.clone();
        let swap_sentinel = sentinel.clone();
        let response_body = body[2..].to_vec();
        let content_range = format!("bytes 2-{}/{}", body.len() - 1, body.len());
        Mock::given(method("GET"))
            .and(path("/resume.jpg"))
            .and(header("range", "bytes=2-"))
            .respond_with(move |_: &wiremock::Request| {
                // The Range request proves the production resume probe has run.
                if matches!(leaf, ResumeLeaf::SwappedSymlink) {
                    fs::rename(&swap_part, swap_part.with_extension("retained")).unwrap();
                    std::os::unix::fs::symlink(&swap_sentinel, &swap_part).unwrap();
                }
                ResponseTemplate::new(206)
                    .insert_header("content-range", content_range.clone())
                    .insert_header("content-type", "image/jpeg")
                    .set_body_bytes(response_body.clone())
            })
            .expect(if matches!(leaf, ResumeLeaf::Symlink) {
                0
            } else {
                1
            })
            .mount(&server)
            .await;
        planner::upsert_seen_for_task(db.as_ref(), &config, &asset, task)
            .await
            .unwrap();
        let client = Client::new();
        // This direct caller has no authenticated publication scope. A rewritten
        // output is retained as uncertain on reopen; no hash alone binds it.
        let mut first_output = None;
        for cycle in 0..2 {
            let result = stream_and_download_from_stream(
                &client,
                stream::iter(vec![Ok(asset.clone())]),
                &config,
                DownloadControls::download_hidden(),
                1,
                CancellationToken::new(),
                StreamRuntime::new(None, None),
            )
            .await
            .unwrap();
            assert_eq!(fs::read(&sentinel).unwrap(), prefix);
            let reopened = crate::state::SqliteStateDb::open(&db_path).await.unwrap();
            if embed {
                let receipts: i64 = reopened
                    .acquire_lock("unbound resume cannot mint a scoped receipt")
                    .unwrap()
                    .query_row("SELECT COUNT(*) FROM download_receipts", [], |row| {
                        row.get(0)
                    })
                    .unwrap();
                assert_eq!(receipts, 0);
            }
            if rejected {
                assert_eq!(result.downloaded, 0);
                assert_eq!(result.failed.len(), 1);
                assert!(!final_path.exists());
                if matches!(leaf, ResumeLeaf::SwappedSymlink) {
                    assert_eq!(fs::read(part.with_extension("retained")).unwrap(), prefix);
                }
                assert!(
                    reopened
                        .get_downloaded_page(0, 10)
                        .await
                        .unwrap()
                        .is_empty()
                );
                let failed = reopened.get_failed().await.unwrap();
                assert_eq!(failed.len(), 1);
                assert!(
                    failed[0]
                        .last_error
                        .as_deref()
                        .is_some_and(|error| error.contains("regular"))
                );
            } else {
                assert_eq!(result.downloaded, usize::from(cycle == 0));
                assert!(result.failed.is_empty());
                assert_eq!(result.exif_failures, 0);
                let bytes = fs::read(&final_path).unwrap();
                if embed {
                    assert_ne!(
                        bytes, body,
                        "opt-in metadata must exercise inode replacement"
                    );
                } else {
                    assert_eq!(bytes, body);
                }
                use sha2::{Digest, Sha256};
                if embed && cycle == 1 {
                    assert_eq!(result.skip_summary.uncertain, 1);
                    assert!(
                        reopened
                            .get_downloaded_page(0, 10)
                            .await
                            .unwrap()
                            .is_empty()
                    );
                    let pending = reopened.get_pending().await.unwrap();
                    assert_eq!(pending.len(), 1);
                    assert_eq!(pending[0].local_path.as_deref(), Some(final_path.as_path()));
                    assert!(pending[0].last_error.as_deref().is_some_and(|error| {
                        error.contains("no_valid_receipt_or_supported_provider_proof")
                    }));
                    assert_eq!(
                        pending[0].download_checksum.as_deref(),
                        Some(
                            data_encoding::HEXLOWER
                                .encode(&Sha256::digest(&body))
                                .as_str()
                        )
                    );
                    assert_eq!(
                        pending[0].local_checksum.as_deref(),
                        Some(
                            data_encoding::HEXLOWER
                                .encode(&Sha256::digest(&bytes))
                                .as_str()
                        )
                    );
                    assert_eq!(
                        Some((
                            bytes,
                            fs::metadata(&final_path).unwrap().modified().unwrap()
                        )),
                        first_output
                    );
                    assert!(!part.exists());
                    continue;
                }
                if embed {
                    first_output = Some((
                        bytes.clone(),
                        fs::metadata(&final_path).unwrap().modified().unwrap(),
                    ));
                }
                let rows = reopened.get_downloaded_page(0, 10).await.unwrap();
                assert_eq!(rows.len(), 1);
                assert_eq!(
                    rows[0].download_checksum.as_deref(),
                    Some(
                        data_encoding::HEXLOWER
                            .encode(&Sha256::digest(&body))
                            .as_str()
                    )
                );
                assert_eq!(
                    rows[0].local_checksum.as_deref(),
                    Some(
                        data_encoding::HEXLOWER
                            .encode(&Sha256::digest(&bytes))
                            .as_str()
                    )
                );
                assert!(!part.exists());
                assert_eq!(reopened.get_downloaded_page(0, 10).await.unwrap().len(), 1);
            }
        }
        server.verify().await;
    }
}

#[cfg(unix)]
#[tokio::test]
async fn bound_embedded_resume_records_source_and_local_proof_then_reopens_quietly() {
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, ResponseTemplate};

    let server = crate::start_wiremock_or_skip!();
    let body = crate::download::pipeline::test_support::MINIMAL_JPEG.to_vec();
    let prefix = &body[..2];
    let response_body = body[2..].to_vec();
    let content_range = format!("bytes 2-{}/{}", body.len() - 1, body.len());
    Mock::given(method("GET"))
        .and(path("/bound-resume.jpg"))
        .and(header("range", "bytes=2-"))
        .respond_with(
            ResponseTemplate::new(206)
                .insert_header("content-range", content_range)
                .insert_header("content-type", "image/jpeg")
                .set_body_bytes(response_body),
        )
        .expect(1)
        .mount(&server)
        .await;
    let dir = TempDir::new().unwrap();
    // An opaque provider checksum cannot itself prove an embedded output.
    let provider_checksum = base64::engine::general_purpose::STANDARD.encode([0x73; 32]);
    let source_checksum = data_encoding::HEXLOWER.encode(&Sha256::digest(&body));
    let asset = TestPhotoAsset::new("BOUND_RESUME")
        .filename("bound-resume.jpg")
        .orig_size(body.len() as u64)
        .orig_url(&format!("{}/bound-resume.jpg", server.uri()))
        .orig_checksum(&provider_checksum)
        .build()
        .with_source_zone(Arc::from("PrimarySync"));
    let mut first_receipt = None;
    let mut first_bytes = None;
    let mut first_destination = None;
    for cycle in 0..3 {
        let (db, mut config) = crate::download::pipeline::test_support::owned_download_fixture(
            dir.path(),
            "PrimarySync",
        )
        .await;
        config.directory = Arc::from(dir.path().join("media"));
        config.retry.max_retries = 0;
        config.metadata.set_exif_datetime = true;
        let config = Arc::new(config);
        let mut planner = TaskPlanner::for_download(Some(db.as_ref())).await.unwrap();
        let plan = planner.plan_download_asset(&asset, &config).await.unwrap();
        assert_eq!(plan.tasks.len(), 1);
        let task = &plan.tasks[0];
        let binding = task
            .publication_binding
            .as_ref()
            .expect("owned selected source must bind publication")
            .as_ref()
            .clone();
        assert_eq!(binding.library, "PrimarySync");
        assert_eq!(binding.child, asset.asset_record_name());
        assert_eq!(binding.master, asset.id());
        assert_eq!(binding.state_id, asset.state_id());
        assert_eq!(binding.origin_pass, "selected-pass");
        let planned_destination = task.download_path.clone();
        let destination = first_destination
            .clone()
            .unwrap_or_else(|| planned_destination.clone());
        let part = crate::download::file::temp_download_path(
            &destination,
            &task.checksum,
            &config.temp_suffix,
        )
        .unwrap();
        if cycle == 0 {
            assert!(
                db.bound_download_receipts(
                    binding.clone(),
                    "original".into(),
                    provider_checksum.clone(),
                    body.len() as u64
                )
                .await
                .unwrap()
                .is_empty()
            );
            fs::create_dir_all(part.parent().unwrap()).unwrap();
            fs::write(&part, prefix).unwrap();
            planner::upsert_seen_for_task(db.as_ref(), &config, &asset, task)
                .await
                .unwrap();
        }
        let result = stream_and_download_from_stream(
            &Client::new(),
            stream::iter(vec![Ok(asset.clone())]),
            &config,
            DownloadControls::download_hidden(),
            1,
            CancellationToken::new(),
            StreamRuntime::new(None, None),
        )
        .await
        .unwrap();
        assert_eq!(
            result.downloaded,
            usize::from(cycle == 0),
            "cycle {cycle}: {result:?}"
        );
        assert!(result.failed.is_empty());
        assert_eq!(result.exif_failures, 0);
        assert_eq!(result.skip_summary.uncertain, 0);
        if cycle > 0 {
            assert_eq!(
                result.skip_summary.on_disk, 1,
                "the bound receipt must settle the selected work"
            );
            if planned_destination != destination {
                assert!(
                    !planned_destination.exists(),
                    "a prospective fallback must not be published"
                );
            }
        }
        let output = fs::read(&destination).unwrap();
        assert_ne!(
            output, body,
            "metadata embedding must produce distinct local bytes"
        );
        let local_checksum = data_encoding::HEXLOWER.encode(&Sha256::digest(&output));
        assert_ne!(source_checksum, local_checksum);
        let receipts = db
            .bound_download_receipts(
                binding.clone(),
                "original".into(),
                provider_checksum.clone(),
                body.len() as u64,
            )
            .await
            .unwrap();
        assert_eq!(receipts.len(), 1);
        let receipt = &receipts[0];
        assert_eq!(receipt.account_key, binding.account_key);
        assert_eq!(receipt.provider_key, binding.provider_key);
        assert_eq!(receipt.library, binding.library);
        assert_eq!(receipt.child, binding.child);
        assert_eq!(receipt.master, binding.master);
        assert_eq!(receipt.state_id, binding.state_id);
        assert_eq!(receipt.scope, binding.scope);
        assert_eq!(receipt.origin_pass, binding.origin_pass);
        assert_eq!(receipt.version_size, "original");
        assert_eq!(receipt.provider_checksum, provider_checksum);
        assert_eq!(receipt.provider_size, body.len() as u64);
        assert_eq!(receipt.native_path.to_path(), destination);
        assert_eq!(receipt.source_checksum, source_checksum);
        assert_eq!(receipt.local_checksum, local_checksum);
        let changed_checksum = base64::engine::general_purpose::STANDARD.encode([0x74; 32]);
        assert!(
            db.bound_download_receipts(
                binding.clone(),
                "original".into(),
                changed_checksum,
                body.len() as u64
            )
            .await
            .unwrap()
            .is_empty()
        );
        assert!(
            db.bound_download_receipts(
                binding.clone(),
                "original".into(),
                provider_checksum.clone(),
                body.len() as u64 + 1
            )
            .await
            .unwrap()
            .is_empty()
        );
        let mut foreign_child = binding.clone();
        foreign_child.child.push_str("-foreign");
        assert!(
            db.bound_download_receipts(
                foreign_child,
                "original".into(),
                provider_checksum.clone(),
                body.len() as u64
            )
            .await
            .unwrap()
            .is_empty()
        );
        let mut foreign_master = binding.clone();
        foreign_master.master.push_str("-foreign");
        assert!(
            db.bound_download_receipts(
                foreign_master,
                "original".into(),
                provider_checksum.clone(),
                body.len() as u64
            )
            .await
            .unwrap()
            .is_empty()
        );
        let rows = db.get_downloaded_page(0, 10).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].download_checksum.as_deref(),
            Some(source_checksum.as_str())
        );
        assert_eq!(
            rows[0].local_checksum.as_deref(),
            Some(local_checksum.as_str())
        );
        assert!(db.get_pending().await.unwrap().is_empty());
        assert!(!part.exists());
        if let Some(previous) = &first_receipt {
            assert_eq!(receipt, previous);
        }
        if let Some(previous) = &first_bytes {
            assert_eq!(&output, previous);
        }
        first_receipt = Some(receipt.clone());
        first_bytes = Some(output);
        first_destination = Some(destination);
        drop(plan);
        drop(planner);
        drop(config);
        drop(db);
    }
    server.verify().await;
}

#[tokio::test]
async fn complete_part_416_recovers_persists_and_skips_after_reopen() {
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    let server = wiremock::MockServer::start().await;
    let body = include_bytes!("../../../../tests/data/media/pattern.jpg").to_vec();
    let digest = Sha256::digest(&body);
    let checksum = base64::engine::general_purpose::STANDARD.encode(digest);
    let expected_hash = data_encoding::HEXLOWER.encode(&digest);
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("state.db");
    let db = Arc::new(crate::state::SqliteStateDb::open(&db_path).await.unwrap());
    let mut config = DownloadConfig::test_default();
    config.directory = Arc::from(dir.path().join("media"));
    config.state_db = Some(db.clone());
    config.retry.max_retries = 0;
    let mut config = Arc::new(config);
    let asset = TestPhotoAsset::new("COMPLETE-RESUME")
        .filename("complete.jpg")
        .orig_size(body.len() as u64)
        .orig_url(&format!("{}/complete.jpg", server.uri()))
        .orig_checksum(&checksum)
        .build();
    let mut planner = TaskPlanner::for_download(Some(db.as_ref())).await.unwrap();
    let plan = planner.plan_download_asset(&asset, &config).await.unwrap();
    assert_eq!(plan.tasks.len(), 1);
    let task = &plan.tasks[0];
    let final_path = task.download_path.clone();
    let part =
        crate::download::file::temp_download_path(&final_path, &task.checksum, &config.temp_suffix)
            .unwrap();
    fs::create_dir_all(part.parent().unwrap()).unwrap();
    fs::write(&part, &body).unwrap();
    planner::upsert_seen_for_task(db.as_ref(), &config, &asset, task)
        .await
        .unwrap();
    assert!(db.get_downloaded_page(0, 10).await.unwrap().is_empty());
    assert!(!final_path.exists());
    let response_body = body.clone();
    let expected_range = format!("bytes={}-", body.len());
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = Arc::clone(&requests);
    Mock::given(method("GET"))
        .and(path("/complete.jpg"))
        .respond_with(move |request: &wiremock::Request| {
            let range = request
                .headers
                .get("range")
                .map(|value| value.to_str().unwrap().to_string());
            recorded.lock().unwrap().push(range.clone());
            if let Some(range) = range {
                assert_eq!(range, expected_range);
                ResponseTemplate::new(416)
            } else {
                ResponseTemplate::new(200)
                    .insert_header("content-type", "image/jpeg")
                    .set_body_bytes(response_body.clone())
            }
        })
        .expect(2)
        .mount(&server)
        .await;
    let client = Client::new();
    for cycle in 0..2 {
        let result = stream_and_download_from_stream(
            &client,
            stream::iter(vec![Ok(asset.clone())]),
            &config,
            DownloadControls::download_hidden(),
            1,
            CancellationToken::new(),
            StreamRuntime::new(None, None),
        )
        .await
        .unwrap();
        assert_eq!(result.downloaded, usize::from(cycle == 0));
        assert!(result.failed.is_empty());
        assert_eq!(fs::read(&final_path).unwrap(), body);
        assert!(!part.exists());
        let reopened = Arc::new(crate::state::SqliteStateDb::open(&db_path).await.unwrap());
        let rows = reopened.get_downloaded_page(0, 10).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].local_checksum.as_deref(),
            Some(expected_hash.as_str())
        );
        assert_eq!(
            rows[0].download_checksum.as_deref(),
            Some(expected_hash.as_str())
        );
        assert!(reopened.get_failed().await.unwrap().is_empty());
        Arc::make_mut(&mut config).state_db = Some(reopened);
    }
    assert_eq!(
        *requests.lock().unwrap(),
        vec![Some(format!("bytes={}-", body.len())), None]
    );
    server.verify().await;
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn restricted_new_media_preserves_sidecar_and_metadata_debt_after_reopen() {
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};
    let server = crate::start_wiremock_or_skip!();
    Mock::given(method("GET"))
        .and(path("/fresh.jpg"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(MINIMAL_JPEG))
        .expect(1)
        .mount(&server)
        .await;
    let dir = TempDir::new().unwrap();
    let database = dir.path().join("state.db");
    let db = Arc::new(crate::state::SqliteStateDb::open(&database).await.unwrap());
    db.upsert_seen(&crate::test_helpers::TestAssetRecord::new("old-claim").build())
        .await
        .unwrap();
    db.acquire_lock("synthetic malformed historical claim")
        .unwrap()
        .execute(
            "UPDATE assets SET local_path='old/../legacy.jpg' WHERE id='old-claim'",
            [],
        )
        .unwrap();
    let asset = TestPhotoAsset::new("fresh")
        .filename("fresh.jpg")
        .item_type("public.jpeg")
        .orig_file_type("public.jpeg")
        .orig_size(MINIMAL_JPEG.len() as u64)
        .orig_url(&format!("{}/fresh.jpg", server.uri()))
        .orig_checksum(
            &base64::engine::general_purpose::STANDARD.encode(Sha256::digest(MINIMAL_JPEG)),
        )
        .build();
    let mut config = DownloadConfig::test_default();
    config.directory = Arc::from(dir.path().join("media"));
    config.metadata.xmp_sidecar = true;
    config.state_db = Some(db.clone());
    let media = crate::download::filter::derive_expected_paths(&asset, &config)[0]
        .path
        .clone();
    fs::create_dir_all(media.parent().unwrap()).unwrap();
    let sidecar = sidecar_path_for(&media);
    fs::write(&sidecar, b"user sidecar must stay untouched").unwrap();
    let mtime = fs::metadata(&sidecar).unwrap().modified().unwrap();
    let config = Arc::new(config);
    let result = stream_and_download_from_stream(
        &Client::new(),
        stream::iter(vec![
            Ok::<crate::icloud::photos::PhotoAsset, anyhow::Error>(asset),
        ]),
        &config,
        DownloadControls::download_hidden(),
        1,
        CancellationToken::new(),
        StreamRuntime::new(None, None),
    )
    .await
    .unwrap();
    assert_eq!(result.downloaded, 1);
    assert_eq!(result.exif_failures, 1);
    assert!(result.failed.is_empty());
    assert_eq!(fs::read(&media).unwrap(), MINIMAL_JPEG);
    assert_eq!(db.get_pending_metadata_rewrites(10).await.unwrap().len(), 1);
    drop(config);
    drop(db);
    for _ in 0..2 {
        let reopened = crate::state::SqliteStateDb::open(&database).await.unwrap();
        let pass = crate::download::metadata_rewrite::run_pending(
            &reopened,
            MetadataFlags::XMP_SIDECAR,
            Arc::from(".part"),
            &CancellationToken::new(),
        )
        .await;
        assert_eq!((pass.applied, pass.failed), (0, 1));
        assert_eq!(
            reopened
                .get_pending_metadata_rewrites(10)
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            fs::read(&sidecar).unwrap(),
            b"user sidecar must stay untouched"
        );
        assert_eq!(fs::metadata(&sidecar).unwrap().modified().unwrap(), mtime);
        assert_eq!(fs::read(&media).unwrap(), MINIMAL_JPEG);
    }
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}
