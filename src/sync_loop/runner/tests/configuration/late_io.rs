//! Selected publication syscall failures through root drift and the production cycle.

use crate::state::SyncTokenStore;
use crate::state::{DownloadStateStore, ReportStateStore};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio_util::sync::CancellationToken;

use crate::sync_cycle::{ENUM_CONFIG_HASH_KEY, PENDING_DOWNLOAD_CONFIG_HASH_KEY, run_cycle};
use crate::sync_loop::test_support::{
    full_album_page_with_download, make_run_cycle_config, make_run_cycle_download_config_builder,
    make_run_cycle_library_state_with_album, make_shared_session_for_run_cycle,
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
        body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<serde_json::Value> {
        let request: serde_json::Value = serde_json::from_str(&body)?;
        if url.contains("/records/lookup?") {
            self.lookups.fetch_add(1, Ordering::SeqCst);
            let records: Vec<_> = request["records"].as_array().unwrap().iter().map(|wanted| {
                self.records["records"].as_array().unwrap().iter()
                    .find(|record| record["recordName"] == wanted["recordName"])
                    .cloned().unwrap_or_else(|| serde_json::json!({"recordName":wanted["recordName"],"serverErrorCode":"UNKNOWN_ITEM"}))
            }).collect();
            return Ok(serde_json::json!({"records":records}));
        }
        self.sources.fetch_add(1, Ordering::SeqCst);
        if url.contains("/changes/zone?") {
            let records = if request["zones"][0]["syncToken"].is_null() {
                self.records["records"].clone()
            } else {
                serde_json::json!([])
            };
            return Ok(
                serde_json::json!({"zones":[{"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"},"records":records,"syncToken":"recovered-cursor","moreComing":false}]}),
            );
        }
        if url.contains("/internal/records/query/batch") {
            return Ok(
                serde_json::json!({"batch":[{"records":[{"fields":{"itemCount":{"value":1}}}]}]}),
            );
        }
        assert!(url.contains("/records/query?"));
        let offset = request["query"]["filterBy"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|filter| filter["fieldName"] == "startRank")
            .and_then(|filter| filter["fieldValue"]["value"].as_u64())
            .unwrap_or(0);
        Ok(
            serde_json::json!({"records":if offset == 0 {self.records["records"].clone()} else {serde_json::json!([])},"syncToken":"recovered-cursor"}),
        )
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
    let endpoint = format!("{}/selected.jpg", server.uri());
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/selected.jpg"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(bytes.as_slice()))
        .mount(&server)
        .await;
    let mut records = full_album_page_with_download(
        "PrimarySync",
        "FAULT",
        "unused",
        &endpoint,
        bytes.len() as u64,
        &provider_checksum,
    );
    records["records"][1]["fields"]["masterRef"]["value"]["zoneID"]["ownerRecordName"] =
        serde_json::json!("_defaultOwner");
    records["records"][0]["fields"]["filenameEnc"]["value"] =
        serde_json::json!(data_encoding::BASE64.encode(b"photo.jpg"));
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
        &crate::test_helpers::TestAssetRecord::new("asset-FAULT")
            .filename("photo.jpg")
            .size(bytes.len() as u64)
            .checksum(&provider_checksum)
            .created_at(asset.created())
            .added_at(asset.added_date())
            .metadata(asset.metadata().clone())
            .build(),
    )
    .await
    .unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "asset-FAULT",
        "original",
        &source,
        &checksum,
        Some(&checksum),
    )
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
    let album = crate::icloud::photos::PhotoAlbum::new(
        crate::icloud::photos::PhotoAlbumConfig {
            params: Arc::new(std::collections::HashMap::new()),
            service_endpoint: Arc::from("https://example.com"),
            name: Arc::from(""),
            list_type: Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
            obj_type: Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
            query_filter: None,
            page_size: 100,
            zone_id: Arc::new(
                serde_json::json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"}),
            ),
            retry_config: crate::retry::RetryConfig::default(),
            container_id: None,
            cross_zone_sources: Vec::new(),
        },
        Box::new(Session {
            records,
            sources: sources.clone(),
            lookups: lookups.clone(),
        }),
    );
    let (_session_dir, session) = make_shared_session_for_run_cycle().await;
    std::fs::write(root.join("fault-enabled"), b"enabled").unwrap();
    let mut completed_files = None;
    let mut completed_receipts = None;
    let recovery_cycle = if case == "reconcile_parent" { 2 } else { 1 };
    for cycle in 0..recovery_cycle + 3 {
        let faulted = cycle < recovery_cycle;
        let db = Arc::new(
            state::SqliteStateDb::open_owned(&db_path, &owner)
                .await
                .unwrap(),
        );
        let mut selected = album.clone();
        selected.set_shadow_capture(
            crate::icloud::photos::inbox::ShadowCapture::new(db.clone(), owner.clone(), "com"),
            Arc::from("private"),
        );
        let lib = make_run_cycle_library_state_with_album(
            "PrimarySync",
            "sync_token:PrimarySync",
            selected,
        );
        let base = make_run_cycle_download_config_builder(&new_root, db.clone());
        let builder = |mode, excludes, groupings, library| {
            let mut config = (*base(mode, excludes, groupings, library)).clone();
            config.temp_suffix = Arc::from(".part");
            config.retry.max_retries = 0;
            Arc::new(config)
        };
        let lookups_before = lookups.load(Ordering::SeqCst);
        let requests_before = server.received_requests().await.unwrap().len();
        let result = run_cycle(
            &[&lib],
            &config,
            Some(db.as_ref()),
            &builder,
            download::DownloadControls::download_hidden(),
            &session,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        drop(lib);
        drop(base);
        drop(db);
        // All cycle owners are released before SQL/state evidence, including
        // the still-faulted first cycle. Recovery removes only the shim switch.
        let db = state::SqliteStateDb::open_owned(&db_path, &owner)
            .await
            .unwrap();
        if faulted {
            let marker = std::fs::read_to_string(root.join("fault-reached"))
                .expect("the selected publication syscall fault must execute");
            let operation = if case == "reconcile_sync" {
                "fdatasync"
            } else {
                "fsync"
            };
            assert!(
                marker.contains(&format!("{operation} errno=5 progress=")),
                "{case}: {marker}"
            );
            assert!(!marker.contains("progress=0\n"), "{case}: {marker}");
        }
        assert_eq!(rows(&db, "account_owner"), ownership);
        assert!(
            !server.received_requests().await.unwrap().is_empty(),
            "selected root drift must transfer into the new root"
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
        if faulted {
            assert!(result.failed_count > 0, "{case}: {result:?}");
            if case == "reconcile_parent" {
                let catalog = db.get_downloaded_page(0, 10).await.unwrap();
                assert_eq!(
                    catalog.len(),
                    1,
                    "failed finalization retains old publication"
                );
                assert_eq!(catalog[0].local_path.as_deref(), Some(source.as_path()));
                assert_eq!(
                    catalog[0].local_checksum.as_deref(),
                    Some(checksum.as_str())
                );
            }
        } else {
            assert_eq!(result.failed_count, 0, "{case}: {result:?}");
        }
        assert_eq!(
            result.db_sync_token_advance_safe, !faulted,
            "{case}: {result:?}"
        );
        // A failed selected transfer can mark the catalog row failed while its
        // prior-root receipt remains evidence. Only verified publication in the
        // new root may establish the new downloaded destination.
        for receipt in &original_receipt {
            assert!(
                rows(&db, "asset_metadata_paths").contains(receipt),
                "old-root receipt must survive selected publication"
            );
        }
        if !faulted {
            let catalog = db.get_downloaded_page(0, 10).await.unwrap();
            assert_eq!(catalog.len(), 1);
            assert_eq!(catalog[0].id.as_ref(), "asset-FAULT");
            assert_eq!(catalog[0].local_path.as_deref(), Some(expected.as_path()));
            assert_eq!(
                catalog[0].local_checksum.as_deref(),
                Some(checksum.as_str())
            );
        }
        assert_eq!(
            db.get_metadata(download::DOWNLOAD_CONFIG_HASH_KEY)
                .await
                .unwrap()
                .as_deref(),
            Some(if faulted {
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
            if faulted {
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
            Some(if faulted {
                "old-cursor"
            } else {
                "recovered-cursor"
            })
        );
        if faulted {
            assert!(
                sources.load(Ordering::SeqCst) > 0,
                "fault must follow current-selection enumeration"
            );
            assert_eq!(
                rows(&db, "asset_metadata_paths"),
                original_receipt,
                "no premature new-root receipt"
            );
            let connection = db
                .acquire_lock("selected publication fault evidence")
                .unwrap();
            let (destinations, verified): (i64, i64) = connection.query_row(
                "SELECT count(*),COALESCE(sum(verified_media),0) FROM provider_active_destinations WHERE child='asset-FAULT' AND admitted=1",
                [], |row| Ok((row.get(0)?,row.get(1)?)),
            ).unwrap();
            assert_eq!(
                destinations, 1,
                "current selected destination debt persists"
            );
            assert_eq!(verified, 0, "fsync failure must not certify publication");
            drop(connection);
            assert_eq!(expected.exists(), case == "reconcile_parent");
            if expected.exists() {
                assert_eq!(std::fs::read(&expected).unwrap(), bytes);
            }
            assert!(
                rows(&db, "owned_temp_files").is_empty(),
                "transfer claims retire after the failed attempt"
            );
            if cycle + 1 == recovery_cycle {
                std::fs::remove_file(root.join("fault-enabled")).unwrap();
            } else {
                // The next reopened cycle must reach the real syscall again.
                std::fs::remove_file(root.join("fault-reached")).unwrap();
            }
        } else {
            assert_eq!(std::fs::read(&expected).unwrap(), bytes);
            let verified: (String, String) = db.acquire_lock("verified selected publication")
                .unwrap().query_row(
                    "SELECT local_checksum,source_checksum FROM provider_active_destinations WHERE child='asset-FAULT' AND admitted=1 AND verified_media=1 ORDER BY generation LIMIT 1",
                    [], |row| Ok((row.get(0)?,row.get(1)?)),
                ).unwrap();
            assert_eq!(verified, (checksum.clone(), checksum.clone()));
            let receipts = (
                rows(&db, "assets"),
                rows(&db, "asset_metadata_paths"),
                rows(&db, "reconciliation_paths"),
                rows(&db, "owned_temp_files"),
            );
            if cycle == recovery_cycle {
                completed_files = Some(census(&media));
                completed_receipts = Some(receipts);
            } else {
                assert_eq!(census(&media), *completed_files.as_ref().unwrap());
                assert_eq!(receipts, *completed_receipts.as_ref().unwrap());
                assert_eq!(result.stats.downloaded, 0, "reopened publication is quiet");
                assert_eq!(
                    server.received_requests().await.unwrap().len(),
                    requests_before,
                    "quiet reopen performs no transfer"
                );
                assert_eq!(
                    lookups.load(Ordering::SeqCst),
                    lookups_before,
                    "quiet config avoids repeated current confirmation"
                );
            }
        }
        if case == "reconcile_parent" && cycle > 0 {
            assert_eq!(
                server.received_requests().await.unwrap().len(),
                requests_before,
                "landed exact bytes must retry durability without another transfer"
            );
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
