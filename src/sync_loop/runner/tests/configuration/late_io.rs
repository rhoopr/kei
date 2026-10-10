//! Real syscall failures reached through config drift and the production cycle.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio_util::sync::CancellationToken;

use crate::sync_cycle::{ENUM_CONFIG_HASH_KEY, PENDING_DOWNLOAD_CONFIG_HASH_KEY, run_cycle};
use crate::sync_loop::test_support::{
    full_album_page_with_download, make_full_album_with_boxed_session, make_run_cycle_config,
    make_run_cycle_download_config_builder, make_run_cycle_library_state_with_album,
    make_shared_session_for_run_cycle,
};
use crate::{download, state};

#[derive(Clone)]
struct Session {
    records: serde_json::Value,
    sources: Arc<AtomicUsize>,
    lookups: Arc<AtomicUsize>,
}
#[async_trait::async_trait]
impl crate::icloud::photos::PhotosSession for Session {
    async fn post(
        &self,
        url: &str,
        _body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<serde_json::Value> {
        if url.contains("/records/lookup?") {
            self.lookups.fetch_add(1, Ordering::SeqCst);
            return Ok(self.records.clone());
        }
        self.sources.fetch_add(1, Ordering::SeqCst);
        if url.contains("/changes/zone?") {
            return Ok(
                serde_json::json!({"zones":[{"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"},"records":[],"syncToken":"recovered-cursor","moreComing":false}]}),
            );
        }
        if url.contains("/internal/records/query/batch") {
            return Ok(
                serde_json::json!({"batch":[{"records":[{"fields":{"itemCount":{"value":0}}}]}]}),
            );
        }
        Ok(serde_json::json!({"records":[],"syncToken":"recovered-cursor"}))
    }
    fn clone_box(&self) -> Box<dyn crate::icloud::photos::PhotosSession> {
        Box::new(self.clone())
    }
}

// The parent Linux matrix runs this child under its exclusive preload shim.
// An ordinary suite entry has no fault environment and performs no work.
#[test]
fn actual_reconciliation_fault_child() {
    let Some(root) = std::env::var_os("KEI_TEST_LATE_IO_ROOT") else {
        return;
    };
    let case = std::env::var("KEI_TEST_LATE_IO_CASE").unwrap();
    assert!(matches!(
        case.as_str(),
        "reconcile_sync" | "reconcile_parent"
    ));
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .block_on(run_case(&PathBuf::from(root), &case));
}

async fn run_case(root: &Path, case: &str) {
    use sha2::{Digest, Sha256};
    let bytes = b"\xff\xd8\xff\xe0\0\x10JFIF\0\x01\x01\0\0\x01\0\x01\0\0\xff\xd9";
    let config = make_run_cycle_config();
    let media = root.join("media");
    let old_root = media.join("old");
    let new_root = media.join("new");
    std::fs::create_dir_all(&old_root).unwrap();
    std::fs::create_dir_all(&new_root).unwrap();
    let source = old_root.join("photo.jpg");
    let unrelated = new_root.join("unrelated.xmp");
    std::fs::write(&source, bytes).unwrap();
    std::fs::write(&unrelated, b"unrelated metadata").unwrap();
    let checksum = data_encoding::HEXLOWER.encode(&Sha256::digest(bytes));
    let provider_checksum = data_encoding::BASE64.encode(&Sha256::digest(bytes));
    let server = wiremock::MockServer::start().await;
    let endpoint = format!("{}/never-fetch", server.uri());
    let records = full_album_page_with_download(
        "PrimarySync",
        "FAULT",
        "unused",
        &endpoint,
        bytes.len() as u64,
        &provider_checksum,
    );
    let asset = crate::icloud::photos::PhotoAsset::new(
        records["records"][0].clone(),
        records["records"][1].clone(),
    );
    let db_path = root.join("state.db");
    let owner = state::db::account::AccountOwner::authenticated(
        "reconcile@example.invalid",
        "com",
        &serde_json::from_value(serde_json::json!({"dsInfo":{"dsid":"reconcile-synthetic"}}))
            .unwrap(),
    )
    .unwrap();
    let db = Arc::new(
        state::SqliteStateDb::open_owned(&db_path, &owner)
            .await
            .unwrap(),
    );
    db.upsert_seen(
        &crate::test_helpers::TestAssetRecord::new("FAULT")
            .filename("photo.jpg")
            .size(bytes.len() as u64)
            .checksum(&provider_checksum)
            .created_at(asset.created())
            .build(),
    )
    .await
    .unwrap();
    db.mark_downloaded("PrimarySync", "FAULT", "original", &source, &checksum, None)
        .await
        .unwrap();
    db.upsert_asset_master_mapping("PrimarySync", "asset-FAULT", "FAULT")
        .await
        .unwrap();
    db.set_metadata(
        ENUM_CONFIG_HASH_KEY,
        &download::compute_config_hash(&config),
    )
    .await
    .unwrap();
    db.set_metadata("sync_token:PrimarySync", "old-cursor")
        .await
        .unwrap();
    db.set_metadata("sync_token:OtherZone", "retained-debt")
        .await
        .unwrap();
    let old_builder = make_run_cycle_download_config_builder(&old_root, db.clone());
    let new_builder = make_run_cycle_download_config_builder(&new_root, db.clone());
    let args = || {
        (
            download::SyncMode::Full,
            Arc::new(rustc_hash::FxHashSet::default()),
            Arc::new(download::AssetGroupings::default()),
            Arc::from("PrimarySync"),
        )
    };
    let (mode, excludes, groupings, library) = args();
    let old_hash = download::hash_download_config(&old_builder(mode, excludes, groupings, library));
    let (mode, excludes, groupings, library) = args();
    let new_config = new_builder(mode, excludes, groupings, library);
    let new_hash = download::hash_download_config(&new_config);
    let expected = download::filter::expected_paths_for(&asset, new_config.as_ref())
        .remove(0)
        .path;
    std::fs::create_dir_all(expected.parent().unwrap()).unwrap();
    std::fs::write(root.join("reconcile-target"), expected.to_str().unwrap()).unwrap();
    db.set_metadata(download::DOWNLOAD_CONFIG_HASH_KEY, &old_hash)
        .await
        .unwrap();
    db.set_metadata(PENDING_DOWNLOAD_CONFIG_HASH_KEY, &new_hash)
        .await
        .unwrap();
    let originals = census(&media);
    let original_receipt = rows(&db, "asset_metadata_paths");
    let ownership = rows(&db, "account_owner");
    drop(new_config);
    drop(old_builder);
    drop(new_builder);
    drop(db);
    let sources = Arc::new(AtomicUsize::new(0));
    let lookups = Arc::new(AtomicUsize::new(0));
    let album = make_full_album_with_boxed_session(
        "PrimarySync",
        Box::new(Session {
            records,
            sources: sources.clone(),
            lookups: lookups.clone(),
        }),
    );
    let lib =
        make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
    let (_session_dir, session) = make_shared_session_for_run_cycle().await;
    std::fs::write(root.join("fault-enabled"), b"enabled").unwrap();
    let mut preserved_partial = Vec::new();
    let mut completed_files = None;
    let mut completed_receipts = None;
    for cycle in 0..4 {
        let db = Arc::new(
            state::SqliteStateDb::open_owned(&db_path, &owner)
                .await
                .unwrap(),
        );
        let builder = make_run_cycle_download_config_builder(&new_root, db.clone());
        let lookups_before = lookups.load(Ordering::SeqCst);
        let result = run_cycle(
            &[&lib],
            &config,
            Some(db.as_ref()),
            false,
            &builder,
            download::DownloadControls::download_hidden(),
            &session,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        drop(builder);
        drop(db);
        // All cycle owners are released before SQL/state evidence, including
        // the still-faulted first cycle. Recovery removes only the shim switch.
        let db = state::SqliteStateDb::open_owned(&db_path, &owner)
            .await
            .unwrap();
        assert_eq!(rows(&db, "account_owner"), ownership);
        assert!(
            server.received_requests().await.unwrap().is_empty(),
            "reconciliation must reuse local media"
        );
        assert_eq!(std::fs::read(&source).unwrap(), bytes);
        assert_eq!(std::fs::read(&unrelated).unwrap(), b"unrelated metadata");
        for original in &originals {
            assert!(
                census(&media).contains(original),
                "source/unrelated bytes and publication identity stay exact"
            );
        }
        assert_eq!(
            db.get_metadata("sync_token:OtherZone")
                .await
                .unwrap()
                .as_deref(),
            Some("retained-debt")
        );
        assert_eq!(
            db.get_metadata(ENUM_CONFIG_HASH_KEY)
                .await
                .unwrap()
                .as_deref(),
            Some(download::compute_config_hash(&config).as_str())
        );
        assert_eq!(
            result.failed_count,
            usize::from(cycle == 0),
            "{case}: {result:?}"
        );
        assert_eq!(
            result.db_sync_token_advance_safe,
            cycle != 0,
            "{case}: {result:?}"
        );
        let catalog = db.get_downloaded_page(0, 10).await.unwrap();
        assert_eq!(catalog.len(), 1);
        assert_eq!(
            catalog[0].local_path.as_deref(),
            Some(if cycle == 0 {
                source.as_path()
            } else {
                expected.as_path()
            })
        );
        assert_eq!(
            catalog[0].local_checksum.as_deref(),
            Some(checksum.as_str())
        );
        assert_eq!(
            db.get_metadata(download::DOWNLOAD_CONFIG_HASH_KEY)
                .await
                .unwrap()
                .as_deref(),
            Some(if cycle == 0 {
                old_hash.as_str()
            } else {
                new_hash.as_str()
            })
        );
        assert_eq!(
            db.get_metadata(PENDING_DOWNLOAD_CONFIG_HASH_KEY)
                .await
                .unwrap()
                .as_deref(),
            if cycle == 0 {
                Some(new_hash.as_str())
            } else {
                None
            }
        );
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some(if cycle == 0 {
                "old-cursor"
            } else {
                "recovered-cursor"
            })
        );
        if cycle == 0 {
            let marker = std::fs::read_to_string(root.join("fault-reached")).unwrap();
            assert!(
                marker.contains("fsync errno=5 progress="),
                "{case}: {marker}"
            );
            assert!(!marker.contains("progress=0\n"));
            assert_eq!(
                sources.load(Ordering::SeqCst),
                0,
                "blocking failure must skip source dispatch"
            );
            assert_eq!(
                rows(&db, "asset_metadata_paths"),
                original_receipt,
                "no premature reconciled receipt"
            );
            assert_eq!(
                rows(&db, "reconciliation_paths").len(),
                1,
                "destination debt persists"
            );
            assert_eq!(expected.exists(), case == "reconcile_parent");
            if expected.exists() {
                assert_eq!(std::fs::read(&expected).unwrap(), bytes);
            }
            preserved_partial = census(&media)
                .into_iter()
                .filter(|entry| entry.0.to_string_lossy().ends_with(".kei-tmp"))
                .collect::<Vec<_>>();
            assert_eq!(
                preserved_partial.len(),
                usize::from(case == "reconcile_sync")
            );
            std::fs::remove_file(root.join("fault-enabled")).unwrap();
        } else {
            assert_eq!(std::fs::read(&expected).unwrap(), bytes);
            for entry in &preserved_partial {
                assert!(
                    census(&media).contains(entry),
                    "historical ambiguous partial stays exact"
                );
            }
            let receipts = (
                rows(&db, "assets"),
                rows(&db, "asset_metadata_paths"),
                rows(&db, "reconciliation_paths"),
                rows(&db, "owned_temp_files"),
            );
            if cycle == 1 {
                completed_files = Some(census(&media));
                completed_receipts = Some(receipts);
            } else {
                assert_eq!(census(&media), *completed_files.as_ref().unwrap());
                assert_eq!(receipts, *completed_receipts.as_ref().unwrap());
                assert_eq!(
                    lookups.load(Ordering::SeqCst),
                    lookups_before,
                    "quiet config avoids repeated reconciliation"
                );
            }
        }
    }
}

fn rows(db: &state::SqliteStateDb, table: &str) -> Vec<Vec<rusqlite::types::Value>> {
    let connection = db.acquire_lock("independent reconciliation rows").unwrap();
    let mut query = connection
        .prepare(&format!("SELECT * FROM {table}"))
        .unwrap();
    let columns = query.column_count();
    let mut rows: Vec<Vec<rusqlite::types::Value>> = query
        .query_map([], |row| {
            (0..columns).map(|column| row.get(column)).collect()
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    rows.sort_by_key(|row| format!("{row:?}"));
    rows
}

type FileEntry = (PathBuf, Vec<u8>, u64, u64, std::time::SystemTime);

fn census(root: &Path) -> Vec<FileEntry> {
    use std::os::unix::fs::MetadataExt;
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        let metadata = std::fs::symlink_metadata(&path).unwrap();
        if metadata.is_dir() {
            entries.extend(census(&path));
        } else {
            assert!(metadata.is_file());
            let bytes = std::fs::read(&path).unwrap();
            entries.push((
                path,
                bytes,
                metadata.ino(),
                metadata.nlink(),
                metadata.modified().unwrap(),
            ));
        }
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries
}
