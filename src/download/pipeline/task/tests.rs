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
use reqwest::Client;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

#[cfg(feature = "xmp")]
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
#[cfg(feature = "xmp")]
use crate::download::pipeline::test_support::{MINIMAL_JPEG, sidecar_path_for};
use crate::download::{DownloadConfig, DownloadControls, DownloadReporting};
use crate::retry::RetryConfig;
#[cfg(feature = "xmp")]
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

#[cfg(feature = "xmp")]
#[tokio::test]
async fn different_byte_destination_race_keeps_loser_failed_without_metadata_write() {
    use crate::state::{MediaType, SqliteStateDb};
    use base64::Engine as _;
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
    let winner_db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
    let loser_db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
    let retry = RetryConfig {
        max_retries: 0,
        base_delay_secs: 0,
        max_delay_secs: 0,
    };
    let metadata = MetadataFlags::from(&crate::config::MetadataConfig {
        xmp_sidecar: true,
        ..crate::config::MetadataConfig::default()
    });
    let make_task = |asset_id: &str, checksum_byte: u8, rating: u8| DownloadTask {
        url: format!("{}/{asset_id}.jpg", server.uri()).into(),
        download_path: download_path.clone(),
        replacement_fingerprint: None,
        pending_cross_parent_root: None,
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
        },
        vec![winner_task],
    )
    .await;
    assert_eq!(winner_result.downloaded, 1);
    assert!(winner_result.failed.is_empty());

    let sidecar_path = sidecar_path_for(&download_path);
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
        },
        vec![loser_task],
    )
    .await;

    assert_eq!(loser_result.downloaded, 0);
    assert_eq!(loser_result.failed.len(), 1);
    assert_eq!(fs::read(&download_path).unwrap(), winner_body);
    assert_eq!(fs::read(&sidecar_path).unwrap(), winner_sidecar);
    assert_eq!(fs::read(&loser_part_path).unwrap(), loser_body);
    assert!(
        loser_db
            .get_downloaded_page(0, 10)
            .await
            .unwrap()
            .is_empty()
    );
    let failed = loser_db.get_failed().await.unwrap();
    assert_eq!(failed.len(), 1);
    assert!(
        failed[0]
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains("verified bytes differ")),
        "collision must remain durable failed work"
    );
}

#[tokio::test]
async fn full_expired_url_cleanup_recovers_queued_tasks_but_respects_shutdown() {
    use base64::Engine as _;
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
            .expect(if user_shutdown { 0 } else { 1 })
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
async fn temporary_ownership_retires_after_publish_and_interruption() {
    use base64::Engine as _;

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
        url: format!("https://example.invalid/{name}.jpg").into(),
        download_path: dir.path().join(format!("{name}.jpg")),
        replacement_fingerprint: None,
        pending_cross_parent_root: None,
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

    download_single_task(
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
    assert!(
        db.get_owned_temp_files_before(i64::MAX)
            .await
            .unwrap()
            .is_empty(),
        "published download must retire ownership"
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
    assert!(
        db.get_owned_temp_files_before(i64::MAX)
            .await
            .unwrap()
            .is_empty(),
        "graceful interruption must retire ownership"
    );
}

#[tokio::test]
async fn truncated_task_retires_ownership_after_settled_prefix_and_resumes() {
    use base64::Engine as _;
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
#[tokio::test]
async fn contract_xmp_gps_accuracy_requires_matching_location_download_and_reopen() {
    use crate::icloud::photos::PhotoAsset;
    use crate::state::SqliteStateDb;
    use base64::Engine as _;
    use futures_util::stream;
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::method;
    use wiremock::{Mock, ResponseTemplate};
    use xmp_toolkit::{XmpMeta, xmp_ns};

    for (native_location, embed) in [(false, true), (true, false), (true, true)] {
        let source = if native_location {
            crate::test_helpers::minimal_jpeg_with_source_gps_and_location()
        } else {
            crate::test_helpers::minimal_jpeg_with_source_gps()
        };
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
        let asset = PhotoAsset::new(
            json!({
                "recordName": "GPS_MASTER",
                "fields": {
                    "filenameEnc": {"value": "source.jpg", "type": "STRING"},
                    "itemType": {"value": "public.jpeg"},
                    "resOriginalRes": {"value": {
                        "size": source.len(), "downloadURL": format!("{}/source.jpg", server.uri()),
                        "fileChecksum": base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&source))
                    }},
                    "resOriginalFileType": {"value": "public.jpeg"}
                }
            }),
            json!({
                "recordName": "asset-GPS_CHILD",
                "fields": {
                    "assetDate": {"value": 1736899200000.0},
                    "locationLatitude": {"value": 1.5}, "locationLongitude": {"value": 2.5}
                }
            }),
        );
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("state.db");
        let db = Arc::new(SqliteStateDb::open(&db_path).await.unwrap());
        let download_dir = dir.path().join("downloads");
        let mut config = DownloadConfig::test_default();
        config.directory = Arc::from(download_dir.as_path());
        config.metadata.set_exif_gps = embed;
        config.metadata.xmp_sidecar = true;
        config.state_db = Some(db.clone());
        let config = Arc::new(config);
        let result = stream_and_download_from_stream(
            &Client::new(),
            stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(asset.clone())]),
            &config,
            DownloadControls::download_hidden(),
            1,
            CancellationToken::new(),
            StreamRuntime::new(None, None),
        )
        .await
        .unwrap();
        assert_eq!(result.downloaded, 1);
        assert!(result.failed.is_empty());
        assert_eq!(result.exif_failures, 0);
        let rows = db.get_downloaded_page(0, 10).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].download_checksum.as_deref(),
            Some(original_checksum.as_str())
        );
        let media = rows[0].local_path.as_ref().unwrap().clone();
        let sidecar = sidecar_path_for(&media);
        let xmp: XmpMeta = fs::read_to_string(&sidecar).unwrap().parse().unwrap();
        assert_eq!(
            xmp.contains_property(xmp_ns::EXIF, "GPSHPositioningError"),
            native_location
        );
        let media_bytes = fs::read(&media).unwrap();
        if native_location {
            assert_eq!(media_bytes, source);
        } else {
            assert_ne!(media_bytes, source);
        }
        let native = crate::download::metadata::read_source_gps(&media).unwrap();
        assert!(native.horizontal_positioning_error.is_some());
        db.record_metadata_write_failure("PrimarySync", asset.state_id(), "original")
            .await
            .unwrap();
        drop(config);
        drop(db);

        let db = Arc::new(SqliteStateDb::open(&db_path).await.unwrap());
        let mut config = DownloadConfig::test_default();
        config.directory = Arc::from(download_dir.as_path());
        config.metadata.xmp_sidecar = true;
        config.state_db = Some(db.clone());
        let config = Arc::new(config);
        let mut previous_sidecar = None;
        for _ in 0..2 {
            let result = stream_and_download_from_stream(
                &Client::new(),
                stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(asset.clone())]),
                &config,
                DownloadControls::download_hidden(),
                1,
                CancellationToken::new(),
                StreamRuntime::new(None, None),
            )
            .await
            .unwrap();
            assert_eq!(result.downloaded, 0);
            assert!(result.failed.is_empty());
            assert_eq!(result.exif_failures, 0);
            assert_eq!(fs::read(&media).unwrap(), media_bytes);
            let sidecar_bytes = fs::read(&sidecar).unwrap();
            if let Some(previous) = previous_sidecar.replace(sidecar_bytes.clone()) {
                assert_eq!(
                    sidecar_bytes, previous,
                    "steady state must not repeat sidecar work"
                );
            }
            let rows = db.get_downloaded_page(0, 10).await.unwrap();
            assert_eq!(
                rows[0].download_checksum.as_deref(),
                Some(original_checksum.as_str())
            );
            let current: XmpMeta = fs::read_to_string(&sidecar).unwrap().parse().unwrap();
            assert_eq!(
                current.contains_property(xmp_ns::EXIF, "GPSHPositioningError"),
                native_location
            );
            assert!(
                db.get_pending_metadata_rewrites(10)
                    .await
                    .unwrap()
                    .is_empty()
            );
        }
        server.verify().await;
    }
}

#[cfg(unix)]
#[tokio::test]
async fn resumed_task_rejects_symlink_and_preserves_regular_resume() {
    use base64::Engine as _;
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
        // Repeat the unchanged production cycle: unsafe input remains failed;
        // successful resume becomes a state-backed skip with no second GET.
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
                let rows = reopened.get_downloaded_page(0, 10).await.unwrap();
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

#[tokio::test]
async fn complete_part_416_recovers_persists_and_skips_after_reopen() {
    use base64::Engine as _;
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
