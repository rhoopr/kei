use std::fs;
use std::sync::Arc;
#[cfg(feature = "xmp")]
use std::sync::atomic::AtomicUsize;
use std::time::{Duration, UNIX_EPOCH};

use anyhow::Result;
#[cfg(feature = "xmp")]
use reqwest::Client;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

use crate::download::error::DownloadError;
use crate::download::filter::{DownloadTask, MetadataPayload};
use crate::download::metadata_rewrite::MetadataFlags;
#[cfg(feature = "xmp")]
use crate::download::pipeline::pass::{PassConfig, run_download_pass};
#[cfg(feature = "xmp")]
use crate::download::pipeline::streaming::{StreamRuntime, stream_and_download_from_stream};
use crate::download::pipeline::task::{
    DownloadSingleContext, DownloadTaskErrorClass, classify_download_task_error,
    download_single_task, set_file_mtime,
};
#[cfg(feature = "xmp")]
use crate::download::pipeline::test_support::{MINIMAL_JPEG, sidecar_path_for};
#[cfg(feature = "xmp")]
use crate::download::{DownloadConfig, DownloadControls, DownloadReporting, DownloadStore};
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
        publication: crate::download::file::FinalPublication::NoReplace,
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
        publication: crate::download::file::FinalPublication::NoReplace,
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
