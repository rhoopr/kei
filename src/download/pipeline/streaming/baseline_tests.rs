//! Baseline matching through the real streaming producer, transfer and SQLite owners.

use std::path::Path;
use std::sync::Arc;

use base64::Engine as _;
use futures_util::stream;
use rusqlite::{Connection, types::Value};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{StreamRuntime, stream_and_download_from_stream};
use crate::download::pipeline::outcome::StreamingResult;
use crate::download::{DownloadConfig, DownloadControls, DownloadReporting, DownloadRunMode};
use crate::icloud::photos::PhotoAsset;
use crate::state::{DownloadStateStore, SqliteStateDb, VersionSizeKey};
use crate::test_helpers::TestAssetRecord;

const STILL: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/data/media/pattern.jpg"
));
const MOTION: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/data/media/pattern.mov"
));

fn generation(tag: u8) -> String {
    base64::engine::general_purpose::STANDARD.encode([tag; 32])
}

fn asset(server: &MockServer, companion: bool) -> PhotoAsset {
    let mut fields = json!({
        "filenameEnc": {"value":"IMG_0001.JPG","type":"STRING"},
        "itemType":{"value":"public.jpeg"},
        "resOriginalRes":{"value":{
            "size":STILL.len(),"downloadURL":format!("{}/still",server.uri()),
            "fileChecksum":generation(1)
        }},
        "resOriginalFileType":{"value":"public.jpeg"}
    });
    if companion {
        fields["resOriginalVidComplRes"] = json!({"value":{
            "size":MOTION.len(),"downloadURL":format!("{}/motion",server.uri()),
            "fileChecksum":generation(2)
        }});
        fields["resOriginalVidComplFileType"] = json!({"value":"com.apple.quicktime-movie"});
    }
    PhotoAsset::new(
        json!({"recordName":"current-master","fields":fields}),
        json!({"recordName":"current-child","fields":{
            "assetDate":{"value":1736899200000.0},"addedDate":{"value":1736899200000.0}
        }}),
    )
    .with_source_zone(Arc::from("SharedSync"))
}

fn config(root: &Path, db: Arc<SqliteStateDb>) -> Arc<DownloadConfig> {
    let mut config = DownloadConfig::test_default();
    config.directory = Arc::from(root);
    config.folder_structure = "none".into();
    config.library = "SharedSync".into();
    config.state_db = Some(db);
    config.concurrent_downloads = 1;
    Arc::new(config)
}

async fn run(
    root: &Path,
    db: Arc<SqliteStateDb>,
    assets: Vec<PhotoAsset>,
    mode: DownloadRunMode,
) -> StreamingResult {
    let count = assets.len() as u64;
    stream_and_download_from_stream(
        &reqwest::Client::new(),
        stream::iter(assets.into_iter().map(Ok::<_, anyhow::Error>)),
        &config(root, db),
        DownloadControls::new(mode, DownloadReporting::hidden()),
        count,
        CancellationToken::new(),
        StreamRuntime::new(None, None),
    )
    .await
    .unwrap()
}

async fn seed(db_path: &Path, media: &Path) {
    std::fs::create_dir_all(media.parent().unwrap()).unwrap();
    std::fs::write(media, STILL).unwrap();
    let db = SqliteStateDb::open(db_path).await.unwrap();
    let record = TestAssetRecord::new("historical-master")
        .library("PrimarySync")
        .filename("IMG_0001.JPG")
        .checksum(&generation(1))
        .size(STILL.len() as u64)
        .build();
    db.upsert_seen(&record).await.unwrap();
    let digest = data_encoding::HEXLOWER.encode(&Sha256::digest(STILL));
    db.mark_verified_download(
        "PrimarySync",
        "historical-master",
        "original",
        media,
        &digest,
        Some(&digest),
        false,
    )
    .await
    .unwrap();
}

fn old_rows(db_path: &Path) -> Vec<Vec<Value>> {
    let conn = Connection::open(db_path).unwrap();
    let mut values = Vec::new();
    for table in ["assets", "asset_metadata_paths"] {
        let mut statement = conn
            .prepare(&format!(
                "SELECT * FROM {table} WHERE library='PrimarySync' AND id='historical-master'"
            ))
            .unwrap();
        let columns = statement.column_count();
        values.extend(
            statement
                .query_map([], |row| {
                    (0..columns)
                        .map(|column| row.get(column))
                        .collect::<Result<Vec<Value>, _>>()
                })
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
        );
    }
    values
}

fn current_rows(db_path: &Path) -> Vec<(String, String, String)> {
    let conn = Connection::open(db_path).unwrap();
    conn.prepare("SELECT version_size,status,COALESCE(local_path,'') FROM assets WHERE library='SharedSync' AND id='current-child' ORDER BY version_size").unwrap()
        .query_map([], |row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).unwrap()
        .collect::<Result<Vec<_>,_>>().unwrap()
}

async fn mount(server: &MockServer, companion_downloads: u64) {
    Mock::given(method("GET"))
        .and(path("/still"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(STILL)
                .insert_header("content-type", "image/jpeg"),
        )
        .expect(0)
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/motion"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(MOTION)
                .insert_header("content-type", "video/quicktime"),
        )
        .expect(companion_downloads)
        .mount(server)
        .await;
}

#[tokio::test]
async fn baseline_stream_reuses_mixed_label_receipts_and_stable_paths_across_reopen() {
    let server = crate::start_wiremock_or_skip!();
    mount(&server, 0).await;
    for name in ["IMG_0001.JPG", "renamed-legacy-suffixed.JPG"] {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("photos");
        let media = root.join(name);
        let db_path = directory.path().join("state.db");
        seed(&db_path, &media).await;
        let before = old_rows(&db_path);
        let mtime = std::fs::metadata(&media).unwrap().modified().unwrap();

        let db = Arc::new(SqliteStateDb::open(&db_path).await.unwrap());
        let preview = run(
            &root,
            db.clone(),
            vec![asset(&server, false)],
            DownloadRunMode::PrintFilenames,
        )
        .await;
        assert!(preview.printed_filenames.is_empty());
        assert!(
            current_rows(&db_path).is_empty(),
            "preview must not adopt state"
        );
        drop(db);

        for cycle in 0..3 {
            let db = Arc::new(SqliteStateDb::open(&db_path).await.unwrap());
            let result = run(
                &root,
                db.clone(),
                vec![asset(&server, false)],
                DownloadRunMode::Download,
            )
            .await;
            assert_eq!(result.downloaded, 0, "cycle {cycle}: {result:?}");
            assert_eq!(result.state_write_failures, 0, "cycle {cycle}: {result:?}");
            assert!(result.failed.is_empty());
            assert_eq!(result.skip_summary.on_disk, 1, "cycle {cycle}: {result:?}");
            assert_eq!(
                current_rows(&db_path),
                vec![(
                    "original".into(),
                    "downloaded".into(),
                    media.to_string_lossy().into_owned()
                )]
            );
            let quiet = run(&root, db.clone(), Vec::new(), DownloadRunMode::Download).await;
            assert_eq!(quiet.downloaded, 0);
            drop(db);
            assert_eq!(
                old_rows(&db_path),
                before,
                "old receipt changed in cycle {cycle}"
            );
            assert_eq!(std::fs::read(&media).unwrap(), STILL);
            assert_eq!(
                std::fs::metadata(&media).unwrap().modified().unwrap(),
                mtime
            );
        }
        let files = std::fs::read_dir(&root)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(files.len(), 1, "no duplicate or staging file");
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn baseline_stream_wrong_same_size_bytes_stay_pending_without_transfer() {
    let server = crate::start_wiremock_or_skip!();
    mount(&server, 0).await;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("photos");
    let media = root.join("IMG_0001.JPG");
    let db_path = directory.path().join("state.db");
    seed(&db_path, &media).await;
    let mut changed = STILL.to_vec();
    let last = changed.len() - 1;
    changed[last] ^= 1;
    std::fs::write(&media, &changed).unwrap();
    let before = old_rows(&db_path);
    let mtime = std::fs::metadata(&media).unwrap().modified().unwrap();

    for cycle in 0..3 {
        let db = Arc::new(SqliteStateDb::open(&db_path).await.unwrap());
        let preview = run(
            &root,
            db.clone(),
            vec![asset(&server, false)],
            DownloadRunMode::PrintFilenames,
        )
        .await;
        assert!(preview.printed_filenames.is_empty());
        let result = run(
            &root,
            db.clone(),
            vec![asset(&server, false)],
            DownloadRunMode::Download,
        )
        .await;
        assert_eq!(result.downloaded, 0, "cycle {cycle}: {result:?}");
        assert_eq!(result.skip_summary.on_disk, 0);
        assert_eq!(result.state_write_failures, 0, "cycle {cycle}: {result:?}");
        assert_eq!(result.skip_summary.by_state, 0);
        assert_eq!(result.skip_summary.uncertain, 1);
        let (outcome, stats) =
            crate::download::pipeline::test_support::build_zero_download_outcome(
                result,
                crate::download::DownloadControls::download_hidden(),
            )
            .await;
        assert!(matches!(
            outcome,
            crate::download::DownloadOutcome::PartialFailure { failed_count: 1 }
        ));
        assert_eq!(stats.skipped.uncertain, 1);
        assert!(
            !stats.sync_token_blocked,
            "durable pending work does not veto observed source progress"
        );
        let rows = current_rows(&db_path);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1, "pending");
        let conn = Connection::open(&db_path).unwrap();
        let verification: (String,String) = conn.query_row("SELECT state,reason FROM asset_verifications WHERE library='SharedSync' AND id='current-child' AND version_size='original'", [], |row|Ok((row.get(0)?,row.get(1)?))).unwrap();
        assert_eq!(
            verification,
            ("unknown".into(), "baseline_match_uncertain".into())
        );
        let quiet = run(&root, db.clone(), Vec::new(), DownloadRunMode::Download).await;
        assert_eq!(quiet.downloaded, 0);
        assert_eq!(current_rows(&db_path)[0].1, "pending");
        drop(db);
        assert_eq!(old_rows(&db_path), before);
        assert_eq!(std::fs::read(&media).unwrap(), changed);
        assert_eq!(
            std::fs::metadata(&media).unwrap().modified().unwrap(),
            mtime
        );
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn baseline_stream_downloads_only_missing_companion_with_preview_parity() {
    let server = crate::start_wiremock_or_skip!();
    mount(&server, 1).await;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("photos");
    let media = root.join("IMG_0001.JPG");
    let db_path = directory.path().join("state.db");
    seed(&db_path, &media).await;
    let before = old_rows(&db_path);
    let mtime = std::fs::metadata(&media).unwrap().modified().unwrap();
    let db = Arc::new(SqliteStateDb::open(&db_path).await.unwrap());
    let preview = run(
        &root,
        db.clone(),
        vec![asset(&server, true)],
        DownloadRunMode::PrintFilenames,
    )
    .await;
    assert_eq!(preview.printed_filenames.len(), 1, "{preview:?}");
    let destination = preview.printed_filenames[0].clone();
    assert!(!destination.exists());
    let result = run(
        &root,
        db.clone(),
        vec![asset(&server, true)],
        DownloadRunMode::Download,
    )
    .await;
    assert_eq!(result.downloaded, 1, "{result:?}");
    assert!(result.failed.is_empty(), "{result:?}");
    assert_eq!(result.state_write_failures, 0, "{result:?}");
    assert_eq!(std::fs::read(&destination).unwrap(), MOTION);
    assert_eq!(std::fs::read(&media).unwrap(), STILL);
    assert_eq!(
        std::fs::metadata(&media).unwrap().modified().unwrap(),
        mtime
    );
    assert_eq!(old_rows(&db_path), before);
    let rows = current_rows(&db_path);
    assert_eq!(rows.len(), 2);
    assert!(
        rows.iter()
            .any(|row| row.0 == VersionSizeKey::LiveOriginal.as_str()
                && row.2 == destination.to_string_lossy())
    );
    drop(db);

    for cycle in 1..3 {
        let db = Arc::new(SqliteStateDb::open(&db_path).await.unwrap());
        let preview = run(
            &root,
            db.clone(),
            vec![asset(&server, true)],
            DownloadRunMode::PrintFilenames,
        )
        .await;
        assert!(
            preview.printed_filenames.is_empty(),
            "cycle {cycle}: {preview:?}"
        );
        let repeated = run(
            &root,
            db,
            vec![asset(&server, true)],
            DownloadRunMode::Download,
        )
        .await;
        assert_eq!(repeated.downloaded, 0, "cycle {cycle}: {repeated:?}");
        assert_eq!(
            repeated.state_write_failures, 0,
            "cycle {cycle}: {repeated:?}"
        );
        assert_eq!(current_rows(&db_path), rows);
        assert_eq!(old_rows(&db_path), before);
        assert_eq!(std::fs::read(&media).unwrap(), STILL);
        assert_eq!(
            std::fs::metadata(&media).unwrap().modified().unwrap(),
            mtime
        );
    }
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url.path(), "/motion");
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn baseline_stream_enabled_metadata_writers_preserve_existing_media_and_sidecar() {
    let server = crate::start_wiremock_or_skip!();
    mount(&server, 0).await;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("photos");
    let media = root.join("renamed-old.JPG");
    let sidecar = root.join("renamed-old.JPG.xmp");
    let db_path = directory.path().join("state.db");
    seed(&db_path, &media).await;
    std::fs::write(&sidecar, b"existing sidecar metadata").unwrap();
    let media_time = std::fs::metadata(&media).unwrap().modified().unwrap();
    let sidecar_time = std::fs::metadata(&sidecar).unwrap().modified().unwrap();
    let before = old_rows(&db_path);
    for cycle in 0..2 {
        let db = Arc::new(SqliteStateDb::open(&db_path).await.unwrap());
        let mut configured = (*config(&root, db)).clone();
        configured.metadata.embed_xmp = true;
        configured.metadata.xmp_sidecar = true;
        configured.metadata.set_exif_datetime = true;
        let result = stream_and_download_from_stream(
            &reqwest::Client::new(),
            stream::iter(vec![Ok::<_, anyhow::Error>(asset(&server, false))]),
            &Arc::new(configured),
            DownloadControls::download_hidden(),
            1,
            CancellationToken::new(),
            StreamRuntime::new(None, None),
        )
        .await
        .unwrap();
        assert_eq!(result.downloaded, 0, "cycle {cycle}: {result:?}");
        assert_eq!(result.state_write_failures, 0, "cycle {cycle}: {result:?}");
        assert_eq!(std::fs::read(&media).unwrap(), STILL);
        assert_eq!(
            std::fs::read(&sidecar).unwrap(),
            b"existing sidecar metadata"
        );
        assert_eq!(
            std::fs::metadata(&media).unwrap().modified().unwrap(),
            media_time
        );
        assert_eq!(
            std::fs::metadata(&sidecar).unwrap().modified().unwrap(),
            sidecar_time
        );
        assert_eq!(old_rows(&db_path), before);
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}

mod album_tests;

mod provider_signature;
