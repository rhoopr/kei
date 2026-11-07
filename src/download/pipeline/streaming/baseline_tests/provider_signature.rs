//! Preview and execution share provider proof without widening production visibility.
use super::{MOTION, STILL as MEDIA};
use crate::download;
use crate::download::{DownloadConfig, DownloadControls, DownloadReporting, DownloadRunMode};
use crate::icloud::photos::PhotoAsset;
use crate::state::{DownloadStateStore, SqliteStateDb};
use base64::Engine as _;
use rusqlite::types::Value as SqlValue;
use serde_json::{Value, json};
use sha1::{Digest, Sha1};
use std::path::Path;
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

fn database_snapshot(path: &std::path::Path) -> Vec<(String, Vec<Vec<rusqlite::types::Value>>)> {
    let conn = rusqlite::Connection::open(path).unwrap();
    let mut tables = conn.prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name").unwrap();
    let names: Vec<String> = tables
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    names
        .into_iter()
        .map(|name| {
            let mut stmt = conn.prepare(&format!("SELECT * FROM \"{name}\"")).unwrap();
            let count = stmt.column_count();
            let mut rows: Vec<Vec<rusqlite::types::Value>> = stmt
                .query_map([], |row| (0..count).map(|i| row.get(i)).collect())
                .unwrap()
                .map(Result::unwrap)
                .collect();
            rows.sort_by_key(|row| format!("{row:?}"));
            (name, rows)
        })
        .collect()
}

fn query_snapshot(path: &std::path::Path, sql: &str) -> Vec<Vec<rusqlite::types::Value>> {
    let conn = rusqlite::Connection::open(path).unwrap();
    let mut stmt = conn.prepare(sql).unwrap();
    let count = stmt.column_count();
    stmt.query_map([], |row| (0..count).map(|i| row.get(i)).collect())
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn tree_snapshot(
    root: &std::path::Path,
) -> Vec<(std::ffi::OsString, Vec<u8>, std::time::SystemTime)> {
    let mut entries: Vec<_> = std::fs::read_dir(root)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            let metadata = std::fs::symlink_metadata(entry.path()).unwrap();
            assert!(metadata.file_type().is_file());
            (
                entry.file_name(),
                std::fs::read(entry.path()).unwrap(),
                metadata.modified().unwrap(),
            )
        })
        .collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries
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

async fn run_raw_candidate(
    db_path: &Path,
    root: &Path,
    records: &[Value],
    mode: DownloadRunMode,
) -> download::pipeline::outcome::StreamingResult {
    let db = Arc::new(
        SqliteStateDb::open_owned(db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    let asset = PhotoAsset::new(records[0].clone(), records[1].clone())
        .with_source_zone(Arc::from(ZONE))
        .with_state_record_name(Arc::from("asset-current-existing"));
    let mut configured = DownloadConfig::test_default();
    configured.directory = root.into();
    configured.folder_structure = "none".into();
    configured.library = ZONE.into();
    configured.state_db = Some(db.clone());
    let configured = Arc::new(configured);
    assert!(configured.primary_layout_source.is_none() && configured.selection_run.is_none());
    let result = download::pipeline::stream_and_download_from_stream(
        &reqwest::Client::new(),
        futures_util::stream::iter(vec![Ok::<_, anyhow::Error>(asset)]),
        &configured,
        DownloadControls::new(mode, DownloadReporting::hidden()),
        1,
        CancellationToken::new(),
        download::pipeline::StreamRuntime::new(None, None),
    )
    .await
    .unwrap();
    assert!(result.enumeration_complete, "{result:?}");
    drop(configured);
    assert_eq!(
        Arc::strong_count(&db),
        1,
        "capture/config cannot survive reopen"
    );
    drop(db);
    eprintln!("PROVIDER_B_CYCLE nonempty=1 {mode:?} {result:?}");
    result
}

#[tokio::test]
async fn v027_provider_signature_preview_execution_parity_for_raw_missing_receipts() {
    for failure in [None, Some("tail"), Some("unsupported"), Some("foreign")] {
        let server = crate::test_helpers::start_wiremock_or_skip("preview parity")
            .await
            .expect("acceptance server must bind");
        Mock::given(method("GET"))
            .and(path("/motion"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(MOTION)
                    .insert_header("content-type", "video/quicktime"),
            )
            .expect(0)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("photos");
        std::fs::create_dir_all(&root).unwrap();
        let old_path = root.join("existing.JPG");
        let mut original = MEDIA.to_vec();
        original.resize(original.len().max(65_537), 42);
        assert!(original.len() > 65_536);
        let source_sha = format!("{:x}", sha2::Sha256::digest(&original));
        Mock::given(method("GET"))
            .and(path("/still"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(original.clone()))
            .expect(u64::from(failure == Some("foreign")))
            .mount(&server)
            .await;
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
        let motion_path = root.join("existing.MOV");
        std::fs::write(&motion_path, MOTION).unwrap();
        let foreign_path = old_path.clone();
        let historical_path = root.join("primary-history.JPG");
        let historical_motion = root.join("primary-history.MOV");
        std::fs::write(&historical_path, &local).unwrap();
        std::fs::write(&historical_motion, MOTION).unwrap();
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
        let sha = download::file::compute_sha256(&historical_path)
            .await
            .unwrap();
        if failure == Some("tail") {
            assert_ne!(sha, source_sha);
        }
        db.mark_verified_download(
            "PrimarySync",
            "old-primary-master",
            "original",
            &historical_path,
            &sha,
            None,
            false,
        )
        .await
        .unwrap();
        let motion_record = crate::test_helpers::TestAssetRecord::new("old-primary-master")
            .library("PrimarySync")
            .version_size(crate::state::VersionSizeKey::LiveOriginal)
            .filename("existing.MOV")
            .checksum(&signature(MOTION))
            .size(MOTION.len() as u64)
            .build();
        db.upsert_seen(&motion_record).await.unwrap();
        let motion_sha = format!("{:x}", sha2::Sha256::digest(MOTION));
        db.mark_verified_download(
            "PrimarySync",
            "old-primary-master",
            "live_original",
            &historical_motion,
            &motion_sha,
            None,
            false,
        )
        .await
        .unwrap();
        for child in ["historical-child-a", "historical-child-b"] {
            db.upsert_asset_master_mapping("PrimarySync", child, "old-primary-master")
                .await
                .unwrap();
        }
        db.set_metadata_capture_revision_for_test("PrimarySync", "old-primary-master", 0);
        let candidate = db
            .get_metadata_capture_candidates("PrimarySync", 1, 1)
            .await
            .unwrap()
            .remove(0);
        assert!(
            db.defer_metadata_capture_ambiguity(&candidate, 1)
                .await
                .unwrap()
        );
        if failure == Some("foreign") {
            let other = crate::test_helpers::TestAssetRecord::new("asset-other-shared")
                .library(ZONE)
                .filename("existing.JPG")
                .checksum(&checksum)
                .size(original.len() as u64)
                .build();
            db.upsert_seen(&other).await.unwrap();
            let hash = download::file::compute_sha256(&foreign_path).await.unwrap();
            db.mark_verified_download(
                ZONE,
                "asset-other-shared",
                "original",
                &foreign_path,
                &hash,
                Some(&hash),
                false,
            )
            .await
            .unwrap();
        }
        let old_rows = historical_rows(&db_path);
        let mappings_sql = "SELECT * FROM asset_master_mappings WHERE library='PrimarySync' AND master_record_name='old-primary-master' ORDER BY asset_record_name";
        let mappings = query_snapshot(&db_path, mappings_sql);
        let mapping_names = query_snapshot(
            &db_path,
            "SELECT asset_record_name,master_record_name FROM asset_master_mappings WHERE library='PrimarySync' AND master_record_name='old-primary-master' ORDER BY asset_record_name",
        );
        assert_eq!(
            mapping_names,
            vec![
                vec![
                    SqlValue::Text("historical-child-a".into()),
                    SqlValue::Text("old-primary-master".into())
                ],
                vec![
                    SqlValue::Text("historical-child-b".into()),
                    SqlValue::Text("old-primary-master".into())
                ],
            ]
        );
        let debt_sql = "SELECT * FROM metadata_capture_retries WHERE library='PrimarySync' AND asset_id='old-primary-master' ORDER BY target_revision";
        let debt = query_snapshot(&db_path, debt_sql);
        assert_eq!(
            query_snapshot(
                &db_path,
                "SELECT library,asset_id,target_revision,attempts FROM metadata_capture_retries WHERE library='PrimarySync' AND asset_id='old-primary-master'"
            ),
            vec![vec![
                SqlValue::Text("PrimarySync".into()),
                SqlValue::Text("old-primary-master".into()),
                SqlValue::Integer(1),
                SqlValue::Integer(1)
            ]]
        );
        let foreign_asset_sql = "SELECT * FROM assets WHERE library='SharedSync-v027-synthetic' AND id='asset-other-shared' ORDER BY version_size";
        let foreign_path_sql = "SELECT * FROM asset_metadata_paths WHERE library='SharedSync-v027-synthetic' AND id='asset-other-shared' ORDER BY version_size,local_path";
        let foreign_asset_rows = query_snapshot(&db_path, foreign_asset_sql);
        let foreign_path_rows = query_snapshot(&db_path, foreign_path_sql);
        let foreign_count = usize::from(failure == Some("foreign"));
        assert_eq!(foreign_asset_rows.len(), foreign_count);
        assert_eq!(foreign_path_rows.len(), foreign_count);
        assert_eq!(
            query_snapshot(
                &db_path,
                "SELECT source_checksum FROM asset_metadata_paths WHERE library='PrimarySync' ORDER BY version_size"
            ),
            vec![vec![SqlValue::Null], vec![SqlValue::Null]]
        );
        assert!(query_snapshot(&db_path, "SELECT * FROM asset_metadata_paths WHERE library='SharedSync-v027-synthetic' AND id='asset-current-existing'").is_empty());
        assert!(query_snapshot(&db_path, "SELECT * FROM download_receipts WHERE library='SharedSync-v027-synthetic' AND child='asset-current-existing'").is_empty());
        assert_eq!(Arc::strong_count(&db), 1);
        drop(db);
        let before_preview = database_snapshot(&db_path);
        let before_tree = tree_snapshot(&root);
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
        let preview =
            run_raw_candidate(&db_path, &root, &records, DownloadRunMode::PrintFilenames).await;
        assert_eq!(
            preview.printed_filenames.len(),
            foreign_count,
            "{failure:?}: {preview:?}"
        );
        let selected = preview.printed_filenames.first().cloned();
        if let Some(path) = &selected {
            assert_ne!(path, &foreign_path);
            assert!(!path.exists());
        }
        assert_eq!(
            database_snapshot(&db_path),
            before_preview,
            "preview mutated state: {failure:?}"
        );
        assert_eq!(
            tree_snapshot(&root),
            before_tree,
            "preview mutated media: {failure:?}"
        );
        assert!(server.received_requests().await.unwrap().is_empty());
        eprintln!(
            "PROVIDER_B_INITIAL parity {failure:?} current_receipt_absent=true requested_foreign={foreign_count} compact=0 bytes={} tail_offset={} path={old_path:?} checksum={checksum} expected_sha256={source_sha}",
            original.len(),
            original.len() - 1
        );
        let execution =
            run_raw_candidate(&db_path, &root, &records, DownloadRunMode::Download).await;
        assert_eq!(
            execution.downloaded, foreign_count,
            "{failure:?}: {execution:?}"
        );
        assert!(execution.failed.is_empty());
        assert_eq!(execution.state_write_failures, 0, "{execution:?}");
        let uncertain = usize::from(matches!(failure, Some("tail" | "unsupported")));
        assert_eq!(execution.skip_summary.uncertain, uncertain);
        let connection = rusqlite::Connection::open(&db_path).unwrap();
        for (version, expected) in [
            (
                "original",
                if failure.is_none() || failure == Some("foreign") {
                    "downloaded"
                } else {
                    "pending"
                },
            ),
            ("live_original", "downloaded"),
        ] {
            let status: String = connection.query_row("SELECT status FROM assets WHERE library=?1 AND id='asset-current-existing' AND version_size=?2", rusqlite::params![ZONE, version], |row| row.get(0)).unwrap();
            assert_eq!(status, expected, "{failure:?} {version}");
            if expected == "downloaded" {
                let path: String = connection.query_row("SELECT local_path FROM assets WHERE library=?1 AND id='asset-current-existing' AND version_size=?2", rusqlite::params![ZONE, version], |row| row.get(0)).unwrap();
                assert_eq!(
                    std::path::Path::new(&path),
                    if version == "original" {
                        selected.as_deref().unwrap_or(old_path.as_path())
                    } else {
                        motion_path.as_path()
                    }
                );
                let proof: (String, String, String) = connection.query_row("SELECT provider_checksum,source_checksum,local_checksum FROM asset_metadata_paths WHERE library=?1 AND id='asset-current-existing' AND version_size=?2 AND local_path=?3", rusqlite::params![ZONE,version,path], |row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).unwrap();
                assert_eq!(
                    proof,
                    if version == "original" {
                        (checksum.clone(), source_sha.clone(), source_sha.clone())
                    } else {
                        (signature(MOTION), motion_sha.clone(), motion_sha.clone())
                    }
                );
                eprintln!("PROVIDER_B_PROOF parity {failure:?} {version} path={path:?} {proof:?}");
            }
        }
        drop(connection);
        let settled_sql = "SELECT version_size,status,local_path,local_checksum FROM assets WHERE library='SharedSync-v027-synthetic' AND id='asset-current-existing' ORDER BY version_size";
        let settled = query_snapshot(&db_path, settled_sql);
        for reopen in 0..2 {
            let before = database_snapshot(&db_path);
            let preview =
                run_raw_candidate(&db_path, &root, &records, DownloadRunMode::PrintFilenames).await;
            assert!(preview.printed_filenames.is_empty());
            assert_eq!(database_snapshot(&db_path), before);
            let repeated =
                run_raw_candidate(&db_path, &root, &records, DownloadRunMode::Download).await;
            assert_eq!(repeated.downloaded, 0, "{repeated:?}");
            assert_eq!(repeated.state_write_failures, 0);
            assert_eq!(repeated.skip_summary.uncertain, uncertain);
            assert_eq!(query_snapshot(&db_path, settled_sql), settled);
            assert_eq!(
                server.received_requests().await.unwrap().len(),
                foreign_count
            );
            eprintln!("PROVIDER_B_REOPEN parity {failure:?} reopen={reopen} gets={foreign_count}");
        }
        assert_eq!(historical_rows(&db_path), old_rows);
        assert_eq!(
            query_snapshot(&db_path, mappings_sql),
            mappings,
            "historical sibling mappings changed: {failure:?}"
        );
        assert_eq!(
            query_snapshot(&db_path, debt_sql),
            debt,
            "ambiguity debt changed: {failure:?}"
        );
        assert_eq!(
            query_snapshot(&db_path, foreign_asset_sql),
            foreign_asset_rows,
            "other Shared asset changed: {failure:?}"
        );
        assert_eq!(
            query_snapshot(&db_path, foreign_path_sql),
            foreign_path_rows,
            "other Shared path receipt changed: {failure:?}"
        );
        let mut after_tree = tree_snapshot(&root);
        if let Some(path) = &selected {
            assert_eq!(std::fs::read(path).unwrap(), original);
            assert_eq!(after_tree.len(), before_tree.len() + 1);
            after_tree.retain(|entry| entry.0 != path.file_name().unwrap());
        }
        assert_eq!(
            after_tree, before_tree,
            "existing media changed: {failure:?}"
        );
        assert_eq!(std::fs::read(&old_path).unwrap(), local);
        assert_eq!(
            std::fs::metadata(&old_path).unwrap().modified().unwrap(),
            mtime
        );
        eprintln!(
            "PREVIEW_PARITY_REACHED {failure:?} mappings=2 debt=1 foreign_rows={} tree={}",
            foreign_count,
            before_tree.len()
        );
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), foreign_count);
        assert!(
            requests
                .iter()
                .all(|request| request.url.path() == "/still")
        );
        server.verify().await;
    }
}

#[tokio::test]
async fn v027_provider_signature_preview_execution_missing_motion_control() {
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
        assert!(original.len() > 65_536);
        let source_sha = format!("{:x}", sha2::Sha256::digest(&original));
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
        let historical_path = root.join("primary-history.JPG");
        std::fs::write(&historical_path, &local).unwrap();
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
        let local_sha = format!("{:x}", sha2::Sha256::digest(&local));
        assert_eq!(sha, local_sha);
        if failure == Some("tail") {
            assert_ne!(sha, source_sha);
        }
        db.mark_verified_download(
            "PrimarySync",
            "old-primary-master",
            "original",
            &historical_path,
            &sha,
            None,
            false,
        )
        .await
        .unwrap();
        let current = crate::test_helpers::TestAssetRecord::new("asset-current-existing")
            .library(ZONE)
            .filename("existing.JPG")
            .checksum(&checksum)
            .size(original.len() as u64)
            .build();
        db.upsert_seen(&current).await.unwrap();
        db.mark_verified_download(
            ZONE,
            "asset-current-existing",
            "original",
            &old_path,
            &sha,
            None,
            false,
        )
        .await
        .unwrap();
        let proof_sql = "SELECT provider_checksum,source_checksum,local_checksum,local_path FROM asset_metadata_paths WHERE library='SharedSync-v027-synthetic' AND id='asset-current-existing' AND version_size='original'";
        assert_eq!(
            query_snapshot(&db_path, proof_sql),
            vec![vec![
                checksum.clone().into(),
                SqlValue::Null,
                sha.clone().into(),
                old_path.to_string_lossy().into_owned().into()
            ]]
        );
        assert!(query_snapshot(&db_path, "SELECT * FROM download_receipts WHERE library='SharedSync-v027-synthetic' AND child='asset-current-existing'").is_empty());
        assert_eq!(
            query_snapshot(
                &db_path,
                "SELECT source_checksum FROM asset_metadata_paths WHERE library='PrimarySync'"
            ),
            vec![vec![SqlValue::Null]]
        );
        assert_eq!(Arc::strong_count(&db), 1);
        drop(db);
        let old_rows = historical_rows(&db_path);
        let before_preview = database_snapshot(&db_path);
        let before_tree = tree_snapshot(&root);
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
        let preview =
            run_raw_candidate(&db_path, &root, &records, DownloadRunMode::PrintFilenames).await;
        assert_eq!(preview.printed_filenames.len(), 1, "{preview:?}");
        let destination = preview.printed_filenames[0].clone();
        assert!(!destination.exists());
        assert_eq!(database_snapshot(&db_path), before_preview);
        assert_eq!(tree_snapshot(&root), before_tree);
        assert!(server.received_requests().await.unwrap().is_empty());
        eprintln!(
            "PROVIDER_B_INITIAL missing-motion {failure:?} source_unknown=true generic_local={sha} compact=0 path={old_path:?} checksum={checksum} expected_sha256={source_sha}"
        );
        let execution =
            run_raw_candidate(&db_path, &root, &records, DownloadRunMode::Download).await;
        assert_eq!(
            execution.downloaded,
            preview.printed_filenames.len(),
            "{execution:?}"
        );
        assert_eq!(execution.state_write_failures, 0, "{execution:?}");
        assert_eq!(
            execution.skip_summary.by_state, 0,
            "fresh verification, not state skip"
        );
        assert!(execution.failed.is_empty());
        assert_eq!(
            execution.skip_summary.uncertain,
            usize::from(failure.is_some())
        );
        assert_eq!(std::fs::read(&destination).unwrap(), MOTION);
        let connection = rusqlite::Connection::open(&db_path).unwrap();
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
        drop(connection);
        let proof = query_snapshot(&db_path, proof_sql);
        eprintln!("PROVIDER_B_PROOF missing-motion {failure:?} path={old_path:?} {proof:?}");
        // Fresh provider proof does not backfill the unknown source of an
        // already-downloaded generic path receipt.
        assert_eq!(
            proof,
            vec![vec![
                checksum.clone().into(),
                SqlValue::Null,
                local_sha.clone().into(),
                old_path.to_string_lossy().into_owned().into()
            ]]
        );
        let settled_sql = "SELECT version_size,status,local_path,local_checksum FROM assets WHERE library='SharedSync-v027-synthetic' AND id='asset-current-existing' ORDER BY version_size";
        let settled = query_snapshot(&db_path, settled_sql);
        for reopen in 0..2 {
            let before = database_snapshot(&db_path);
            let preview =
                run_raw_candidate(&db_path, &root, &records, DownloadRunMode::PrintFilenames).await;
            assert!(preview.printed_filenames.is_empty());
            assert_eq!(database_snapshot(&db_path), before);
            let repeated =
                run_raw_candidate(&db_path, &root, &records, DownloadRunMode::Download).await;
            assert_eq!(repeated.downloaded, 0, "{repeated:?}");
            assert_eq!(repeated.state_write_failures, 0);
            assert_eq!(
                repeated.skip_summary.uncertain,
                usize::from(failure.is_some())
            );
            assert_eq!(query_snapshot(&db_path, settled_sql), settled);
            assert_eq!(query_snapshot(&db_path, proof_sql), proof);
            assert_eq!(server.received_requests().await.unwrap().len(), 1);
            eprintln!("PROVIDER_B_REOPEN missing-motion {failure:?} reopen={reopen} gets=1");
        }
        assert_eq!(historical_rows(&db_path), old_rows);
        assert_eq!(std::fs::read(&old_path).unwrap(), local);
        assert_eq!(
            std::fs::metadata(&old_path).unwrap().modified().unwrap(),
            mtime
        );
        assert!(!root.join("existing.JPG").exists());
        let after_tree = tree_snapshot(&root);
        assert_eq!(after_tree.len(), before_tree.len() + 1);
        for entry in &before_tree {
            assert!(after_tree.contains(entry));
        }
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].url.path(), "/motion");
        eprintln!("PROVIDER_B_MISSING_REACHED {failure:?} gets=1 still={status}");
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
