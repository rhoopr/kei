//! Preview and execution share provider proof without widening production visibility.
use super::{MOTION, STILL as MEDIA};
use crate::download;
use crate::state::{DownloadStateStore, SqliteStateDb};
use base64::Engine as _;
use serde_json::{Value, json};
use sha1::{Digest, Sha1};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};
const ZONE: &str = "SharedSync-v027-synthetic";
fn baseline_owner() -> crate::state::db::account::AccountOwner {
    crate::state::db::account::AccountOwner::authenticated(
        "baseline@example.invalid",
        "com",
        &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-baseline"}})).unwrap(),
    )
    .unwrap()
}

fn historical_rows(path: &std::path::Path) -> Vec<Vec<rusqlite::types::Value>> {
    let conn = rusqlite::Connection::open(path).unwrap();
    let mut rows = Vec::new();
    for table in ["assets", "asset_metadata_paths"] {
        let mut stmt = conn
            .prepare(&format!(
                "SELECT * FROM {table} WHERE library='PrimarySync' ORDER BY id,version_size"
            ))
            .unwrap();
        let count = stmt.column_count();
        rows.extend(
            stmt.query_map([], |r| {
                (0..count)
                    .map(|i| r.get(i))
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap(),
        );
    }
    let mut stmt = conn.prepare(
        "SELECT * FROM metadata_capture_retries WHERE library='PrimarySync' ORDER BY asset_id,target_revision"
    ).unwrap();
    let count = stmt.column_count();
    rows.extend(
        stmt.query_map([], |r| {
            (0..count)
                .map(|i| r.get(i))
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap(),
    );
    rows
}

fn generation(n: u8) -> String {
    base64::engine::general_purpose::STANDARD.encode([n; 32])
}

fn zone() -> Value {
    json!({"zoneName":ZONE,"ownerRecordName":"_defaultOwner"})
}

fn pair(id: &str, filename: &str, url: &str, tag: u8) -> Vec<Value> {
    let mut records = full_album_page_with_download(
        ZONE,
        id,
        "unused",
        url,
        MEDIA.len() as u64,
        &generation(tag),
    )["records"]
        .as_array()
        .unwrap()
        .clone();
    records[0]["fields"]["filenameEnc"] = json!({"value":filename,"type":"STRING"});
    records[1]["fields"]["masterRef"]["value"]["zoneID"] = zone();
    records
}

fn signature(bytes: &[u8]) -> String {
    let mut hash = Sha1::new();
    hash.update(b"com.apple.XattrObjectSalt\0com.apple.DataObjectSalt\0");
    hash.update(bytes);
    let mut value = vec![1];
    value.extend_from_slice(&hash.finalize());
    base64::engine::general_purpose::STANDARD.encode(value)
}

#[tokio::test]
async fn v027_provider_signature_preview_execution_parity_for_raw_missing_receipts() {
    use crate::download::{DownloadConfig, DownloadControls, DownloadReporting, DownloadRunMode};
    use crate::icloud::photos::PhotoAsset;
    for failure in [None, Some("tail"), Some("unsupported")] {
        let server = crate::start_wiremock_or_skip!();
        Mock::given(method("GET"))
            .and(path("/still"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/motion"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(MOTION)
                    .insert_header("content-type", "video/quicktime"),
            )
            .expect(1)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("photos");
        std::fs::create_dir_all(&root).unwrap();
        let old_path = root.join("renamed-suffixed_2.JPG");
        let mut original = MEDIA.to_vec();
        original.resize(original.len().max(65_537), 42);
        let mut local = original.clone();
        if failure == Some("tail") {
            *local.last_mut().unwrap() ^= 1;
        }
        let mut checksum = signature(&original);
        if failure == Some("unsupported") {
            let mut decoded = base64::engine::general_purpose::STANDARD
                .decode(&checksum)
                .unwrap();
            decoded[0] = 4;
            checksum = base64::engine::general_purpose::STANDARD.encode(decoded);
        }
        std::fs::write(&old_path, &local).unwrap();
        let mtime = std::fs::metadata(&old_path).unwrap().modified().unwrap();
        let db_path = dir.path().join("state.db");
        let db = Arc::new(
            SqliteStateDb::open_owned(&db_path, &baseline_owner())
                .await
                .unwrap(),
        );
        let record = crate::test_helpers::TestAssetRecord::new("old-primary-master")
            .library("PrimarySync")
            .filename("existing.JPG")
            .checksum(&checksum)
            .size(original.len() as u64)
            .build();
        db.upsert_seen(&record).await.unwrap();
        let sha = download::file::compute_sha256(&old_path).await.unwrap();
        db.mark_verified_download(
            "PrimarySync",
            "old-primary-master",
            "original",
            &old_path,
            &sha,
            None,
            false,
        )
        .await
        .unwrap();
        let old_rows = historical_rows(&db_path);
        let mut records = pair(
            "current-existing",
            "existing.JPG",
            &format!("{}/still", server.uri()),
            1,
        );
        records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(checksum);
        records[0]["fields"]["resOriginalRes"]["value"]["size"] = json!(original.len());
        records[0]["fields"]["resOriginalVidComplRes"] = json!({"value":{
            "downloadURL":format!("{}/motion",server.uri()),"size":MOTION.len(),
            "fileChecksum":signature(MOTION)}});
        records[0]["fields"]["resOriginalVidComplFileType"] =
            json!({"value":"com.apple.quicktime-movie"});
        let asset = PhotoAsset::new(records[0].clone(), records[1].clone())
            .with_source_zone(Arc::from(ZONE))
            .with_state_record_name(Arc::from("asset-current-existing"));
        let mut configured = DownloadConfig::test_default();
        configured.directory = root.clone().into();
        configured.folder_structure = "none".into();
        configured.library = ZONE.into();
        configured.state_db = Some(db.clone());
        configured.concurrent_downloads = 1;
        let configured = Arc::new(configured);
        let run = |mode| {
            let asset = asset.clone();
            let configured = configured.clone();
            async move {
                download::pipeline::stream_and_download_from_stream(
                    &reqwest::Client::new(),
                    futures_util::stream::iter(vec![Ok::<_, anyhow::Error>(asset)]),
                    &configured,
                    DownloadControls::new(mode, DownloadReporting::hidden()),
                    1,
                    CancellationToken::new(),
                    download::pipeline::StreamRuntime::new(None, None),
                )
                .await
                .unwrap()
            }
        };
        let preview = run(DownloadRunMode::PrintFilenames).await;
        assert_eq!(preview.printed_filenames.len(), 1, "{preview:?}");
        let destination = preview.printed_filenames[0].clone();
        assert!(!destination.exists());
        let connection = rusqlite::Connection::open(&db_path).unwrap();
        let current_count: i64 = connection
            .query_row(
                "SELECT count(*) FROM assets WHERE library=?1",
                [ZONE],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            current_count, 0,
            "preview cannot adopt or admit current work"
        );
        let execution = run(DownloadRunMode::Download).await;
        assert_eq!(
            execution.downloaded,
            preview.printed_filenames.len(),
            "{execution:?}"
        );
        assert_eq!(execution.state_write_failures, 0, "{execution:?}");
        assert_eq!(std::fs::read(&destination).unwrap(), MOTION);
        let status: String = connection.query_row(
            "SELECT status FROM assets WHERE library=?1 AND id='asset-current-existing' AND version_size='original'",
            [ZONE], |row| row.get(0)).unwrap();
        assert_eq!(
            status,
            if failure.is_none() {
                "downloaded"
            } else {
                "pending"
            }
        );
        assert!(
            run(DownloadRunMode::PrintFilenames)
                .await
                .printed_filenames
                .is_empty()
        );
        assert_eq!(historical_rows(&db_path), old_rows);
        assert_eq!(std::fs::read(&old_path).unwrap(), local);
        assert_eq!(
            std::fs::metadata(&old_path).unwrap().modified().unwrap(),
            mtime
        );
        assert!(!root.join("existing.JPG").exists());
        server.verify().await;
    }
}

const RUN_CYCLE_ASSET_DATE_MS: i64 = 1_700_000_000_000;
fn full_album_page_with_download(
    zone: &str,
    record_name: &str,
    sync_token: &str,
    download_url: &str,
    size: u64,
    checksum: &str,
) -> serde_json::Value {
    serde_json::json!({
        "records": [
            {
                "recordName": record_name,
                "recordType": "CPLMaster",
                "fields": {
                    "filenameEnc": {"value": "cGhvdG8uanBn", "type": "STRING"},
                    "resOriginalRes": {
                        "value": {
                            "downloadURL": download_url,
                            "size": size,
                            "fileChecksum": checksum
                        }
                    },
                    "resOriginalWidth": {"value": 100, "type": "INT64"},
                    "resOriginalHeight": {"value": 100, "type": "INT64"},
                    "resOriginalFileType": {"value": "public.jpeg"},
                    "itemType": {"value": "public.jpeg"},
                    "adjustmentRenderType": {"value": 0, "type": "INT64"}
                },
                "recordChangeTag": "ct-master"
            },
            {
                "recordName": format!("asset-{record_name}"),
                "recordType": "CPLAsset",
                "fields": {
                    "masterRef": {
                        "value": {"recordName": record_name, "zoneID": {"zoneName": zone}},
                        "type": "REFERENCE"
                    },
                    "assetDate": {"value": RUN_CYCLE_ASSET_DATE_MS, "type": "TIMESTAMP"},
                    "addedDate": {"value": RUN_CYCLE_ASSET_DATE_MS, "type": "TIMESTAMP"}
                },
                "recordChangeTag": "ct-asset"
            }
        ],
        "syncToken": sync_token
    })
}
