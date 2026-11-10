//! Goal-driven baseline contract with independently retained historical metadata debt.
//! A concurrent rank change requires a bounded second inventory: a delta alone cannot
//! recover an unchanged item skipped by rank shifts. This strengthens coverage; it
//! does not permit extra downloads or full inventory after the baseline completes.
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, Respond, ResponseTemplate};

use crate::commands::{AlbumPass, PassKind};
use crate::download;
use crate::icloud::photos::{PhotoLibrary, PhotosSession};
use crate::state::{DownloadStateStore, SqliteStateDb};
use crate::sync_cycle::{run_cycle, sync_token_key};
use crate::sync_loop::test_support::{
    album_count_response, full_album_page_with_download, make_run_cycle_config,
    make_run_cycle_download_config_builder, make_run_cycle_library_state_with_passes,
    make_shared_session_for_run_cycle,
};

const ZONE: &str = "SharedSync-v027-synthetic";
const OLD_TOKEN: &str = "old-version-token-must-never-be-sent";
const BOUNDARY: &str = "v27-before-inventory";
const MEDIA: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/data/media/pattern.jpg"
));

fn baseline_owner() -> crate::state::db::account::AccountOwner {
    crate::state::db::account::AccountOwner::authenticated(
        "baseline@example.invalid",
        "com",
        &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-baseline"}})).unwrap(),
    )
    .unwrap()
}

/// Remove the current epoch and return the database to schema 42 so the next
/// owned open runs the epoch migration again with historical state seeded.
fn clear_test_epoch(conn: &rusqlite::Connection) {
    conn.execute(
        "DELETE FROM metadata WHERE key=?1 \
         OR key LIKE 'sync_epoch_library:v1:%' \
         OR key LIKE 'source_revalidation_reset_pending:v1:%' \
         OR key LIKE 'source_revalidation_reset_proof:v1:%'",
        [crate::state::db::sync_epoch::SYNC_EPOCH_KEY],
    )
    .unwrap();
    conn.execute_batch("PRAGMA user_version=42").unwrap();
}

fn epoch_record(
    conn: &rusqlite::Connection,
    zone: &str,
) -> Option<crate::state::db::sync_epoch::SyncEpochLibrary> {
    crate::state::db::sync_epoch::read_library(conn, zone).unwrap()
}

fn force_epoch_due(conn: &rusqlite::Connection, zone: &str) {
    let key = crate::state::db::sync_epoch::library_key(zone);
    conn.execute(
        "UPDATE metadata SET value=json_set(value,'$.next_attempt_at',0) WHERE key=?1",
        [key],
    )
    .unwrap();
}

fn zone() -> Value {
    json!({"zoneName":ZONE,"ownerRecordName":"_defaultOwner"})
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
    if PRESERVE_SHARED_CLAIMS.try_get().unwrap_or(false) {
        for query in [
            "SELECT * FROM asset_master_mappings WHERE library='PrimarySync' ORDER BY asset_record_name",
            "SELECT * FROM asset_metadata_capture_revisions WHERE library='PrimarySync' ORDER BY asset_id",
        ] {
            let mut stmt = conn.prepare(query).unwrap();
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
    }
    rows
}

fn generation(n: u8) -> String {
    base64::engine::general_purpose::STANDARD.encode([n; 32])
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

#[derive(Clone)]
struct BaselineSession {
    database: &'static str,
    cycle: usize,
    inventory: Arc<Vec<Value>>,
    concurrent: Arc<Vec<Value>>,
    incremental_addition: Arc<Vec<Value>>,
    requests: Arc<Mutex<Vec<(String, Value)>>>,
    inventory_queries: Arc<AtomicUsize>,
    inventory_starts: Arc<AtomicUsize>,
    boundaries: Arc<AtomicUsize>,
    changes: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl PhotosSession for BaselineSession {
    async fn post(
        &self,
        url: &str,
        body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        let request: Value = serde_json::from_str(&body)?;
        assert!(
            !body.contains(OLD_TOKEN),
            "old-version cursor crossed provider boundary"
        );
        self.requests
            .lock()
            .unwrap()
            .push((url.into(), request.clone()));
        if url.contains("/zones/list") {
            assert!(
                url.contains(&format!("/{}", self.database)),
                "boundary must use the Photos library discovered database scope"
            );
            self.boundaries.fetch_add(1, Ordering::SeqCst);
            return Ok(json!({"zones":[{"zoneID":zone(),"syncToken":BOUNDARY,"deleted":false}]}));
        }
        if request["query"]["recordType"] == "CheckIndexingState" {
            assert_eq!(request["zoneID"], zone());
            return Ok(
                json!({"records":[{"recordName":"index-state","recordType":"CheckIndexingState","fields":{"state":{"value":"FINISHED"}}}]}),
            );
        }
        if url.contains("/internal/records/query/batch") {
            return Ok(album_count_response(
                if self.cycle > 0 && !self.incremental_addition.is_empty() {
                    4
                } else if self.changes.load(Ordering::SeqCst) > 0 {
                    3
                } else {
                    2
                },
            ));
        }
        if url.contains("/records/query?") {
            assert_eq!(request["zoneID"], zone());
            self.inventory_queries.fetch_add(1, Ordering::SeqCst);
            let offset = request["query"]["filterBy"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|f| f["fieldName"] == "startRank")
                .and_then(|f| f["fieldValue"]["value"].as_u64())
                .unwrap_or(0);
            // Rank mutation during inventory invalidates the first traversal. The
            // bounded second inventory sees C; it must reuse all completed files.
            let mut records = Vec::new();
            if offset == 0 {
                self.inventory_starts.fetch_add(1, Ordering::SeqCst);
                records.extend(self.inventory.iter().cloned());
                if self.changes.load(Ordering::SeqCst) > 0 {
                    records.extend(self.concurrent.iter().cloned());
                }
            }
            return Ok(
                json!({"records":records,"syncToken":"inventory-last-page-not-baseline-boundary"}),
            );
        }
        if url.contains("/changes/zone?") {
            assert_eq!(request["zones"][0]["zoneID"], zone());
            let bridge = self.changes.fetch_add(1, Ordering::SeqCst);
            let expected = if self.cycle == 0 && bridge == 0 {
                BOUNDARY.to_string()
            } else if self.cycle == 0 {
                assert_eq!(
                    bridge, 1,
                    "baseline may perform at most one immediate repeat"
                );
                "v27-after-baseline-change".to_string()
            } else {
                format!("v27-after-cycle-{}", self.cycle)
            };
            assert_eq!(
                request["zones"][0]["syncToken"], expected,
                "baseline must bridge its saved boundary; later cycles use fresh committed tokens"
            );
            let changing = self.cycle == 0 && bridge == 0;
            let successor = if changing {
                "v27-after-baseline-change".to_string()
            } else {
                format!("v27-after-cycle-{}", self.cycle + 1)
            };
            return Ok(json!({"zones":[{"zoneID":zone(),
                "records":if changing {self.concurrent.as_ref().clone()} else if self.cycle == 1 {self.incremental_addition.as_ref().clone()} else {Vec::new()},
                "syncToken":successor,"moreComing":false}]}));
        }
        if url.contains("/records/lookup?") {
            let requested = request["records"].as_array().unwrap();
            let records: Vec<_> = self
                .inventory
                .iter()
                .chain(self.concurrent.iter())
                .chain(self.incremental_addition.iter())
                .filter(|r| requested.iter().any(|q| q["recordName"] == r["recordName"]))
                .cloned()
                .collect();
            return Ok(json!({"records":records}));
        }
        anyhow::bail!("unexpected synthetic baseline endpoint: {url}");
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

#[tokio::test]
async fn v027_baseline_mixed_receipt_concurrent_delta_then_two_quiet_reopened_cycles() {
    for database in ["private", "shared"] {
        baseline_contract(database, false, false, false, false, false, false).await;
    }
}

tokio::task_local! {static CURRENT_SIDECAR_FAILURE: bool;}
tokio::task_local! {static REQUIRE_OBSERVATION_PROOF: bool;}
tokio::task_local! {static PRESERVE_EXTERNAL_LINKS: bool;}
tokio::task_local! {pub(super) static PRESERVE_SHARED_CLAIMS: bool;}
tokio::task_local! {static RESTORE_DELETED_RECEIPTS: bool;}

#[tokio::test]
async fn v027_restored_selected_receipt_survives_reopen_without_duplicate_or_rewrite() {
    RESTORE_DELETED_RECEIPTS
        .scope(
            true,
            baseline_contract("private", false, false, false, true, false, false),
        )
        .await;
}

#[tokio::test]
async fn v027_observation_proof_tracks_baseline_restart_and_quiet_deltas() {
    REQUIRE_OBSERVATION_PROOF
        .scope(
            true,
            baseline_contract("private", false, false, false, false, false, true),
        )
        .await;
}

#[tokio::test]
async fn v027_observation_proof_keeps_failed_media_outstanding_until_quiet_retry() {
    REQUIRE_OBSERVATION_PROOF
        .scope(
            true,
            baseline_contract("private", false, false, true, false, false, false),
        )
        .await;
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn v027_observation_proof_keeps_requested_metadata_outstanding() {
    REQUIRE_OBSERVATION_PROOF
        .scope(
            true,
            CURRENT_SIDECAR_FAILURE.scope(
                true,
                baseline_contract("private", true, false, false, false, false, false),
            ),
        )
        .await;
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn v027_current_requested_sidecar_retries_quietly_without_historical_backfill() {
    CURRENT_SIDECAR_FAILURE
        .scope(
            true,
            baseline_contract("private", true, false, false, false, false, false),
        )
        .await;
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn v027_baseline_preserves_queued_existing_metadata_while_new_files_get_sidecars() {
    for database in ["private", "shared"] {
        baseline_contract(database, true, false, false, false, false, false).await;
    }
}

#[cfg(all(unix, feature = "xmp"))]
#[tokio::test]
async fn v027_normal_sync_preserves_external_historical_media_and_sidecar_hardlinks() {
    for database in ["private", "shared"] {
        PRESERVE_EXTERNAL_LINKS
            .scope(
                true,
                baseline_contract(database, true, false, false, false, false, false),
            )
            .await;
    }
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn v027_normal_selected_sync_preserves_shared_historical_claims_and_debt() {
    for database in ["private", "shared"] {
        PRESERVE_SHARED_CLAIMS
            .scope(
                true,
                baseline_contract(database, true, false, false, false, false, false),
            )
            .await;
    }
}

#[tokio::test]
async fn v027_baseline_primary_template_change_preserves_existing_paths_and_downloads_missing() {
    for database in ["private", "shared"] {
        baseline_contract(database, false, true, false, false, false, false).await;
    }
}

#[tokio::test]
async fn v027_baseline_failed_transfer_survives_reopen_and_retries_with_quiet_provider() {
    for database in ["private", "shared"] {
        baseline_contract(database, false, false, true, false, false, false).await;
    }
}

#[tokio::test]
async fn v027_same_zone_current_receipt_template_change_does_not_reconcile_old_path() {
    for database in ["private", "shared"] {
        baseline_contract(database, false, true, false, true, false, false).await;
    }
}

fn current_content_receipts(path: &std::path::Path) -> Vec<Vec<rusqlite::types::Value>> {
    let conn = rusqlite::Connection::open(path).unwrap();
    let mut rows = Vec::new();
    for query in [
        "SELECT local_path,checksum,local_checksum,download_checksum FROM assets WHERE library=?1 AND id='asset-current-existing' ORDER BY version_size",
        "SELECT local_path,provider_checksum,local_checksum,source_checksum FROM asset_metadata_paths WHERE library=?1 AND id='asset-current-existing' ORDER BY version_size,local_path",
    ] {
        let mut stmt = conn.prepare(query).unwrap();
        rows.extend(
            stmt.query_map([ZONE], |row| {
                (0..4)
                    .map(|i| row.get(i))
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap(),
        );
    }
    rows
}

#[tokio::test]
async fn v027_invalid_prepared_operation_keeps_pending_config_after_cycle_and_reopen() {
    baseline_contract("private", false, true, false, true, true, false).await;
}

#[tokio::test]
async fn v027_bound_baseline_then_new_delta_asset_then_quiet_cycle_without_inventory() {
    for database in ["private", "shared"] {
        baseline_contract(database, false, false, false, false, false, true).await;
    }
}

async fn baseline_contract(
    database: &'static str,
    metadata_enabled: bool,
    primary_template_change: bool,
    initial_failure: bool,
    same_zone_current_receipt: bool,
    retained_operation: bool,
    add_after_baseline: bool,
) {
    // The default fixture's historical owner is another identity. It may
    // classify its own old path, but cannot authorize the current child to
    // reuse it. The same-zone control retains an exact current receipt.
    let foreign_existing = !same_zone_current_receipt;
    let server = crate::start_wiremock_or_skip!();
    let current_cycle = Arc::new(AtomicUsize::new(0));
    Mock::given(method("GET"))
        .and(path("/incremental-new"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(MEDIA)
                .insert_header("content-type", "image/jpeg"),
        )
        .expect(u64::from(add_after_baseline))
        .mount(&server)
        .await;
    let mut added_mtime = None;

    if initial_failure {
        let observed_cycle = Arc::clone(&current_cycle);
        Mock::given(method("GET"))
            .and(path("/missing"))
            .respond_with(move |_request: &wiremock::Request| {
                if observed_cycle.load(Ordering::SeqCst) == 0 {
                    ResponseTemplate::new(500)
                } else {
                    // The retained URL has expired by the next normal run.
                    ResponseTemplate::new(410)
                }
            })
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/missing-refreshed"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(MEDIA)
                    .insert_header("content-type", "image/jpeg"),
            )
            .expect(1)
            .mount(&server)
            .await;
    }
    for (endpoint, expected) in [
        ("/existing", usize::from(foreign_existing)),
        ("/missing", 1),
        ("/concurrent", 1),
    ] {
        if initial_failure && endpoint == "/missing" {
            continue;
        }
        Mock::given(method("GET"))
            .and(path(endpoint))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(MEDIA)
                    .insert_header("content-type", "image/jpeg"),
            )
            .expect(if retained_operation {
                0
            } else {
                expected as u64
            })
            .mount(&server)
            .await;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    std::fs::create_dir_all(&root).unwrap();
    let missing_root = if primary_template_change {
        root.join("requested-template")
    } else {
        root.clone()
    };
    let old_path = root.join("stable-renamed-legacy.JPG");
    std::fs::write(&old_path, MEDIA).unwrap();
    let old_mtime = std::fs::metadata(&old_path).unwrap().modified().unwrap();
    let old_sidecar = root.join("stable-renamed-legacy.JPG.xmp");
    let sidecar_bytes = b"<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF xmlns:rdf='http://www.w3.org/1999/02/22-rdf-syntax-ns#'/></x:xmpmeta>";
    let old_sidecar_mtime = if metadata_enabled {
        std::fs::write(&old_sidecar, sidecar_bytes).unwrap();
        Some(std::fs::metadata(&old_sidecar).unwrap().modified().unwrap())
    } else {
        None
    };
    let external_links = if PRESERVE_EXTERNAL_LINKS.try_get().unwrap_or(false) {
        let outside = dir.path().join("external-archive");
        std::fs::create_dir(&outside).unwrap();
        let media_alias = outside.join("preserved.JPG");
        let sidecar_alias = outside.join("preserved.xmp");
        std::fs::hard_link(&old_path, &media_alias).unwrap();
        std::fs::hard_link(&old_sidecar, &sidecar_alias).unwrap();
        Some((media_alias, sidecar_alias))
    } else {
        None
    };
    let db_path = dir.path().join("state.db");
    let token_key = sync_token_key(ZONE);
    {
        let db = SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap();
        let record = crate::test_helpers::TestAssetRecord::new("old-primary-master")
            .library("PrimarySync")
            .filename("existing.JPG")
            .checksum(&generation(1))
            .size(MEDIA.len() as u64)
            .build();
        db.upsert_seen(&record).await.unwrap();
        let sha = download::file::compute_sha256(&old_path).await.unwrap();
        db.mark_verified_download(
            "PrimarySync",
            "old-primary-master",
            "original",
            &old_path,
            &sha,
            Some(&sha),
            false,
        )
        .await
        .unwrap();
        if same_zone_current_receipt {
            let provider_records = pair(
                "current-existing",
                "existing.JPG",
                &format!("{}/existing", server.uri()),
                1,
            );
            let current = crate::icloud::photos::PhotoAsset::new(
                provider_records[0].clone(),
                provider_records[1].clone(),
            )
            .with_source_zone(Arc::from(ZONE))
            .with_state_record_name(Arc::from("asset-current-existing"));
            let exact = crate::test_helpers::TestAssetRecord::new("asset-current-existing")
                .library(ZONE)
                .filename("existing.JPG")
                .checksum(&generation(1))
                .size(MEDIA.len() as u64)
                .created_at(current.created())
                .added_at(current.added_date())
                .metadata((*current.metadata_arc(crate::state::VersionSizeKey::Original)).clone())
                .build();
            db.upsert_seen(&exact).await.unwrap();
            db.upsert_asset_master_mapping(ZONE, "asset-current-existing", "current-existing")
                .await
                .unwrap();
            db.mark_verified_download(
                ZONE,
                "asset-current-existing",
                "original",
                &old_path,
                &sha,
                Some(&sha),
                false,
            )
            .await
            .unwrap();
        }
        if metadata_enabled {
            // Existing durable retry API: a previous sidecar/embed write failed
            // after verified media publication. The normal cycle must retain
            // this work without attempting it or converting it to a new failure.
            db.record_metadata_write_failure("PrimarySync", "old-primary-master", "original")
                .await
                .unwrap();
            assert!(db.get_metadata_retry_markers().await.unwrap().contains(&(
                "PrimarySync".into(),
                "old-primary-master".into(),
                "original".into()
            )));
        }
        // The valid content receipt remains useful while its historical master
        // attribution is ambiguous. Normal v27 progress must retain this old work
        // without turning metadata repair into a cursor prerequisite.
        db.set_metadata_capture_revision_for_test("PrimarySync", "old-primary-master", 0);
        for child in ["historical-child-a", "historical-child-b"] {
            db.upsert_asset_master_mapping("PrimarySync", child, "old-primary-master")
                .await
                .unwrap();
            if PRESERVE_SHARED_CLAIMS.try_get().unwrap_or(false) {
                db.acquire_lock("seed ambiguous historical shared-path claim").unwrap().execute(
                    "INSERT INTO asset_metadata_paths(library,id,version_size,local_path,provider_checksum,local_checksum) VALUES('PrimarySync',?1,'original',?2,?3,'historical-local')",
                    rusqlite::params![child,old_path.to_str().unwrap(),generation(1)],
                ).unwrap();
            }
        }
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
        db.set_metadata(&format!("sync_token:{ZONE}"), OLD_TOKEN)
            .await
            .unwrap();
        db.set_metadata(&format!("pending_sync_token:old-config:{ZONE}"), OLD_TOKEN)
            .await
            .unwrap();
        if RESTORE_DELETED_RECEIPTS.try_get().unwrap_or(false) {
            db.mark_soft_deleted("PrimarySync", "old-primary-master", None)
                .await
                .unwrap();
            db.mark_soft_deleted(ZONE, "asset-current-existing", None)
                .await
                .unwrap();
            assert_eq!(db.acquire_lock("prove deleted restoration seed").unwrap().query_row::<i64,_,_>(
                "SELECT count(*) FROM assets WHERE is_deleted=1 AND id IN ('old-primary-master','asset-current-existing')",[],|r|r.get(0)).unwrap(),2);
        }
    }
    if PRESERVE_SHARED_CLAIMS.try_get().unwrap_or(false) {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        assert_eq!(conn.query_row::<i64,_,_>("SELECT count(*) FROM asset_metadata_paths WHERE local_checksum='historical-local' AND local_path=?1",[old_path.to_str().unwrap()],|r|r.get(0)).unwrap(), 2);
    }
    let original_rows = historical_rows(&db_path);
    let original_current_receipts = if same_zone_current_receipt {
        let receipts = current_content_receipts(&db_path);
        assert_eq!(
            receipts.len(),
            2,
            "valid current asset and exact path receipt must both be seeded"
        );
        Some(receipts)
    } else {
        None
    };
    let mut records = pair(
        "current-existing",
        "existing.JPG",
        &format!("{}/existing", server.uri()),
        1,
    );
    records.extend(pair(
        "current-missing",
        "missing.JPG",
        &format!("{}/missing", server.uri()),
        2,
    ));
    let concurrent = pair(
        "during-baseline",
        "concurrent.JPG",
        &format!("{}/concurrent", server.uri()),
        3,
    );
    let requests = Arc::new(Mutex::new(Vec::new()));
    let config = make_run_cycle_config();
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let trace = CURRENT_SIDECAR_FAILURE
        .try_with(|value| *value)
        .unwrap_or(false)
        .then(crate::test_helpers::TracingCapture::install);
    let current_sidecar_failure = CURRENT_SIDECAR_FAILURE
        .try_with(|value| *value)
        .unwrap_or(false);
    let mut successful_sidecars = std::collections::BTreeMap::new();
    let mut metadata_retry_state = None;
    let mut captured_metadata = None;
    for cycle in 0..if retained_operation {
        1
    } else if current_sidecar_failure || recorded_only::enabled() {
        4
    } else {
        3
    } {
        if current_sidecar_failure {
            let blocked = root.join("missing.JPG.xmp");
            if cycle == 0 {
                std::fs::create_dir(&blocked).unwrap();
            }
            if cycle == 2 {
                std::fs::remove_dir(&blocked).unwrap();
            }
        }
        current_cycle.store(cycle, Ordering::SeqCst);
        if (initial_failure && cycle == 1) || (current_sidecar_failure && cycle == 2) {
            // Simulate the retry deadline becoming due without sleeping or
            // altering admission, receipts, source evidence or destinations.
            SqliteStateDb::open_owned(&db_path, &baseline_owner())
                .await
                .unwrap()
                .make_selection_retry_due(baseline_owner(), "asset-current-missing".into())
                .await
                .unwrap();
        }
        let db = Arc::new(
            SqliteStateDb::open_owned(&db_path, &baseline_owner())
                .await
                .unwrap(),
        );
        if current_sidecar_failure && cycle == 2 {
            use crate::state::db::MetadataRewriteStore;
            let generation:String=db.acquire_lock("capture due metadata round").unwrap().query_row("SELECT generation FROM provider_active_destinations WHERE child='asset-current-missing' AND verified_metadata=0",[],|r|r.get(0)).unwrap();
            let rows = db
                .pending_selection_metadata(baseline_owner(), generation.clone(), 10)
                .await
                .unwrap();
            assert_eq!(rows.len(), 1);
            db.validate_metadata_writer(&rows[0]).await.unwrap();
            let mut stale = rows[0].clone();
            stale.asset.created_at += chrono::Duration::seconds(1);
            assert!(
                db.validate_metadata_writer(&stale).await.is_err(),
                "stale captured writer cannot write even a sidecar"
            );
            captured_metadata = Some((generation, rows));
        }
        let cycle_records = if initial_failure && cycle > 0 {
            let mut current = pair(
                "current-existing",
                "existing.JPG",
                &format!("{}/existing", server.uri()),
                1,
            );
            current.extend(pair(
                "current-missing",
                "missing.JPG",
                &format!("{}/missing-refreshed", server.uri()),
                2,
            ));
            current
        } else {
            records.clone()
        };
        let session = BaselineSession {
            database,
            cycle,
            inventory: Arc::new(cycle_records),
            concurrent: Arc::new(concurrent.clone()),
            incremental_addition: Arc::new(if add_after_baseline {
                pair(
                    "incremental-new",
                    "incremental-new.JPG",
                    &format!("{}/incremental-new", server.uri()),
                    4,
                )
            } else {
                Vec::new()
            }),
            requests: requests.clone(),
            inventory_queries: Arc::new(AtomicUsize::new(0)),
            inventory_starts: Arc::new(AtomicUsize::new(0)),
            boundaries: Arc::new(AtomicUsize::new(0)),
            changes: Arc::new(AtomicUsize::new(0)),
        };
        let mut library = PhotoLibrary::new(
            format!("https://synthetic.example/database/1/photos/production/{database}"),
            Arc::new(std::collections::HashMap::new()),
            Box::new(session.clone()),
            Arc::new(zone()),
            database.into(),
            crate::retry::RetryConfig::default(),
        )
        .await
        .unwrap();
        library.shadow_capture = Some(crate::icloud::photos::inbox::ShadowCapture::new(
            db.clone(),
            baseline_owner(),
            "com",
        ));
        let passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: library.all(),
            exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
        }];
        let mut selected = make_run_cycle_library_state_with_passes(ZONE, &token_key, passes);
        selected.library = library;
        if cycle == 0 {
            recorded_only::seed(&selected.library, db.as_ref()).await;
            raw_replay::seed(&selected.library, db.as_ref()).await;
        }
        let base_builder = make_run_cycle_download_config_builder(&root, db.clone());
        if primary_template_change && cycle == 0 {
            // Persist the genuine prior path configuration through the existing
            // metadata API. The first normal v27 run observes a real change,
            // without inventing an interrupted journal or touching old media.
            let mut previous = (*base_builder(
                download::SyncMode::Full,
                Arc::new(rustc_hash::FxHashSet::default()),
                Arc::new(download::AssetGroupings::default()),
                Arc::from(ZONE),
            ))
            .clone();
            previous.folder_structure = "none".into();
            previous.edited_naming = crate::types::EditedNaming::Suffix;
            #[cfg(feature = "xmp")]
            {
                previous.metadata.xmp_sidecar = false;
            }
            db.set_metadata(
                download::DOWNLOAD_CONFIG_HASH_KEY,
                &download::hash_download_config(&previous),
            )
            .await
            .unwrap();
        }
        let retained = if retained_operation {
            let retained_root = root.parent().unwrap().join("retained-layout-root");
            std::fs::create_dir_all(&retained_root).unwrap();
            let operation = download::prepared_layout_with_refresh(
                &db,
                &retained_root,
                "retained-independent.JPG",
                ZONE,
                true,
            )
            .await;
            assert!(operation.refresh_metadata);
            let before = serde_json::to_vec(&operation).unwrap();
            let stage = operation.members[0].stage.as_ref().unwrap().to_path();
            // Valid prepared journals now finish from their recorded proof.
            // This is genuinely unresolved work: the stage changed after its
            // durable fingerprint, so config completion must remain blocked.
            std::fs::write(&stage, b"changed after recorded preparation").unwrap();
            let bytes = std::fs::read(&stage).unwrap();
            let previous_hash = db
                .get_metadata(download::DOWNLOAD_CONFIG_HASH_KEY)
                .await
                .unwrap();
            Some((operation.operation, before, stage, bytes, previous_hash))
        } else {
            None
        };
        let builder = |mode, exclude, groupings, zone| {
            let mut configured = (*base_builder(mode, exclude, groupings, zone)).clone();
            configured.folder_structure = if primary_template_change {
                "requested-template"
            } else {
                "none"
            }
            .into();
            if primary_template_change {
                configured.edited_naming = crate::types::EditedNaming::Primary;
            }
            if recorded_only::enabled() && cycle >= 2 {
                configured.folder_structure = "changed-template".into();
            }
            #[cfg(feature = "xmp")]
            {
                configured.metadata.xmp_sidecar = metadata_enabled;
            }
            if initial_failure {
                configured.retry.max_retries = 0;
            }
            Arc::new(configured)
        };
        raw_replay::startup(&selected.library).await;
        let result = run_cycle(
            &[&selected],
            &config,
            Some(db.as_ref()),
            false,
            &builder,
            download::DownloadControls::download_hidden(),
            &shared,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        if let Some((operation_id, before, stage, bytes, previous_hash)) = &retained {
            for snapshot in [
                SqliteStateDb::open_owned(&db_path, &baseline_owner())
                    .await
                    .unwrap(),
                SqliteStateDb::open_owned(&db_path, &baseline_owner())
                    .await
                    .unwrap(),
            ] {
                assert_eq!(
                    snapshot
                        .get_metadata(download::DOWNLOAD_CONFIG_HASH_KEY)
                        .await
                        .unwrap(),
                    *previous_hash,
                    "retained operation must not certify new config"
                );
                assert!(
                    snapshot
                        .get_metadata(crate::sync_cycle::PENDING_DOWNLOAD_CONFIG_HASH_KEY)
                        .await
                        .unwrap()
                        .is_some(),
                    "pending config must survive cycle and SQLite reopen"
                );
                assert!(
                    snapshot.get_metadata(&token_key).await.unwrap().is_none(),
                    "invalid recorded work must not establish a fresh completion token"
                );
                assert_eq!(
                    snapshot
                        .get_metadata(&format!("sync_token:{ZONE}"))
                        .await
                        .unwrap()
                        .as_deref(),
                    Some(OLD_TOKEN),
                    "the historical token remains untouched"
                );
                let operations = snapshot
                    .primary_layout_operations(ZONE.into())
                    .await
                    .unwrap();
                let operation = operations
                    .iter()
                    .find(|op| &op.operation == operation_id)
                    .unwrap();
                assert_eq!(serde_json::to_vec(operation).unwrap(), *before);
                assert_eq!(std::fs::read(stage).unwrap(), *bytes);
            }
        }
        recorded_only::assert_unadmitted(db.as_ref());
        if let Some((media_alias, sidecar_alias)) = &external_links {
            assert_eq!(std::fs::read(media_alias).unwrap(), MEDIA);
            assert_eq!(std::fs::read(sidecar_alias).unwrap(), sidecar_bytes);
            assert_eq!(
                std::fs::metadata(media_alias).unwrap().modified().unwrap(),
                old_mtime
            );
            assert_eq!(
                std::fs::metadata(sidecar_alias)
                    .unwrap()
                    .modified()
                    .unwrap(),
                old_sidecar_mtime.unwrap()
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                assert_eq!(
                    std::fs::metadata(media_alias).unwrap().ino(),
                    std::fs::metadata(&old_path).unwrap().ino()
                );
                assert_eq!(
                    std::fs::metadata(sidecar_alias).unwrap().ino(),
                    std::fs::metadata(&old_sidecar).unwrap().ino()
                );
            }
        }
        assert_eq!(std::fs::read(&old_path).unwrap(), MEDIA);
        assert_eq!(
            std::fs::metadata(&old_path).unwrap().modified().unwrap(),
            old_mtime
        );
        assert_eq!(
            historical_rows(&db_path),
            original_rows,
            "historical records and metadata must remain unchanged"
        );
        if metadata_enabled {
            assert_eq!(std::fs::read(&old_sidecar).unwrap(), sidecar_bytes);
            assert_eq!(
                std::fs::metadata(&old_sidecar).unwrap().modified().unwrap(),
                old_sidecar_mtime.unwrap()
            );
            assert!(
                db.get_metadata_retry_markers().await.unwrap().contains(&(
                    "PrimarySync".into(),
                    "old-primary-master".into(),
                    "original".into()
                )),
                "historical rewrite marker must remain pending"
            );
            if current_sidecar_failure && cycle == 0 {
                assert!(
                    result.stats.exif_failures > 0,
                    "current requested write failure stays visible"
                );
            } else {
                assert_eq!(
                    result.stats.exif_failures,
                    0,
                    "historical metadata work must remain untouched: {result:?}, trace={:?}",
                    trace.as_ref().map(|value| value.0.events())
                );
            }
            for name in ["missing.JPG.xmp", "concurrent.JPG.xmp"] {
                if current_sidecar_failure && cycle < 2 && name == "missing.JPG.xmp" {
                    continue;
                }
                let output = std::fs::read_to_string(root.join(name)).unwrap();
                if current_sidecar_failure {
                    let observed = (
                        output.clone(),
                        std::fs::metadata(root.join(name))
                            .unwrap()
                            .modified()
                            .unwrap(),
                    );
                    if let Some(prior) = successful_sidecars.get(name) {
                        assert_eq!(prior, &observed, "successful sidecar must not be rewritten");
                    } else {
                        successful_sidecars.insert(name, observed);
                    }
                }
                assert!(
                    output.contains("xmpmeta") && output.contains("rdf:RDF"),
                    "new download must publish an XMP packet: {name}"
                );
            }
        }
        if current_sidecar_failure {
            let work = db.recorded_selection_work().await.unwrap();
            assert_eq!(
                work.invalid_roots, 0,
                "valid current roots must remain reportable"
            );
            assert_eq!(
                work.profiles
                    .iter()
                    .map(|profile| profile.pending_media)
                    .sum::<u64>(),
                0
            );
            assert_eq!(
                work.profiles
                    .iter()
                    .map(|profile| profile.pending_metadata)
                    .sum::<u64>(),
                u64::from(cycle < 2)
            );
            assert!(
                work.stored_media_receipts > 0,
                "retired receipt evidence stays visible separately"
            );
            let conn = db
                .acquire_lock("current metadata debt and retirement")
                .unwrap();
            let debt:i64=conn.query_row("SELECT count(*) FROM provider_active_destinations WHERE child='asset-current-missing' AND verified_media=1 AND verified_metadata=0",[],|r|r.get(0)).unwrap();
            if cycle < 2 {
                assert_eq!(debt, 1, "failed metadata remains durable during backoff");
                let retry:(i64,i64)=conn.query_row("SELECT attempts,next_retry_at FROM provider_active_decisions WHERE child='asset-current-missing' AND attempts>0",[],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
                assert!(retry.1 > chrono::Utc::now().timestamp());
                if let Some(prior) = metadata_retry_state {
                    assert_eq!(retry, prior, "quiet backoff must not attempt again");
                }
                metadata_retry_state = Some(retry);
            } else {
                assert_eq!(debt, 0);
                let retained:i64=conn.query_row("SELECT count(*) FROM provider_active_destinations WHERE verified_metadata=1 AND (frozen_metadata IS NOT NULL OR prepared_hash IS NOT NULL)",[],|r|r.get(0)).unwrap();
                assert_eq!(retained, 0, "finished snapshots must retire safely");
            }
        }
        if current_sidecar_failure && cycle == 2 {
            let (generation, rows) = captured_metadata.take().unwrap();
            let before:(i64,i64)=db.acquire_lock("completed decision deadline").unwrap().query_row("SELECT attempts,next_retry_at FROM provider_active_decisions WHERE generation=?1 AND child='asset-current-missing'",[&generation],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
            assert!(
                db.defer_selection_metadata_round(baseline_owner(), generation.clone(), rows)
                    .await
                    .unwrap()
                    .is_empty(),
                "completed captured owner must be skipped"
            );
            let after:(i64,i64)=db.acquire_lock("unchanged completed decision deadline").unwrap().query_row("SELECT attempts,next_retry_at FROM provider_active_decisions WHERE generation=?1 AND child='asset-current-missing'",[&generation],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
            assert_eq!(
                before, after,
                "completed work must not acquire a new attempt or deadline"
            );
        }
        if primary_template_change {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            for table in ["primary_layout_bindings", "primary_layout_claims"] {
                let count: i64 = conn.query_row(&format!(
                    "SELECT count(*) FROM {table} WHERE child IN ('old-primary-master','asset-current-existing')"
                ), [], |row| row.get(0)).unwrap();
                assert_eq!(
                    count, 0,
                    "normal sync must not establish a layout handover for historical media: {table}"
                );
            }
            let operations: i64 = conn.query_row(
                "SELECT count(*) FROM primary_layout_operations WHERE json_extract(CAST(header AS TEXT),'$.child') IN ('old-primary-master','asset-current-existing')",
                [], |row| row.get(0)).unwrap();
            assert_eq!(
                operations, 0,
                "normal sync must not create even a cancelled historical layout journal"
            );
            if foreign_existing {
                assert_eq!(
                    std::fs::read(missing_root.join("existing.JPG")).unwrap(),
                    MEDIA,
                    "foreign historical identity must materialize its own path"
                );
            } else {
                assert!(
                    !missing_root.join("existing.JPG").exists(),
                    "template changes must retain the verified historical path"
                );
            }
        }
        if let Some(expected) = &original_current_receipts {
            assert!(
                current_content_receipts(&db_path) == *expected,
                "same-zone configuration drift must keep current path and content receipts unchanged"
            );
        }
        if RESTORE_DELETED_RECEIPTS.try_get().unwrap_or(false) {
            let current: (bool, String, String, String) = db.acquire_lock("current restoration result").unwrap().query_row(
                "SELECT is_deleted,status,local_path,local_checksum FROM assets WHERE library=?1 AND id='asset-current-existing' AND version_size='original'",[ZONE],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap();
            assert!(!current.0);
            assert_eq!(current.1, "downloaded");
            assert_eq!(std::path::Path::new(&current.2), old_path);
            assert_eq!(
                current.3,
                download::file::compute_sha256(&old_path).await.unwrap()
            );
            assert!(
                !root.join("existing.JPG").exists(),
                "restoration cannot create a second path"
            );
            assert!(db.acquire_lock("historical source stays deleted").unwrap().query_row::<bool,_,_>(
                "SELECT is_deleted FROM assets WHERE library='PrimarySync' AND id='old-primary-master'",[],|r|r.get(0)).unwrap());
        }
        let old_rows = db.get_downloaded_page(0, 100).await.unwrap();
        assert_eq!(
            old_rows
                .iter()
                .find(|r| r.library.as_ref() == "PrimarySync")
                .unwrap()
                .local_path
                .as_ref(),
            Some(&old_path)
        );
        if retained_operation {
            // Corrupt, already-recorded publication work is an integrity
            // failure for this library, not an archive-layout prerequisite.
            // Recovery cannot certify it or safely start another publication.
            assert_eq!(result.failed_count, 1, "{result:?}");
            assert!(!result.db_sync_token_advance_safe, "{result:?}");
            assert_eq!(result.stats.downloaded, 0);
            assert_eq!(session.inventory_queries.load(Ordering::SeqCst), 0);
            assert_eq!(session.changes.load(Ordering::SeqCst), 0);
            for name in ["existing.JPG", "missing.JPG", "concurrent.JPG"] {
                assert!(!missing_root.join(name).exists());
            }
            continue;
        }
        if cycle == 0 {
            assert_eq!(
                session.boundaries.load(Ordering::SeqCst),
                1,
                "first v27 cycle must acquire a fresh scoped zone boundary before inventory: {result:?}"
            );
            assert_eq!(
                session.inventory_starts.load(Ordering::SeqCst),
                2,
                "a raw rank change requires a second inventory followed by a quiet bridge: {result:?}"
            );
            let captured = requests.lock().unwrap();
            let boundary_position = captured
                .iter()
                .position(|(url, _)| url.contains("/zones/list"))
                .unwrap();
            let inventory_position = captured
                .iter()
                .position(|(url, body)| {
                    url.contains("/records/query?")
                        && body["query"]["recordType"] != "CheckIndexingState"
                })
                .unwrap();
            assert!(
                boundary_position < inventory_position,
                "boundary must precede first inventory request"
            );
            drop(captured);

            assert_eq!(
                result.stats.downloaded,
                (if initial_failure { 1 } else { 2 }) + usize::from(foreign_existing),
                "foreign existing media plus genuinely missing inventory+concurrent content"
            );
        } else {
            if recorded_only::enabled() && cycle == 2 {
                assert_eq!(
                    session.inventory_starts.load(Ordering::SeqCst),
                    1,
                    "new selected path policy requires one complete comparison"
                );
            } else {
                assert_eq!(
                    session.inventory_queries.load(Ordering::SeqCst),
                    0,
                    "quiet reopened cycles must remain incremental"
                );
            }
            assert_eq!(
                result.stats.downloaded,
                usize::from((initial_failure || add_after_baseline) && cycle == 1)
            );
        }
        assert_eq!(
            session.changes.load(Ordering::SeqCst),
            if cycle == 0 { 2 } else { 1 }
        );
        if (initial_failure || current_sidecar_failure) && cycle == 0 {
            assert!(
                result.failed_count > 0,
                "failed transfer must remain visible: {result:?}"
            );
        } else {
            assert_eq!(result.failed_count, 0, "{result:?}");
            assert!(result.db_sync_token_advance_safe, "{result:?}");
        }
        assert_eq!(
            db.get_metadata(&token_key).await.unwrap().as_deref(),
            Some(format!("v27-after-cycle-{}", cycle + 1).as_str())
        );
        if REQUIRE_OBSERVATION_PROOF.try_with(|v| *v).unwrap_or(false) || recorded_only::enabled() {
            let encoded = db
                .get_metadata(&format!("observed_selection:v27:{ZONE}"))
                .await
                .unwrap()
                .expect("a completed selected observation must commit its proof with its token");
            let proof: Value = serde_json::from_str(&encoded).unwrap();
            assert_eq!(proof["version"], 1);
            let observed = db.observed_selection_status().await.unwrap();
            assert_eq!(observed.len(), 1);
            assert!(!observed[0].settlement_unknown);
            assert_eq!(
                observed[0].pending_media,
                initial_failure && !current_sidecar_failure && cycle == 0,
                "media failure is outstanding on cycle zero and settled by quiet retry on cycle one"
            );
            assert_eq!(
                observed[0].pending_metadata,
                current_sidecar_failure && cycle < 2
            );

            assert!(
                !encoded.contains("v27-after-cycle"),
                "proof must bind the opaque cursor without rendering it"
            );
        }
        assert_eq!(
            db.get_metadata(&format!("sync_token:{ZONE}"))
                .await
                .unwrap()
                .as_deref(),
            Some(OLD_TOKEN)
        );
        for id in [
            "asset-current-existing",
            "asset-current-missing",
            "asset-during-baseline",
        ] {
            if initial_failure && cycle == 0 && id == "asset-current-missing" {
                continue;
            }
            assert!(
                old_rows
                    .iter()
                    .any(|r| r.library.as_ref() == ZONE && r.id.as_ref() == id),
                "distinct current identity missing: {id}"
            );
        }
        if primary_template_change {
            assert!(
                !root.join("existing.JPG").exists(),
                "the changed template owns the new path"
            );
        } else if foreign_existing {
            assert_eq!(
                std::fs::read(root.join("existing.JPG")).unwrap(),
                MEDIA,
                "foreign historical identity must materialize its own path"
            );
        } else {
            assert!(
                !root.join("existing.JPG").exists(),
                "exact current receipt must not acquire a duplicate"
            );
        }
        if initial_failure && cycle == 0 {
            assert_eq!(
                result.stats.state_write_failures, 0,
                "the injected media failure must not be confused with a state-write failure"
            );
            assert!(!missing_root.join("missing.JPG").exists());
        } else {
            assert_eq!(
                std::fs::read(missing_root.join("missing.JPG")).unwrap(),
                MEDIA
            );
        }
        assert_eq!(
            std::fs::read(missing_root.join("concurrent.JPG")).unwrap(),
            MEDIA
        );
        assert_eq!(
            std::fs::read_dir(&root).unwrap().count(),
            if primary_template_change {
                2
            } else if metadata_enabled {
                6 + 2 * usize::from(foreign_existing)
            } else if initial_failure && cycle == 0 {
                2 + usize::from(foreign_existing)
            } else {
                3 + usize::from(add_after_baseline && cycle > 0) + usize::from(foreign_existing)
            }
        );
        if add_after_baseline {
            let conn = db
                .acquire_lock("prove bound format2 incremental ledger")
                .unwrap();
            let roots: i64 = conn.query_row(
                "SELECT count(*) FROM provider_active_generations WHERE json_extract(CAST(specification AS TEXT),'$.format')=2",
                [], |row| row.get(0)).unwrap();
            assert!(
                roots > 0,
                "bound fixture must exercise format2, not fallback"
            );
            for table in [
                "provider_shadow_pages",
                "provider_catalog_pages",
                "provider_selection_rank_pages",
            ] {
                assert_eq!(
                    conn.query_row::<i64, _, _>(
                        &format!("SELECT count(*) FROM {table}"),
                        [],
                        |row| row.get(0)
                    )
                    .unwrap(),
                    0,
                    "new work must not require {table}"
                );
            }
            if cycle > 0 {
                let verified: i64 = conn.query_row(
                    "SELECT count(*) FROM provider_active_destinations d JOIN provider_active_generations g ON g.id=d.generation WHERE d.child='asset-incremental-new' AND d.admitted=1 AND d.verified_media=1 AND json_extract(CAST(g.specification AS TEXT),'$.format')=2",
                    [], |row| row.get(0)).unwrap();
                assert!(
                    verified > 0,
                    "incremental addition must enter and complete the same ledger"
                );
            }
            drop(conn);
            let added = missing_root.join("incremental-new.JPG");
            if cycle == 0 {
                assert!(!added.exists());
            } else {
                assert_eq!(std::fs::read(&added).unwrap(), MEDIA);
                let mtime = std::fs::metadata(&added).unwrap().modified().unwrap();
                if let Some(previous) = added_mtime {
                    assert_eq!(mtime, previous);
                }
                added_mtime = Some(mtime);
                assert!(old_rows.iter().any(|row| row.library.as_ref() == ZONE
                    && row.id.as_ref() == "asset-incremental-new"
                    && row.local_path.as_ref() == Some(&added)));
            }
        }
        let pending = db.get_pending().await.unwrap();
        let failed = db.get_failed().await.unwrap();
        if initial_failure && cycle == 0 {
            // A quiet bridge resets failed asset rows to pending. The age-gated
            // legacy cleanup may promote them again within the same second.
            // Neither status is the independent ledger's retry authority.
            let unresolved: Vec<_> = pending.iter().chain(&failed).collect();
            assert_eq!(unresolved.len(), 1);
            assert_eq!(unresolved[0].id.as_ref(), "asset-current-missing");
            assert_eq!(unresolved[0].library.as_ref(), ZONE);
            let retained: (i64, i64, String, i64) = db
                .acquire_lock("assert durable failed transfer retry")
                .unwrap()
                .query_row(
                    "SELECT w.admitted,w.verified_media,d.reason,d.next_retry_at
                     FROM provider_active_decisions d
                     JOIN provider_active_destinations w ON w.generation=d.generation
                       AND w.pass_key=d.pass_key AND w.child=d.child
                     WHERE d.child='asset-current-missing'",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .unwrap();
            assert_eq!(retained.0, 1, "failed transfer stays admitted");
            assert_eq!(retained.1, 0, "failed transfer stays unverified");
            assert_eq!(retained.2, "download_failed", "truthful media reason");
            assert!(
                retained.3 > chrono::Utc::now().timestamp(),
                "actual transfer failure carries a positive item deadline"
            );
        } else {
            assert!(pending.is_empty());
            assert!(failed.is_empty());
        }
    }
    assert!(
        requests
            .lock()
            .unwrap()
            .iter()
            .all(|(_, b)| !b.to_string().contains(OLD_TOKEN))
    );
    if recorded_only::enabled() {
        assert!(
            requests
                .lock()
                .unwrap()
                .iter()
                .all(|(url, body)| !url.contains("/records/lookup")
                    || !body.to_string().contains("old-only-unadmitted")),
            "legacy catalog alone must never cause current lookup"
        );
    }
    server.verify().await;
}

#[cfg(target_os = "linux")]
mod process_death;

#[tokio::test]
async fn v027_bound_baseline_unaccountable_inventory_keeps_cursor_unset() {
    #[derive(Clone)]
    struct InvalidInventory {
        base: BaselineSession,
        record: Value,
    }
    #[async_trait::async_trait]
    impl PhotosSession for InvalidInventory {
        async fn post(
            &self,
            url: &str,
            body: String,
            headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            let request: Value = serde_json::from_str(&body)?;
            if url.contains("/records/query?")
                && request["query"]["recordType"] != "CheckIndexingState"
            {
                return Ok(json!({"records":[self.record.clone()],"syncToken":"must-not-save"}));
            }
            self.base.post(url, body, headers).await
        }
        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(self.clone())
        }
    }
    #[derive(Clone)]
    struct ValidEmpty(BaselineSession);
    #[async_trait::async_trait]
    impl PhotosSession for ValidEmpty {
        async fn post(
            &self,
            url: &str,
            body: String,
            headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            if url.contains("/internal/records/query/batch") {
                return Ok(album_count_response(0));
            }
            self.0.post(url, body, headers).await
        }
        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(self.clone())
        }
    }
    for record in [
        json!({"recordName":"future-current","recordType":"FuturePhotoRecord","fields":{}}),
        json!({"recordName":"asset-without-master","recordType":"CPLAsset","fields":{}}),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        std::fs::create_dir(&media).unwrap();
        let old = media.join("old-untouched.JPG");
        std::fs::write(&old, MEDIA).unwrap();
        let mtime = std::fs::metadata(&old).unwrap().modified().unwrap();
        let db = Arc::new(
            SqliteStateDb::open_owned(&dir.path().join("state.db"), &baseline_owner())
                .await
                .unwrap(),
        );
        let existing = crate::test_helpers::TestAssetRecord::new("existing-master")
            .library(ZONE)
            .version_size(crate::state::VersionSizeKey::Original)
            .filename("old-untouched.JPG")
            .build();
        db.upsert_seen(&existing).await.unwrap();
        db.mark_downloaded(
            ZONE,
            "existing-master",
            "original",
            &old,
            "synthetic-local",
            None,
        )
        .await
        .unwrap();
        db.upsert_asset_master_mapping(ZONE, "existing-child", "existing-master")
            .await
            .unwrap();
        assert!(
            db.claim_legacy_master_state_owner(ZONE, "existing-master", "existing-child")
                .await
                .unwrap()
        );
        db.set_metadata(&format!("sync_token:{ZONE}"), OLD_TOKEN)
            .await
            .unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let provider = InvalidInventory {
            record,
            base: BaselineSession {
                database: "private",
                cycle: 0,
                inventory: Arc::new(Vec::new()),
                concurrent: Arc::new(Vec::new()),
                incremental_addition: Arc::new(Vec::new()),
                requests: Arc::clone(&requests),
                inventory_queries: Arc::new(AtomicUsize::new(0)),
                inventory_starts: Arc::new(AtomicUsize::new(0)),
                boundaries: Arc::new(AtomicUsize::new(0)),
                changes: Arc::new(AtomicUsize::new(0)),
            },
        };
        let mut library = PhotoLibrary::new(
            "https://synthetic.example/database/1/photos/production/private".into(),
            Arc::new(std::collections::HashMap::new()),
            Box::new(provider),
            Arc::new(zone()),
            "private".into(),
            crate::retry::RetryConfig::default(),
        )
        .await
        .unwrap();
        library.shadow_capture = Some(crate::icloud::photos::inbox::ShadowCapture::new(
            db.clone(),
            baseline_owner(),
            "com",
        ));
        let key = sync_token_key(ZONE);
        let passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: library.all(),
            exclude_ids: Default::default(),
        }];
        let mut selected = make_run_cycle_library_state_with_passes(ZONE, &key, passes);
        selected.library = library;
        let builder = make_run_cycle_download_config_builder(&media, db.clone());
        let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
        let result = run_cycle(
            &[&selected],
            &make_run_cycle_config(),
            Some(db.as_ref()),
            false,
            &builder,
            download::DownloadControls::download_hidden(),
            &shared,
            &CancellationToken::new(),
        )
        .await;
        assert!(
            result.as_ref().map_or(true, |r| r.failed_count > 0
                || !r.db_sync_token_advance_safe),
            "unaccountable inventory must visibly fail: {result:?}"
        );
        let reopened = SqliteStateDb::open_owned(&dir.path().join("state.db"), &baseline_owner())
            .await
            .unwrap();
        assert!(reopened.get_metadata(&key).await.unwrap().is_none());
        assert_eq!(
            reopened
                .get_metadata(&format!("sync_token:{ZONE}"))
                .await
                .unwrap()
                .as_deref(),
            Some(OLD_TOKEN)
        );
        assert!(
            requests
                .lock()
                .unwrap()
                .iter()
                .all(|(url, _)| !url.contains("/records/lookup")),
            "incomplete baseline cannot trigger source lookup"
        );
        let owned: (i64, String, String) = {
            let conn = reopened
                .acquire_lock("inspect preserved source ownership")
                .unwrap();
            conn.query_row("SELECT is_deleted,local_path,local_checksum FROM assets WHERE library=?1 AND id='existing-master' AND version_size='original'", [ZONE], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap()
        };
        assert_eq!(
            owned,
            (0, old.to_str().unwrap().into(), "synthetic-local".into())
        );
        let (active, debt): (i64, i64) = {
            let conn = reopened
                .acquire_lock("inspect incomplete source baseline")
                .unwrap();
            (
                conn.query_row(
                    "SELECT COUNT(*) FROM source_revalidation_rounds WHERE phase='active'",
                    [],
                    |r| r.get(0),
                )
                .unwrap(),
                conn.query_row("SELECT COUNT(*) FROM source_revalidation_debt", [], |r| {
                    r.get(0)
                })
                .unwrap(),
            )
        };
        assert_eq!(
            active, 0,
            "unaccountable inventory cannot activate source debt"
        );
        assert!(debt > 0, "unfinished source candidates remain durable");
        let summary = reopened.get_summary().await.unwrap();
        assert_eq!(summary.source_revalidation_eligible, 0);
        assert_eq!(summary.source_revalidation_retained, debt as u64);
        assert_eq!(std::fs::read(&old).unwrap(), MEDIA);
        assert_eq!(std::fs::metadata(&old).unwrap().modified().unwrap(), mtime);
        drop(selected);
        drop(db);
        let recovered = Arc::new(reopened);
        let retry_requests = Arc::new(Mutex::new(Vec::new()));
        let valid = ValidEmpty(BaselineSession {
            database: "private",
            cycle: 0,
            inventory: Arc::new(Vec::new()),
            concurrent: Arc::new(Vec::new()),
            incremental_addition: Arc::new(Vec::new()),
            requests: Arc::clone(&retry_requests),
            inventory_queries: Arc::new(AtomicUsize::new(0)),
            inventory_starts: Arc::new(AtomicUsize::new(0)),
            boundaries: Arc::new(AtomicUsize::new(0)),
            changes: Arc::new(AtomicUsize::new(0)),
        });
        let mut library = PhotoLibrary::new(
            "https://synthetic.example/database/1/photos/production/private".into(),
            Arc::new(std::collections::HashMap::new()),
            Box::new(valid),
            Arc::new(zone()),
            "private".into(),
            crate::retry::RetryConfig::default(),
        )
        .await
        .unwrap();
        library.shadow_capture = Some(crate::icloud::photos::inbox::ShadowCapture::new(
            recovered.clone(),
            baseline_owner(),
            "com",
        ));
        let passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: library.all(),
            exclude_ids: Default::default(),
        }];
        let mut selected = make_run_cycle_library_state_with_passes(ZONE, &key, passes);
        selected.library = library;
        let builder = make_run_cycle_download_config_builder(&media, recovered.clone());
        let result = run_cycle(
            &[&selected],
            &make_run_cycle_config(),
            Some(recovered.as_ref()),
            false,
            &builder,
            download::DownloadControls::download_hidden(),
            &shared,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            result.failed_count, 0,
            "valid recovery baseline: {result:?}"
        );
        assert!(recovered.get_metadata(&key).await.unwrap().is_some());
        assert!(
            recovered
                .get_summary()
                .await
                .unwrap()
                .source_revalidation_eligible
                > 0
        );
        assert_eq!(
            recovered
                .get_summary()
                .await
                .unwrap()
                .source_revalidation_retained,
            0,
            "superseded unfinished rounds are not pending obligations"
        );
        crate::sync_loop::runner::drain_source_revalidation(
            &[&selected],
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(
            retry_requests
                .lock()
                .unwrap()
                .iter()
                .any(|(url, _)| url.contains("/records/lookup"))
        );
        assert_eq!(std::fs::read(&old).unwrap(), MEDIA);
    }
}

#[path = "v027_provider_signature.rs"]
mod provider_signature;

mod recorded_only;

mod modified_publication;

mod fallback_publication;

mod selected_coverage;

mod selected_albums;

mod historical_pending;

mod independent_retry;

mod raw_replay;

mod indexing_layout;

mod source_originals;

mod configuration_safety;

mod current_sparse_coverage;

#[cfg(all(unix, feature = "xmp"))]
mod sidecar_namespace;

#[tokio::test]
async fn issue1044_confirmed_delta_deletion_survives_unrelated_sparse_hold_after_baseline() {
    // Force both sides of the second-resolution promotion boundary.
    confirmed_delta_deletion_with_seen_at(0, "pending").await;
    confirmed_delta_deletion_with_seen_at(i64::MAX, "failed").await;
}

async fn confirmed_delta_deletion_with_seen_at(seen_at: i64, expected_status: &str) {
    use crate::state::SparseIdentityStore as _;

    #[derive(Clone)]
    struct DeletionSession {
        cycle: usize,
        lookups: Arc<Mutex<Vec<Vec<String>>>>,
    }
    #[async_trait::async_trait]
    impl PhotosSession for DeletionSession {
        async fn post(&self, url: &str, body: String, _: &[(&str, &str)]) -> anyhow::Result<Value> {
            let request: Value = serde_json::from_str(&body)?;
            let primary = json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"});
            if url.contains("/zones/list") {
                return Ok(
                    json!({"zones":[{"zoneID":primary,"syncToken":"baseline-boundary","deleted":false}]}),
                );
            }
            if request["query"]["recordType"] == "CheckIndexingState" {
                return Ok(
                    json!({"records":[{"recordName":"index-state","recordType":"CheckIndexingState","fields":{"state":{"value":"FINISHED"}}}]}),
                );
            }
            if url.contains("/internal/records/query/batch") {
                return Ok(album_count_response(0));
            }
            if url.contains("/records/query?") {
                return Ok(json!({"records":[],"syncToken":"rank-token"}));
            }
            if url.contains("/changes/zone?") {
                let (expected, records) = if self.cycle == 0 {
                    ("baseline-boundary", Vec::new())
                } else {
                    let mut sparse = crate::test_helpers::sparse_shared_asset_record();
                    sparse["recordName"] = json!("unresolved-child");
                    sparse["fields"]
                        .as_object_mut()
                        .unwrap()
                        .remove("masterRef");
                    (
                        "baseline-complete",
                        vec![
                            sparse,
                            json!({"recordName":"live-child","recordType":"CPLAsset","deleted":true}),
                        ],
                    )
                };
                assert_eq!(request["zones"][0]["syncToken"], expected);
                return Ok(
                    json!({"zones":[{"zoneID":primary,"syncToken":if self.cycle == 0 {"baseline-complete"} else {"held-successor"},"moreComing":false,"records":records}]}),
                );
            }
            if url.contains("/records/lookup?") {
                assert_eq!(request["zoneID"]["zoneName"], "PrimarySync");
                let names: Vec<String> = request["records"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|entry| entry["recordName"].as_str().unwrap().to_owned())
                    .collect();
                assert_eq!(request["zoneID"]["ownerRecordName"], "_defaultOwner");
                self.lookups.lock().unwrap().push(names.clone());
                let mut pair = full_album_page_with_download(
                    "PrimarySync",
                    "deleted-master",
                    "unused",
                    "https://example.invalid/photo",
                    4,
                    "unused",
                )["records"]
                    .as_array()
                    .unwrap()
                    .clone();
                for record in &mut pair {
                    record["fields"]["isDeleted"] = json!({"value":1,"type":"INT64"});
                    record["fields"]["isExpunged"] = json!({"value":1,"type":"INT64"});
                    record["fields"]["dateExpunged"] =
                        json!({"value":1_792_541_032_000_i64,"type":"TIMESTAMP"});
                }
                let mut sparse = crate::test_helpers::sparse_shared_asset_record();
                sparse["recordName"] = json!("unresolved-child");
                pair.push(sparse);
                pair.retain(|record| {
                    names
                        .iter()
                        .any(|name| record["recordName"] == name.as_str())
                });
                return Ok(json!({"records":pair}));
            }
            anyhow::bail!("unexpected synthetic provider endpoint {url}")
        }
        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(self.clone())
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("state.db");
    let root = dir.path().join("photos");
    std::fs::create_dir(&root).unwrap();
    let paths = [root.join("live.jpg"), root.join("live.mov")];
    let bytes = [
        b"synthetic still".as_slice(),
        b"synthetic motion".as_slice(),
    ];
    let config = make_run_cycle_config();
    let lookups = Arc::new(Mutex::new(Vec::new()));
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    for cycle in 0..3 {
        let db = Arc::new(
            SqliteStateDb::open_owned(&db_path, &baseline_owner())
                .await
                .unwrap(),
        );
        if cycle == 1 {
            let pending = crate::test_helpers::TestAssetRecord::new("deleted-master")
                .library("PrimarySync")
                .filename("pending.jpg")
                .build();
            db.upsert_seen(&pending).await.unwrap();
            db.acquire_lock("set deterministic promotion boundary")
                .unwrap()
                .execute(
                    "UPDATE assets SET last_seen_at=?1 WHERE library='PrimarySync' AND id='deleted-master'",
                    [seen_at],
                )
                .unwrap();
            db.upsert_asset_master_mapping("PrimarySync", "asset-deleted-master", "deleted-master")
                .await
                .unwrap();
            for ((version, name), (path, content)) in [
                (crate::state::VersionSizeKey::Original, "live.jpg"),
                (crate::state::VersionSizeKey::LiveOriginal, "live.mov"),
            ]
            .into_iter()
            .zip(paths.iter().zip(bytes))
            {
                std::fs::write(path, content).unwrap();
                let record = crate::test_helpers::TestAssetRecord::new("live-master")
                    .library("PrimarySync")
                    .version_size(version)
                    .filename(name)
                    .build();
                db.upsert_seen(&record).await.unwrap();
                db.mark_downloaded(
                    "PrimarySync",
                    "live-master",
                    version.as_str(),
                    path,
                    "synthetic-local",
                    None,
                )
                .await
                .unwrap();
            }
            db.upsert_asset_master_mapping("PrimarySync", "live-child", "live-master")
                .await
                .unwrap();
        }
        let mut library = PhotoLibrary::new(
            "https://synthetic.example/database/1/photos/production/private".into(),
            Arc::new(std::collections::HashMap::new()),
            Box::new(DeletionSession {
                cycle,
                lookups: lookups.clone(),
            }),
            Arc::new(json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"})),
            "private".into(),
            crate::retry::RetryConfig::default(),
        )
        .await
        .unwrap();
        library.shadow_capture = Some(crate::icloud::photos::inbox::ShadowCapture::new(
            db.clone(),
            baseline_owner(),
            "com",
        ));
        let passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: library.all(),
            exclude_ids: Arc::default(),
        }];
        let mut selected = make_run_cycle_library_state_with_passes(
            "PrimarySync",
            "sync_token:PrimarySync",
            passes,
        );
        selected.library = library;
        let builder = make_run_cycle_download_config_builder(&root, db.clone());
        let result = run_cycle(
            &[&selected],
            &config,
            Some(db.as_ref()),
            false,
            &builder,
            download::DownloadControls::download_hidden(),
            &shared,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("baseline-complete"),
            "cycle={cycle}: {result:?}"
        );
        if cycle == 0 {
            assert_eq!(result.failed_count, 0, "{result:?}");
            continue;
        }
        assert!(result.stats.identity_incomplete, "{result:?}");
        {
            let requested = lookups.lock().unwrap();
            assert!(
                requested
                    .iter()
                    .any(|names| names.iter().any(|name| name == "deleted-master")
                        && names.iter().any(|name| name == "asset-deleted-master")),
                "{requested:?}"
            );
        }
        assert!(
            !db.sparse_identities("PrimarySync")
                .await
                .unwrap()
                .is_empty()
        );
        drop(selected);
        drop(db);
        let reopened = SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap();
        assert_eq!(
            reopened
                .get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("baseline-complete")
        );
        assert_eq!(reopened.get_summary().await.unwrap().source_deleted, 3);
        assert!(
            reopened
                .get_pending()
                .await
                .unwrap()
                .iter()
                .all(|asset| asset.id.as_ref() != "deleted-master")
        );
        assert!(
            reopened
                .get_failed()
                .await
                .unwrap()
                .iter()
                .all(|asset| asset.id.as_ref() != "deleted-master")
        );
        let conn = reopened.acquire_lock("inspect synthetic deletion").unwrap();
        let retired: (String, i64, Option<i64>) = conn.query_row(
            "SELECT status,is_deleted,deleted_at FROM assets WHERE library='PrimarySync' AND id='deleted-master' AND version_size='original'",
            [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).unwrap();
        assert_eq!(retired, (expected_status.into(), 1, Some(1_792_541_032)));
        // The second reopened cycle must not retry a confirmed deletion.
        let deletion_lookups = lookups
            .lock()
            .unwrap()
            .iter()
            .filter(|names| names.iter().any(|name| name == "deleted-master"))
            .count();
        assert_eq!(deletion_lookups, 1, "cycle={cycle}");
        let mut stmt = conn.prepare("SELECT version_size,status,is_deleted FROM assets WHERE library='PrimarySync' AND id='live-master' ORDER BY version_size").unwrap();
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            rows,
            vec![
                ("live_original".into(), "downloaded".into(), 1),
                ("original".into(), "downloaded".into(), 1)
            ]
        );
        for (path, expected) in paths.iter().zip(bytes) {
            assert_eq!(std::fs::read(path).unwrap(), expected);
        }
    }
}

#[tokio::test]
async fn issue1044_historical_missed_livephoto_deletion_retries_after_baseline() {
    historical_source_revalidation_case(false).await;
}

#[tokio::test]
async fn issue1044_historical_live_source_settles_after_unknown_and_reopen() {
    historical_source_revalidation_case(true).await;
}

async fn historical_source_revalidation_case(live: bool) {
    #[derive(Clone)]
    struct DeletionSession {
        live: bool,
        cycle: usize,
        lookups: Arc<AtomicUsize>,
    }
    #[async_trait::async_trait]
    impl PhotosSession for DeletionSession {
        async fn post(&self, url: &str, body: String, _: &[(&str, &str)]) -> anyhow::Result<Value> {
            let request: Value = serde_json::from_str(&body)?;
            let primary = json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"});
            if url.contains("/zones/list") {
                return Ok(
                    json!({"zones":[{"zoneID":primary,"syncToken":"baseline-boundary","deleted":false}]}),
                );
            }
            if request["query"]["recordType"] == "CheckIndexingState" {
                return Ok(
                    json!({"records":[{"recordName":"index-state","recordType":"CheckIndexingState","fields":{"state":{"value":"FINISHED"}}}]}),
                );
            }
            if url.contains("/internal/records/query/batch") {
                return Ok(album_count_response(0));
            }
            if url.contains("/records/query?") {
                return Ok(json!({"records":[],"syncToken":"rank-token"}));
            }
            if url.contains("/changes/zone?") {
                let expected = if self.cycle == 0 {
                    "baseline-boundary"
                } else {
                    "baseline-complete"
                };
                assert_eq!(request["zones"][0]["syncToken"], expected);
                return Ok(
                    json!({"zones":[{"zoneID":primary,"syncToken":"baseline-complete","moreComing":false,"records":[]}]}),
                );
            }
            if url.contains("/records/lookup?") {
                assert_eq!(request["zoneID"]["zoneName"], "PrimarySync");
                let mut names: Vec<&str> = request["records"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|record| record["recordName"].as_str().unwrap())
                    .collect();
                names.sort_unstable();
                assert_eq!(names, vec!["live-child", "live-master"]);
                self.lookups.fetch_add(1, Ordering::SeqCst);
                if self.cycle == 0 {
                    return Ok(json!({"records":[
                        {"recordName":"live-master","serverErrorCode":"RETRY_LATER"},
                        {"recordName":"live-child","serverErrorCode":"RETRY_LATER"}
                    ]}));
                }
                if self.live {
                    return Ok(json!({"records":[
                        {"recordName":"live-master","recordType":"CPLMaster","zoneID":primary,"fields":{
                            "filenameEnc":{"value":"photo.jpg","type":"STRING"},
                            "resOriginalRes":{"value":{"downloadURL":"https://synthetic.example/photo.jpg","size":1024,"fileChecksum":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="}},
                            "resOriginalFileType":{"value":"public.jpeg"},"itemType":{"value":"public.jpeg"}}},
                        {"recordName":"live-child","recordType":"CPLAsset","zoneID":primary,"fields":{
                            "masterRef":{"value":{"recordName":"live-master","zoneID":primary},"type":"REFERENCE"},
                            "assetDate":{"value":1700000000000_i64,"type":"TIMESTAMP"}}}
                    ]}));
                }
                return Ok(json!({"records":[
                    {"recordName":"live-master","recordType":"CPLMaster","zoneID":primary,"fields":{
                        "isDeleted":{"value":1,"type":"INT64"},"isExpunged":{"value":1,"type":"INT64"},"dateExpunged":{"value":1792541032000_i64,"type":"TIMESTAMP"}}},
                    {"recordName":"live-child","recordType":"CPLAsset","zoneID":primary,"fields":{
                        "masterRef":{"value":{"recordName":"live-master"}},"isDeleted":{"value":1,"type":"INT64"},"isExpunged":{"value":1,"type":"INT64"},"dateExpunged":{"value":1792541032000_i64,"type":"TIMESTAMP"}}}
                ]}));
            }
            anyhow::bail!("unexpected synthetic provider endpoint {url}")
        }
        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(self.clone())
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("state.db");
    let root = dir.path().join("photos");
    std::fs::create_dir(&root).unwrap();
    let paths = [root.join("live.jpg"), root.join("live.mov")];
    let bytes = [
        b"synthetic still".as_slice(),
        b"synthetic motion".as_slice(),
    ];
    let config = make_run_cycle_config();
    let lookups = Arc::new(AtomicUsize::new(0));
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    for cycle in 0..3 {
        let db = Arc::new(
            SqliteStateDb::open_owned(&db_path, &baseline_owner())
                .await
                .unwrap(),
        );
        if cycle == 0 {
            for ((version, name), (path, content)) in [
                (crate::state::VersionSizeKey::Original, "live.jpg"),
                (crate::state::VersionSizeKey::LiveOriginal, "live.mov"),
            ]
            .into_iter()
            .zip(paths.iter().zip(bytes))
            {
                std::fs::write(path, content).unwrap();
                let record = crate::test_helpers::TestAssetRecord::new("live-master")
                    .library("PrimarySync")
                    .version_size(version)
                    .added_at(chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap())
                    .filename(name)
                    .build();
                db.upsert_seen(&record).await.unwrap();
                db.mark_downloaded(
                    "PrimarySync",
                    "live-master",
                    version.as_str(),
                    path,
                    "synthetic-local",
                    None,
                )
                .await
                .unwrap();
            }
            db.acquire_lock("seed existing verification debt").unwrap().execute(
                "INSERT INTO asset_verifications(library,id,version_size,state,reason,checked_at) VALUES('PrimarySync','live-master','original','unknown','existing-obligation',1)", [],
            ).unwrap();
            db.upsert_asset_master_mapping("PrimarySync", "live-child", "live-master")
                .await
                .unwrap();
            assert!(
                db.claim_legacy_master_state_owner("PrimarySync", "live-master", "live-child")
                    .await
                    .unwrap()
            );
        }
        let mut library = PhotoLibrary::new(
            "https://synthetic.example/database/1/photos/production/private".into(),
            Arc::new(std::collections::HashMap::new()),
            Box::new(DeletionSession {
                live,
                cycle,
                lookups: Arc::clone(&lookups),
            }),
            Arc::new(json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"})),
            "private".into(),
            crate::retry::RetryConfig::default(),
        )
        .await
        .unwrap();
        library.shadow_capture = Some(crate::icloud::photos::inbox::ShadowCapture::new(
            db.clone(),
            baseline_owner(),
            "com",
        ));
        let passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: library.all(),
            exclude_ids: Arc::default(),
        }];
        let mut selected = make_run_cycle_library_state_with_passes(
            "PrimarySync",
            "sync_token:v27:PrimarySync",
            passes,
        );
        selected.library = library;
        let builder = make_run_cycle_download_config_builder(&root, db.clone());
        let result = run_cycle(
            &[&selected],
            &config,
            Some(db.as_ref()),
            false,
            &builder,
            download::DownloadControls::download_hidden(),
            &shared,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            db.get_metadata("sync_token:v27:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("baseline-complete"),
            "cycle={cycle}: {result:?}"
        );
        assert_eq!(result.failed_count, 0, "{result:?}");
        assert!(
            db.get_metadata("observed_selection:v27:PrimarySync")
                .await
                .unwrap()
                .is_some(),
            "the new baseline must have a durable selection proof"
        );
        let mut stale = make_run_cycle_library_state_with_passes(
            "StaleSync",
            "sync_token:v27:StaleSync",
            Vec::new(),
        );
        stale.plan_is_stale = true;
        crate::sync_loop::runner::drain_source_revalidation(
            &[&stale, &selected],
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        drop(selected);
        drop(db);
        let reopened = SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap();
        assert_eq!(
            reopened
                .get_metadata("sync_token:v27:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("baseline-complete")
        );
        let conn = reopened.acquire_lock("inspect synthetic deletion").unwrap();
        let existing: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM asset_verifications WHERE reason='existing-obligation'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(existing, 1);
        if cycle == 2 {
            assert_eq!(lookups.load(Ordering::SeqCst), 2);
        }
        let mut stmt = conn.prepare("SELECT version_size,status,is_deleted FROM assets WHERE library='PrimarySync' AND id='live-master' ORDER BY version_size").unwrap();
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        if cycle == 0 {
            assert!(rows.iter().all(|row| row.2 == 0));
            let debt: i64 = conn
                .query_row("SELECT COUNT(*) FROM source_revalidation_debt", [], |r| {
                    r.get(0)
                })
                .unwrap();
            assert_eq!(debt, 1);
            conn.execute("UPDATE source_revalidation_debt SET next_retry_at=0", [])
                .unwrap();
        } else {
            let remaining: i64 = conn
                .query_row("SELECT COUNT(*) FROM source_revalidation_debt", [], |r| {
                    r.get(0)
                })
                .unwrap();
            assert_eq!(
                remaining, 0,
                "confirmed live or deleted evidence must settle the debt"
            );
            assert_eq!(
                rows,
                vec![
                    (
                        "live_original".into(),
                        "downloaded".into(),
                        i64::from(!live)
                    ),
                    ("original".into(), "downloaded".into(), i64::from(!live))
                ]
            );
            assert!(
                lookups.load(Ordering::SeqCst) > 0,
                "provider deletion must be confirmed"
            );
        }
        for (path, expected) in paths.iter().zip(bytes) {
            assert_eq!(std::fs::read(path).unwrap(), expected);
        }
    }
}

#[derive(Clone)]
struct ResetCycleSession {
    zone: &'static str,
    inventory: Arc<Vec<Value>>,
    full_queries: Arc<AtomicUsize>,
    changes: Arc<AtomicUsize>,
    fail_full: bool,
    auth_expired: bool,
    boundary: &'static str,
}

#[async_trait::async_trait]
impl PhotosSession for ResetCycleSession {
    async fn post(
        &self,
        url: &str,
        body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        let request: Value = serde_json::from_str(&body)?;
        if url.contains("/zones/list") {
            return Ok(
                json!({"zones":[{"zoneID":{"zoneName":self.zone,"ownerRecordName":"_defaultOwner"},"syncToken":self.boundary,"deleted":false}]}),
            );
        }
        if request["query"]["recordType"] == "CheckIndexingState" {
            return Ok(
                json!({"records":[{"recordName":"index-state","recordType":"CheckIndexingState","fields":{"state":{"value":"FINISHED"}}}]}),
            );
        }
        if url.contains("/internal/records/query/batch") {
            return Ok(album_count_response((self.inventory.len() / 2) as u64));
        }
        if url.contains("/records/query?") {
            if self.fail_full {
                anyhow::bail!("synthetic reset enumeration failure");
            }
            let offset = request["query"]["filterBy"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|filter| filter["fieldName"] == "startRank")
                .and_then(|filter| filter["fieldValue"]["value"].as_u64())
                .unwrap_or(0);
            if self.auth_expired {
                if offset == 0 {
                    self.full_queries.fetch_add(1, Ordering::SeqCst);
                }
                return Err(anyhow::Error::new(
                    crate::icloud::photos::session::HttpStatusError {
                        status: 401,
                        url: "https://synthetic.example/records/query".into(),
                        retry_after: None,
                        body: None,
                    },
                ));
            }
            if offset == 0 {
                self.full_queries.fetch_add(1, Ordering::SeqCst);
                return Ok(
                    json!({"records":self.inventory.as_ref().clone(),"syncToken":"inventory-last-page","moreComing":false}),
                );
            }
            return Ok(json!({"records":[],"syncToken":"inventory-last-page"}));
        }
        if url.contains("/changes/zone?") {
            let n = self.changes.fetch_add(1, Ordering::SeqCst);
            return Ok(
                json!({"zones":[{"zoneID":{"zoneName":self.zone,"ownerRecordName":"_defaultOwner"},"records":[],"syncToken":format!("reset-after-{n}"),"moreComing":false}]}),
            );
        }
        if url.contains("/records/lookup?") {
            let requested = request["records"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|record| record["recordName"].as_str())
                .collect::<rustc_hash::FxHashSet<_>>();
            let records = self
                .inventory
                .iter()
                .filter(|record| {
                    record["recordName"]
                        .as_str()
                        .is_some_and(|name| requested.contains(name))
                })
                .cloned()
                .collect::<Vec<_>>();
            return Ok(json!({"records": records}));
        }
        anyhow::bail!("unexpected reset-cycle endpoint: {url}");
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

#[tokio::test]
async fn genuine_v36_upgrade_runs_one_epoch_baseline_and_preserves_state() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    std::fs::create_dir_all(&root).unwrap();
    let db_path = dir.path().join("state.db");
    {
        let stock = SqliteStateDb::create_stock_v36_for_test(&db_path, &baseline_owner());
        let conn = stock.acquire_lock("seed genuine v36 state").unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO metadata(key,value) VALUES(?1,'v36-token')",
            [format!("sync_token:v27:{ZONE}")],
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO metadata(key,value) VALUES('baseline_cursor:v27:old-scope:old-enum:','old-boundary')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO metadata(key,value) VALUES(?1,'1')",
            [crate::sync_cycle::indexing_replay_key(ZONE)],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO owned_temp_files(path,claimed_at) VALUES(x'01',0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO metadata_capture_retries(library,asset_id,target_revision,evidence,generation,attempts,last_attempt_at,next_retry_at) VALUES(?1,'asset-x',1,'e',1,1,0,0)",
            [ZONE],
        )
        .unwrap();
    }

    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute(
            "DELETE FROM metadata WHERE key=?1",
            [crate::state::db::sync_epoch::TEST_PREEXISTING_EPOCH_KEY],
        )
        .unwrap();
    }
    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    {
        let conn = db.acquire_lock("inspect genuine v36 migration").unwrap();
        let epoch = crate::state::db::sync_epoch::validate(&conn).unwrap();
        assert_eq!(epoch.origin, "existing_upgrade");
    }

    let full_queries = Arc::new(AtomicUsize::new(0));
    let changes = Arc::new(AtomicUsize::new(0));
    let session = ResetCycleSession {
        zone: ZONE,
        inventory: Arc::new(Vec::new()),
        full_queries: Arc::clone(&full_queries),
        changes: Arc::clone(&changes),
        fail_full: false,
        auth_expired: false,
        boundary: BOUNDARY,
    };
    let mut library = PhotoLibrary::new(
        "https://synthetic.example/database/1/photos/production/private".into(),
        Arc::new(std::collections::HashMap::new()),
        Box::new(session),
        Arc::new(zone()),
        "private".into(),
        crate::retry::RetryConfig::default(),
    )
    .await
    .unwrap();
    library.shadow_capture = Some(crate::icloud::photos::inbox::ShadowCapture::new(
        db.clone(),
        baseline_owner(),
        "com",
    ));
    let key = sync_token_key(ZONE);
    let passes = vec![AlbumPass {
        kind: PassKind::Unfiled,
        album: library.all(),
        exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
    }];
    let mut selected = make_run_cycle_library_state_with_passes(ZONE, &key, passes);
    selected.library = library;
    let builder = make_run_cycle_download_config_builder(&root, db.clone());
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let result = run_cycle(
        &[&selected],
        &make_run_cycle_config(),
        Some(db.as_ref()),
        false,
        &builder,
        download::DownloadControls::download_hidden(),
        &shared,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(result.failed_count, 0);
    assert_eq!(full_queries.load(Ordering::SeqCst), 1);
    {
        let conn = db.acquire_lock("inspect genuine v36 outcome").unwrap();
        let active: Option<String> = conn
            .query_row(
                "SELECT value FROM metadata WHERE key=?1",
                [format!("sync_token:v27:{ZONE}")],
                |row| row.get(0),
            )
            .ok();
        assert!(active.is_some());
        assert_ne!(active.as_deref(), Some("v36-token"));
        let old_boundary: Option<String> = conn
            .query_row(
                "SELECT value FROM metadata WHERE key='baseline_cursor:v27:old-scope:old-enum:'",
                [],
                |row| row.get(0),
            )
            .ok();
        assert_eq!(
            old_boundary.as_deref(),
            Some("old-boundary"),
            "the pre-epoch cursor must remain untouched"
        );
        let replay_key = crate::sync_cycle::indexing_replay_key(ZONE);
        let replay_present: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM metadata WHERE key=?1)",
                [&replay_key],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            !replay_present,
            "the completed epoch replay must retire the production marker: {replay_key}"
        );
        for table in ["owned_temp_files", "metadata_capture_retries"] {
            let count: i64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, 1, "v36 state must survive the epoch: {table}");
        }
        let record = crate::state::db::sync_epoch::read_library(&conn, ZONE)
            .unwrap()
            .unwrap();
        assert_eq!(record.state, "complete");
        assert!(record.binding.is_none());
    }
    let _ = run_cycle(
        &[&selected],
        &make_run_cycle_config(),
        Some(db.as_ref()),
        false,
        &builder,
        download::DownloadControls::download_hidden(),
        &shared,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        full_queries.load(Ordering::SeqCst),
        1,
        "the post-epoch cycle must be incremental"
    );
}

#[tokio::test]
async fn reset_cycle_stores_one_baseline_then_rediscovers_quietly() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    std::fs::create_dir_all(&root).unwrap();
    let db_path = dir.path().join("state.db");
    let media = root.join("existing.JPG");
    std::fs::write(&media, MEDIA).unwrap();
    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    let record = crate::test_helpers::TestAssetRecord::new("asset-reset")
        .library(ZONE)
        .build();
    db.upsert_seen(&record).await.unwrap();
    db.mark_downloaded(
        ZONE,
        "asset-reset",
        "original",
        &media,
        "synthetic-local",
        None,
    )
    .await
    .unwrap();
    {
        let conn = db
            .acquire_lock("seed pending generation and local work")
            .unwrap();
        conn.execute(
            "INSERT INTO owned_temp_files(path,claimed_at) VALUES(x'01',0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO metadata_capture_retries(library,asset_id,target_revision,evidence,generation,attempts,last_attempt_at,next_retry_at) VALUES('other-scope','asset-x',1,'e',1,1,0,0)",
            [],
        )
        .unwrap();
    }
    {
        let conn = db.acquire_lock("seed historical reset state").unwrap();
        clear_test_epoch(&conn);
        conn.execute(
            "INSERT INTO source_revalidation_rounds(id,library,scope,enum_hash,boundary,high_id,after_id,phase) VALUES('old-round',?1,'scope','policy','boundary','high','after','active')",
            [ZONE],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO source_revalidation_debt(round,id) VALUES('old-round','asset-reset')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO metadata(key,value) VALUES(?1,'old-token')",
            [format!("sync_token:v27:{ZONE}")],
        )
        .unwrap();
        conn.execute_batch("PRAGMA user_version=42").unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO metadata(key,value) VALUES(?1,'stale-proof')",
            [format!("observed_selection:v27:{ZONE}")],
        )
        .unwrap();
    }
    drop(db);

    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    {
        let conn = db.acquire_lock("verify historical reset").unwrap();
        let phase: String = conn
            .query_row(
                "SELECT phase FROM source_revalidation_rounds WHERE id='old-round'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(phase, "abandoned");
        let old_token: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM metadata WHERE key=?1)",
                [format!("sync_token:v27:{ZONE}")],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            old_token,
            "migration quarantines rounds but leaves lazy enrollment to the cycle"
        );
        let enrolled: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM metadata WHERE key=?1)",
                [crate::state::db::sync_epoch::library_key(ZONE)],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!enrolled, "enrollment must wait for the selected library");
    }

    let full_queries = Arc::new(AtomicUsize::new(0));
    let changes = Arc::new(AtomicUsize::new(0));
    let session = ResetCycleSession {
        zone: ZONE,
        inventory: Arc::new(Vec::new()),
        full_queries: Arc::clone(&full_queries),
        changes: Arc::clone(&changes),
        fail_full: false,
        auth_expired: false,
        boundary: BOUNDARY,
    };
    let config = make_run_cycle_config();
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;

    let mut library = PhotoLibrary::new(
        "https://synthetic.example/database/1/photos/production/private".into(),
        Arc::new(std::collections::HashMap::new()),
        Box::new(session.clone()),
        Arc::new(zone()),
        "private".into(),
        crate::retry::RetryConfig::default(),
    )
    .await
    .unwrap();
    library.shadow_capture = Some(crate::icloud::photos::inbox::ShadowCapture::new(
        db.clone(),
        baseline_owner(),
        "com",
    ));
    let key = sync_token_key(ZONE);
    let passes = vec![AlbumPass {
        kind: PassKind::Unfiled,
        album: library.all(),
        exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
    }];
    let mut selected = make_run_cycle_library_state_with_passes(ZONE, &key, passes);
    selected.library = library;
    let builder = make_run_cycle_download_config_builder(&root, db.clone());
    let result = run_cycle(
        &[&selected],
        &config,
        Some(db.as_ref()),
        false,
        &builder,
        download::DownloadControls::download_hidden(),
        &shared,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(result.failed_count, 0);
    assert_eq!(
        full_queries.load(Ordering::SeqCst),
        1,
        "the reset must establish exactly one fresh baseline"
    );
    {
        let conn = db.acquire_lock("verify completed reset baseline").unwrap();
        assert!(
            !crate::state::db::sync_epoch::should_wake(&conn, ZONE, chrono::Utc::now().timestamp())
                .unwrap()
        );
        let token: Option<String> = conn
            .query_row(
                "SELECT value FROM metadata WHERE key=?1",
                [format!("sync_token:v27:{ZONE}")],
                |row| row.get(0),
            )
            .ok();
        assert!(token.is_some(), "fresh baseline must store a checkpoint");
        let active: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM source_revalidation_rounds WHERE library=?1 AND phase='active'",
                [ZONE],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(active, 1, "fresh baseline must activate its round");
    }
    drop(db);

    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    let mut library = PhotoLibrary::new(
        "https://synthetic.example/database/1/photos/production/private".into(),
        Arc::new(std::collections::HashMap::new()),
        Box::new(session),
        Arc::new(zone()),
        "private".into(),
        crate::retry::RetryConfig::default(),
    )
    .await
    .unwrap();
    library.shadow_capture = Some(crate::icloud::photos::inbox::ShadowCapture::new(
        db.clone(),
        baseline_owner(),
        "com",
    ));
    let passes = vec![AlbumPass {
        kind: PassKind::Unfiled,
        album: library.all(),
        exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
    }];
    let mut selected = make_run_cycle_library_state_with_passes(ZONE, &key, passes);
    selected.library = library;
    let builder = make_run_cycle_download_config_builder(&root, db.clone());
    run_cycle(
        &[&selected],
        &config,
        Some(db.as_ref()),
        false,
        &builder,
        download::DownloadControls::download_hidden(),
        &shared,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        full_queries.load(Ordering::SeqCst),
        1,
        "a quiet post-reset cycle must not repeat the full enumeration"
    );
    assert!(changes.load(Ordering::SeqCst) > 0);
    {
        let conn = db
            .acquire_lock("verify pending generation and local work")
            .unwrap();
        for table in ["owned_temp_files", "metadata_capture_retries"] {
            let count: i64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(
                count, 1,
                "pending state must survive reset and reopen: {table}"
            );
        }
    }
}

#[tokio::test]
async fn reset_cycle_changed_root_keeps_old_verified_file() {
    use std::path::{Path, PathBuf};

    fn files_under(root: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(root) else {
            return out;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(files_under(&path));
            } else {
                out.push(path);
            }
        }
        out
    }

    let dir = tempfile::tempdir().unwrap();
    let old_root = dir.path().join("old");
    let new_root = dir.path().join("new");
    std::fs::create_dir_all(&old_root).unwrap();
    std::fs::create_dir_all(&new_root).unwrap();
    let old_media = old_root.join("existing.JPG");
    std::fs::write(&old_media, MEDIA).unwrap();
    let old_mtime = std::fs::metadata(&old_media).unwrap().modified().unwrap();
    let local_checksum = crate::download::file::compute_sha256(&old_media)
        .await
        .unwrap();

    let server = wiremock::MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/asset"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
        .mount(&server)
        .await;
    let records = pair(
        "asset-reset",
        "existing.JPG",
        &format!("{}/asset", server.uri()),
        1,
    );

    let db_path = dir.path().join("state.db");
    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    let record = crate::test_helpers::TestAssetRecord::new("asset-reset")
        .library(ZONE)
        .version_size(crate::state::VersionSizeKey::Original)
        .checksum(&generation(1))
        .filename("existing.JPG")
        .size(MEDIA.len() as u64)
        .build();
    db.upsert_seen(&record).await.unwrap();
    db.mark_downloaded(
        ZONE,
        "asset-reset",
        "original",
        &old_media,
        &local_checksum,
        None,
    )
    .await
    .unwrap();
    db.claim_legacy_master_state_owner(ZONE, "asset-reset", "asset-asset-reset")
        .await
        .unwrap();
    let sidecar = old_root.join("existing.JPG.xmp");
    std::fs::write(&sidecar, b"synthetic sidecar").unwrap();
    let sidecar_mtime = std::fs::metadata(&sidecar).unwrap().modified().unwrap();
    {
        let conn = db.acquire_lock("seed historical reset state").unwrap();
        clear_test_epoch(&conn);
        conn.execute(
            "INSERT INTO source_revalidation_rounds(id,library,scope,enum_hash,boundary,high_id,after_id,phase) VALUES('old-round',?1,'scope','policy','boundary','high','after','active')",
            [ZONE],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO source_revalidation_debt(round,id) VALUES('old-round','asset-reset')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO metadata(key,value) VALUES(?1,'old-token')",
            [format!("sync_token:v27:{ZONE}")],
        )
        .unwrap();
        conn.execute_batch("PRAGMA user_version=42").unwrap();
    }
    drop(db);

    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    let full_queries = Arc::new(AtomicUsize::new(0));
    let changes = Arc::new(AtomicUsize::new(0));
    let session = ResetCycleSession {
        zone: ZONE,
        inventory: Arc::new(records),
        full_queries: Arc::clone(&full_queries),
        changes: Arc::clone(&changes),
        fail_full: false,
        auth_expired: false,
        boundary: BOUNDARY,
    };
    let mut library = PhotoLibrary::new(
        "https://synthetic.example/database/1/photos/production/private".into(),
        Arc::new(std::collections::HashMap::new()),
        Box::new(session),
        Arc::new(zone()),
        "private".into(),
        crate::retry::RetryConfig::default(),
    )
    .await
    .unwrap();
    library.shadow_capture = Some(crate::icloud::photos::inbox::ShadowCapture::new(
        db.clone(),
        baseline_owner(),
        "com",
    ));
    let key = sync_token_key(ZONE);
    let passes = vec![AlbumPass {
        kind: PassKind::Unfiled,
        album: library.all(),
        exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
    }];
    let mut selected = make_run_cycle_library_state_with_passes(ZONE, &key, passes);
    selected.library = library;
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let builder = make_run_cycle_download_config_builder(&new_root, db.clone());
    run_cycle(
        &[&selected],
        &make_run_cycle_config(),
        Some(db.as_ref()),
        false,
        &builder,
        download::DownloadControls::download_hidden(),
        &shared,
        &CancellationToken::new(),
    )
    .await
    .unwrap();

    let media_gets = server
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|request| request.url.path() == "/asset")
        .count();
    assert_eq!(media_gets, 0, "verified old-root media must not be fetched");
    assert_eq!(std::fs::read(&old_media).unwrap(), MEDIA);
    assert_eq!(
        std::fs::metadata(&old_media).unwrap().modified().unwrap(),
        old_mtime,
        "old recorded path must not be rewritten"
    );
    assert!(
        files_under(&new_root)
            .iter()
            .all(|path| path.file_name().and_then(|name| name.to_str()) != Some("existing.JPG")),
        "a changed root must not relocate the verified file"
    );
    assert_eq!(std::fs::read(&sidecar).unwrap(), b"synthetic sidecar");
    assert_eq!(
        std::fs::metadata(&sidecar).unwrap().modified().unwrap(),
        sidecar_mtime,
        "sidecar bytes and timestamp must be preserved"
    );
    {
        let conn = db.acquire_lock("verify metadata path").unwrap();
        let metadata_path: String = conn
            .query_row(
                "SELECT local_path FROM asset_metadata_paths WHERE library=?1 AND id='asset-reset' AND version_size='original'",
                [ZONE],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(metadata_path, old_media.to_str().unwrap());
    }
    assert_eq!(full_queries.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn reset_cycle_keeps_healthy_peer_incremental() {
    const PEER: &str = "SharedSync-reset-peer";
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    std::fs::create_dir_all(&root).unwrap();
    let db_path = dir.path().join("state.db");
    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    {
        let conn = db.acquire_lock("seed peer and reset state").unwrap();
        clear_test_epoch(&conn);
        conn.execute(
            "INSERT INTO source_revalidation_rounds(id,library,scope,enum_hash,boundary,high_id,after_id,phase) VALUES('old-round',?1,'scope','policy','boundary','high','after','active')",
            [ZONE],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO source_revalidation_debt(round,id) VALUES('old-round','asset-reset')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO metadata(key,value) VALUES(?1,'old-token')",
            [format!("sync_token:v27:{ZONE}")],
        )
        .unwrap();
        conn.execute_batch("PRAGMA user_version=42").unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO metadata(key,value) VALUES(?1,'peer-token')",
            [format!("sync_token:v27:{PEER}")],
        )
        .unwrap();
    }
    drop(db);
    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    {
        let conn = db.acquire_lock("verify peer token survives reset").unwrap();
        let peer_token: Option<String> = conn
            .query_row(
                "SELECT value FROM metadata WHERE key=?1",
                [format!("sync_token:v27:{PEER}")],
                |row| row.get(0),
            )
            .ok();
        assert_eq!(peer_token.as_deref(), Some("peer-token"));
        let epoch = crate::state::db::sync_epoch::validate(&conn).unwrap().epoch;
        let peer_record = json!({
            "version": 2,
            "epoch": epoch,
            "library": PEER,
            "state": "complete",
            "reason": "existing_upgrade",
            "attempt": "peer-attempt",
            "attempts": 1,
            "next_attempt_at": 0,
            "last_outcome": "complete",
            "baseline_scope": "peer-scope",
            "enrolled_at": 0,
            "completed_at": 0
        });
        conn.execute(
            "INSERT INTO metadata(key,value) VALUES(?1,?2)",
            rusqlite::params![
                crate::state::db::sync_epoch::library_key(PEER),
                peer_record.to_string()
            ],
        )
        .unwrap();
    }

    let reset_full = Arc::new(AtomicUsize::new(0));
    let reset_changes = Arc::new(AtomicUsize::new(0));
    let peer_full = Arc::new(AtomicUsize::new(0));
    let peer_changes = Arc::new(AtomicUsize::new(0));
    let reset_session = ResetCycleSession {
        zone: ZONE,
        inventory: Arc::new(Vec::new()),
        full_queries: Arc::clone(&reset_full),
        changes: Arc::clone(&reset_changes),
        fail_full: false,
        auth_expired: false,
        boundary: BOUNDARY,
    };
    let peer_session = ResetCycleSession {
        zone: PEER,
        inventory: Arc::new(Vec::new()),
        full_queries: Arc::clone(&peer_full),
        changes: Arc::clone(&peer_changes),
        fail_full: false,
        auth_expired: false,
        boundary: BOUNDARY,
    };
    let mut reset_library = PhotoLibrary::new(
        "https://synthetic.example/database/1/photos/production/private".into(),
        Arc::new(std::collections::HashMap::new()),
        Box::new(reset_session),
        Arc::new(zone()),
        "private".into(),
        crate::retry::RetryConfig::default(),
    )
    .await
    .unwrap();
    reset_library.shadow_capture = Some(crate::icloud::photos::inbox::ShadowCapture::new(
        db.clone(),
        baseline_owner(),
        "com",
    ));
    let mut peer_library = PhotoLibrary::new(
        "https://synthetic.example/database/1/photos/production/private".into(),
        Arc::new(std::collections::HashMap::new()),
        Box::new(peer_session),
        Arc::new(json!({"zoneName":PEER,"ownerRecordName":"_defaultOwner"})),
        "private".into(),
        crate::retry::RetryConfig::default(),
    )
    .await
    .unwrap();
    peer_library.shadow_capture = Some(crate::icloud::photos::inbox::ShadowCapture::new(
        db.clone(),
        baseline_owner(),
        "com",
    ));
    let reset_key = sync_token_key(ZONE);
    let peer_key = sync_token_key(PEER);
    let reset_passes = vec![AlbumPass {
        kind: PassKind::Unfiled,
        album: reset_library.all(),
        exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
    }];
    let peer_passes = vec![AlbumPass {
        kind: PassKind::Unfiled,
        album: peer_library.all(),
        exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
    }];
    let mut reset_selected =
        make_run_cycle_library_state_with_passes(ZONE, &reset_key, reset_passes);
    reset_selected.library = reset_library;
    let mut peer_selected = make_run_cycle_library_state_with_passes(PEER, &peer_key, peer_passes);
    peer_selected.library = peer_library;
    let builder = make_run_cycle_download_config_builder(&root, db.clone());
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    run_cycle(
        &[&reset_selected, &peer_selected],
        &make_run_cycle_config(),
        Some(db.as_ref()),
        false,
        &builder,
        download::DownloadControls::download_hidden(),
        &shared,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        reset_full.load(Ordering::SeqCst),
        1,
        "reset library must establish one baseline"
    );
    assert_eq!(
        peer_full.load(Ordering::SeqCst),
        0,
        "healthy peer must not repeat a full enumeration"
    );
    assert!(
        peer_changes.load(Ordering::SeqCst) > 0,
        "healthy peer must advance incrementally"
    );
}

#[tokio::test]
async fn reset_cycle_failure_backs_off_without_tight_loop() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    std::fs::create_dir_all(&root).unwrap();
    let db_path = dir.path().join("state.db");
    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    {
        let conn = db.acquire_lock("seed failing reset state").unwrap();
        clear_test_epoch(&conn);
        conn.execute(
            "INSERT INTO source_revalidation_rounds(id,library,scope,enum_hash,boundary,high_id,after_id,phase) VALUES('old-round',?1,'scope','policy','boundary','high','after','active')",
            [ZONE],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO source_revalidation_debt(round,id) VALUES('old-round','asset-reset')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO metadata(key,value) VALUES(?1,'old-token')",
            [format!("sync_token:v27:{ZONE}")],
        )
        .unwrap();
        conn.execute_batch("PRAGMA user_version=42").unwrap();
    }
    drop(db);
    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    let full_queries = Arc::new(AtomicUsize::new(0));
    let session = ResetCycleSession {
        zone: ZONE,
        inventory: Arc::new(Vec::new()),
        full_queries: Arc::clone(&full_queries),
        changes: Arc::new(AtomicUsize::new(0)),
        fail_full: true,
        auth_expired: false,
        boundary: BOUNDARY,
    };
    let mut library = PhotoLibrary::new(
        "https://synthetic.example/database/1/photos/production/private".into(),
        Arc::new(std::collections::HashMap::new()),
        Box::new(session),
        Arc::new(zone()),
        "private".into(),
        crate::retry::RetryConfig::default(),
    )
    .await
    .unwrap();
    library.shadow_capture = Some(crate::icloud::photos::inbox::ShadowCapture::new(
        db.clone(),
        baseline_owner(),
        "com",
    ));
    let key = sync_token_key(ZONE);
    let passes = vec![AlbumPass {
        kind: PassKind::Unfiled,
        album: library.all(),
        exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
    }];
    let mut selected = make_run_cycle_library_state_with_passes(ZONE, &key, passes);
    selected.library = library;
    let base = make_run_cycle_download_config_builder(&root, db.clone());
    let builder = move |mode, exclude, groupings, zone| {
        let mut config = (*base(mode, exclude, groupings, zone)).clone();
        config.retry.max_retries = 0;
        Arc::new(config)
    };
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let _ = run_cycle(
        &[&selected],
        &make_run_cycle_config(),
        Some(db.as_ref()),
        false,
        &builder,
        download::DownloadControls::download_hidden(),
        &shared,
        &CancellationToken::new(),
    )
    .await;
    let now = chrono::Utc::now().timestamp();
    {
        let conn = db.acquire_lock("inspect failed reset backoff").unwrap();
        assert!(
            !crate::state::db::sync_epoch::should_wake(&conn, ZONE, now).unwrap(),
            "a failed reset must not remain immediately due"
        );
        let value: String = conn
            .query_row(
                "SELECT value FROM metadata WHERE key=?1",
                [crate::state::db::sync_epoch::library_key(ZONE)],
                |row| row.get(0),
            )
            .unwrap();
        let record: Value = serde_json::from_str(&value).unwrap();
        assert_eq!(record["attempts"], 1);
        assert!(record["next_attempt_at"].as_i64().unwrap() > now);
    }
    let before_backoff_cycle = full_queries.load(Ordering::SeqCst);
    let _ = run_cycle(
        &[&selected],
        &make_run_cycle_config(),
        Some(db.as_ref()),
        false,
        &builder,
        download::DownloadControls::download_hidden(),
        &shared,
        &CancellationToken::new(),
    )
    .await;
    assert_eq!(
        full_queries.load(Ordering::SeqCst),
        before_backoff_cycle,
        "a normal scheduled cycle must not bypass epoch backoff"
    );
    {
        let conn = db.acquire_lock("force epoch due").unwrap();
        force_epoch_due(&conn, ZONE);
    }
    let _ = run_cycle(
        &[&selected],
        &make_run_cycle_config(),
        Some(db.as_ref()),
        false,
        &builder,
        download::DownloadControls::download_hidden(),
        &shared,
        &CancellationToken::new(),
    )
    .await;
    {
        let conn = db
            .acquire_lock("inspect second failed reset backoff")
            .unwrap();
        let value: String = conn
            .query_row(
                "SELECT value FROM metadata WHERE key=?1",
                [crate::state::db::sync_epoch::library_key(ZONE)],
                |row| row.get(0),
            )
            .unwrap();
        let record: Value = serde_json::from_str(&value).unwrap();
        assert_eq!(record["attempts"], 2);
        assert!(
            !crate::state::db::sync_epoch::should_wake(&conn, ZONE, chrono::Utc::now().timestamp())
                .unwrap()
        );
    }
}

#[tokio::test]
async fn epoch_pending_defer_preserves_record_and_token_without_provider_work() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    std::fs::create_dir_all(&root).unwrap();
    let db_path = dir.path().join("state.db");
    seed_existing_upgrade_db(&db_path);
    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    db.enroll_sync_epoch(vec![(ZONE.to_owned(), "scope".to_owned())])
        .await
        .unwrap();
    let future = chrono::Utc::now().timestamp() + 3600;
    {
        let conn = db.acquire_lock("seed deferred epoch state").unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO metadata(key,value) VALUES(?1,'existing-token')",
            [format!("sync_token:v27:{ZONE}")],
        )
        .unwrap();
        let key = crate::state::db::sync_epoch::library_key(ZONE);
        conn.execute(
            "UPDATE metadata SET value=json_set(value,'$.next_attempt_at',?2) WHERE key=?1",
            rusqlite::params![key, future],
        )
        .unwrap();
    }
    let full_queries = Arc::new(AtomicUsize::new(0));
    let changes = Arc::new(AtomicUsize::new(0));
    let selected = reset_cycle_state(
        Arc::clone(&db),
        ZONE,
        Arc::clone(&full_queries),
        Arc::clone(&changes),
        false,
    )
    .await;
    let builder = make_run_cycle_download_config_builder(&root, db.clone());
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let _ = run_cycle(
        &[&selected],
        &make_run_cycle_config(),
        Some(db.as_ref()),
        false,
        &builder,
        download::DownloadControls::download_hidden(),
        &shared,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    let conn = db.acquire_lock("inspect deferred epoch state").unwrap();
    let present = |key: &str| -> bool {
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM metadata WHERE key=?1)",
            [key],
            |row| row.get(0),
        )
        .unwrap()
    };
    eprintln!(
        "DEFER full_queries={} changes={} record={} token={}",
        full_queries.load(Ordering::SeqCst),
        changes.load(Ordering::SeqCst),
        present(&crate::state::db::sync_epoch::library_key(ZONE)),
        present(&format!("sync_token:v27:{ZONE}")),
    );
    assert_eq!(
        full_queries.load(Ordering::SeqCst),
        0,
        "a backed-off epoch must not run a provider inventory"
    );
    assert_eq!(
        changes.load(Ordering::SeqCst),
        0,
        "a backed-off epoch must not run a provider delta"
    );
    assert!(
        present(&crate::state::db::sync_epoch::library_key(ZONE)),
        "the epoch record must survive an eligible deferred cycle"
    );
    assert!(
        present(&format!("sync_token:v27:{ZONE}")),
        "the existing token must survive an eligible deferred cycle"
    );
}

#[tokio::test]
async fn epoch_config_drift_stages_then_promotes_without_repeating_full() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    std::fs::create_dir_all(&root).unwrap();
    let db_path = dir.path().join("state.db");
    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    {
        let conn = db.acquire_lock("seed epoch config drift state").unwrap();
        clear_test_epoch(&conn);
        conn.execute(
            "INSERT INTO source_revalidation_rounds(id,library,scope,enum_hash,boundary,high_id,after_id,phase) VALUES('old-round',?1,'scope','policy','boundary','high','after','active')",
            [ZONE],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO source_revalidation_debt(round,id) VALUES('old-round','asset-reset')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO metadata(key,value) VALUES(?1,'old-token')",
            [format!("sync_token:v27:{ZONE}")],
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO metadata(key,value) VALUES('enum_config_hash','old-enum')",
            [],
        )
        .unwrap();
        conn.execute_batch("PRAGMA user_version=42").unwrap();
    }
    drop(db);

    let config = make_run_cycle_config();
    let current_enum = download::compute_config_hash(&config);
    let pending_key = format!("pending_sync_token:v27:{current_enum}:{ZONE}");
    let active_key = format!("sync_token:v27:{ZONE}");
    let full_queries = Arc::new(AtomicUsize::new(0));
    let changes = Arc::new(AtomicUsize::new(0));
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;

    let run = |full_queries: Arc<AtomicUsize>, changes: Arc<AtomicUsize>| {
        let config = &config;
        let root = &root;
        let shared = &shared;
        let db_path = &db_path;
        async move {
            let db = Arc::new(
                SqliteStateDb::open_owned(db_path, &baseline_owner())
                    .await
                    .unwrap(),
            );
            let session = ResetCycleSession {
                zone: ZONE,
                inventory: Arc::new(Vec::new()),
                full_queries,
                changes,
                fail_full: false,
                auth_expired: false,
                boundary: BOUNDARY,
            };
            let mut library = PhotoLibrary::new(
                "https://synthetic.example/database/1/photos/production/private".into(),
                Arc::new(std::collections::HashMap::new()),
                Box::new(session),
                Arc::new(zone()),
                "private".into(),
                crate::retry::RetryConfig::default(),
            )
            .await
            .unwrap();
            library.shadow_capture = Some(crate::icloud::photos::inbox::ShadowCapture::new(
                db.clone(),
                baseline_owner(),
                "com",
            ));
            let key = sync_token_key(ZONE);
            let passes = vec![AlbumPass {
                kind: PassKind::Unfiled,
                album: library.all(),
                exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
            }];
            let mut selected = make_run_cycle_library_state_with_passes(ZONE, &key, passes);
            selected.library = library;
            let builder = make_run_cycle_download_config_builder(root, db.clone());
            run_cycle(
                &[&selected],
                config,
                Some(db.as_ref()),
                false,
                &builder,
                download::DownloadControls::download_hidden(),
                shared,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
            drop(db);
        }
    };

    run(Arc::clone(&full_queries), Arc::clone(&changes)).await;
    {
        let db = SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap();
        let conn = db.acquire_lock("verify staged promotion").unwrap();
        let active: Option<String> = conn
            .query_row(
                "SELECT value FROM metadata WHERE key=?1",
                [&active_key],
                |row| row.get(0),
            )
            .ok();
        assert!(
            active.is_some(),
            "the exact staged token must become active"
        );
        let pending_present: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM metadata WHERE key=?1)",
                [&pending_key],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            !pending_present,
            "promotion must consume the staged token: {pending_key}"
        );
        let record = epoch_record(&conn, ZONE).expect("epoch record");
        assert_eq!(record.state, "complete");
        assert!(
            record.binding.is_none(),
            "promotion must consume the staged binding"
        );
        assert_eq!(full_queries.load(Ordering::SeqCst), 1);
    }
    run(Arc::clone(&full_queries), Arc::clone(&changes)).await;
    assert_eq!(
        full_queries.load(Ordering::SeqCst),
        1,
        "a quiet follow-up must not repeat the epoch baseline"
    );
    assert!(changes.load(Ordering::SeqCst) > 0);
}

#[tokio::test]
async fn pending_enum_promotion_binds_exact_staged_proof() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    std::fs::create_dir_all(&root).unwrap();
    let db_path = dir.path().join("state.db");
    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    {
        let conn = db
            .acquire_lock("seed pending enum promotion state")
            .unwrap();
        clear_test_epoch(&conn);
        conn.execute(
            "INSERT INTO source_revalidation_rounds(id,library,scope,enum_hash,boundary,high_id,after_id,phase) VALUES('old-round',?1,'scope','policy','boundary','high','after','active')",
            [ZONE],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO source_revalidation_debt(round,id) VALUES('old-round','asset-reset')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO metadata(key,value) VALUES(?1,'old-token')",
            [format!("sync_token:v27:{ZONE}")],
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO metadata(key,value) VALUES('enum_config_hash','old-enum')",
            [],
        )
        .unwrap();
        conn.execute_batch("PRAGMA user_version=42").unwrap();
    }
    drop(db);

    let config = make_run_cycle_config();
    let current_enum = download::compute_config_hash(&config);
    let pending_key = format!("pending_sync_token:v27:{current_enum}:{ZONE}");
    let active_key = format!("sync_token:v27:{ZONE}");
    let full_queries = Arc::new(AtomicUsize::new(0));
    let changes = Arc::new(AtomicUsize::new(0));
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;

    let run = |cycle: usize| {
        let full_queries = Arc::clone(&full_queries);
        let changes = Arc::clone(&changes);
        let config = &config;
        let root = &root;
        let shared = &shared;
        let db_path = &db_path;
        async move {
            let db = Arc::new(
                SqliteStateDb::open_owned(db_path, &baseline_owner())
                    .await
                    .unwrap(),
            );
            let session = ResetCycleSession {
                zone: ZONE,
                inventory: Arc::new(Vec::new()),
                full_queries,
                changes,
                fail_full: false,
                auth_expired: false,
                boundary: BOUNDARY,
            };
            let mut library = PhotoLibrary::new(
                "https://synthetic.example/database/1/photos/production/private".into(),
                Arc::new(std::collections::HashMap::new()),
                Box::new(session),
                Arc::new(zone()),
                "private".into(),
                crate::retry::RetryConfig::default(),
            )
            .await
            .unwrap();
            library.shadow_capture = Some(crate::icloud::photos::inbox::ShadowCapture::new(
                db.clone(),
                baseline_owner(),
                "com",
            ));
            let active_key = sync_token_key(ZONE);
            let passes = vec![AlbumPass {
                kind: PassKind::Unfiled,
                album: library.all(),
                exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
            }];
            let mut selected = make_run_cycle_library_state_with_passes(ZONE, &active_key, passes);
            selected.library = library;
            let builder = make_run_cycle_download_config_builder(root, db.clone());
            let _ = run_cycle(
                &[&selected],
                config,
                Some(db.as_ref()),
                false,
                &builder,
                download::DownloadControls::download_hidden(),
                shared,
                &CancellationToken::new(),
            )
            .await;
            drop(db);
            let _ = cycle;
        }
    };

    run(0).await;
    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    {
        let conn = db.acquire_lock("verify one-cycle promotion").unwrap();
        let active: Option<String> = conn
            .query_row(
                "SELECT value FROM metadata WHERE key=?1",
                [&active_key],
                |row| row.get(0),
            )
            .ok();
        assert_eq!(
            active.as_deref(),
            Some("reset-after-0"),
            "the exact staged token must become active"
        );
        let pending_present: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM metadata WHERE key=?1)",
                [&pending_key],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            !pending_present,
            "promotion must consume the staged reset token: {pending_key}"
        );
        let record = epoch_record(&conn, ZONE).expect("epoch record");
        assert_eq!(record.state, "complete");
        assert!(
            record.binding.is_none(),
            "promotion must consume the staged reset binding"
        );
        let active_rounds: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM source_revalidation_rounds WHERE library=?1 AND phase='active'",
                [ZONE],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(active_rounds, 1);
    }
    drop(db);
    run(1).await;
    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    let conn = db.acquire_lock("verify promoted proof").unwrap();
    let active: Option<String> = conn
        .query_row(
            "SELECT value FROM metadata WHERE key=?1",
            [&active_key],
            |row| row.get(0),
        )
        .ok();
    assert!(
        active.is_some(),
        "a later cycle must retain an active checkpoint"
    );
    let pending_present: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM metadata WHERE key=?1)",
            [&pending_key],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        !pending_present,
        "promotion must consume the staged reset token: {pending_key}"
    );
    let record = epoch_record(&conn, ZONE).expect("epoch record");
    assert_eq!(record.state, "complete");
    assert!(
        record.binding.is_none(),
        "promotion must consume the staged reset binding"
    );
    let active_rounds: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM source_revalidation_rounds WHERE library=?1 AND phase='active'",
            [ZONE],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(active_rounds, 1);
    assert_eq!(
        full_queries.load(Ordering::SeqCst),
        1,
        "promotion must reuse the completed baseline"
    );
}

#[tokio::test]
async fn reset_promotion_abort_preserves_evidence_after_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    std::fs::create_dir_all(&root).unwrap();
    let db_path = dir.path().join("state.db");
    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    {
        let conn = db.acquire_lock("seed reset promotion abort").unwrap();
        clear_test_epoch(&conn);
        conn.execute(
            "INSERT INTO source_revalidation_rounds(id,library,scope,enum_hash,boundary,high_id,after_id,phase) VALUES('old-round',?1,'scope','policy','boundary','high','after','active')",
            [ZONE],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO source_revalidation_debt(round,id) VALUES('old-round','asset-reset')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO metadata(key,value) VALUES(?1,'old-token')",
            [format!("sync_token:v27:{ZONE}")],
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO metadata(key,value) VALUES('enum_config_hash','old-enum')",
            [],
        )
        .unwrap();
        conn.execute_batch("PRAGMA user_version=42").unwrap();
    }
    drop(db);

    let config = make_run_cycle_config();
    let current_enum = download::compute_config_hash(&config);
    let pending_key = format!("pending_sync_token:v27:{current_enum}:{ZONE}");
    let active_key = format!("sync_token:v27:{ZONE}");
    let full_queries = Arc::new(AtomicUsize::new(0));
    let changes = Arc::new(AtomicUsize::new(0));
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let run = |full_queries: Arc<AtomicUsize>, changes: Arc<AtomicUsize>| {
        let config = &config;
        let root = &root;
        let shared = &shared;
        let db_path = &db_path;
        async move {
            let db = Arc::new(
                SqliteStateDb::open_owned(db_path, &baseline_owner())
                    .await
                    .unwrap(),
            );
            let session = ResetCycleSession {
                zone: ZONE,
                inventory: Arc::new(Vec::new()),
                full_queries,
                changes,
                fail_full: false,
                auth_expired: false,
                boundary: BOUNDARY,
            };
            let mut library = PhotoLibrary::new(
                "https://synthetic.example/database/1/photos/production/private".into(),
                Arc::new(std::collections::HashMap::new()),
                Box::new(session),
                Arc::new(zone()),
                "private".into(),
                crate::retry::RetryConfig::default(),
            )
            .await
            .unwrap();
            library.shadow_capture = Some(crate::icloud::photos::inbox::ShadowCapture::new(
                db.clone(),
                baseline_owner(),
                "com",
            ));
            let key = sync_token_key(ZONE);
            let passes = vec![AlbumPass {
                kind: PassKind::Unfiled,
                album: library.all(),
                exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
            }];
            let mut selected = make_run_cycle_library_state_with_passes(ZONE, &key, passes);
            selected.library = library;
            let builder = make_run_cycle_download_config_builder(root, db.clone());
            let _ = run_cycle(
                &[&selected],
                config,
                Some(db.as_ref()),
                false,
                &builder,
                download::DownloadControls::download_hidden(),
                shared,
                &CancellationToken::new(),
            )
            .await;
            drop(db);
        }
    };
    // Fail only the promotion cleanup after the staged token/binding committed.
    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    {
        let conn = db.acquire_lock("install reset promotion abort").unwrap();
        conn.execute_batch(
            "CREATE TRIGGER fail_reset_promotion_clear BEFORE DELETE ON metadata \
             WHEN OLD.key LIKE 'baseline_cursor:v27:%' \
             BEGIN SELECT RAISE(ABORT,'synthetic reset promotion abort'); END;",
        )
        .unwrap();
    }
    drop(db);
    run(Arc::clone(&full_queries), Arc::clone(&changes)).await;
    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    {
        let conn = db.acquire_lock("inspect aborted promotion").unwrap();
        let present = |key: &str| -> bool {
            conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM metadata WHERE key=?1)",
                [key],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert!(
            !present(&active_key),
            "aborted promotion must not write the active token"
        );
        assert!(present(&pending_key));
        let record = epoch_record(&conn, ZONE).expect("ready record must remain");
        assert_eq!(record.state, "ready");
        assert!(
            record.binding.is_some(),
            "ready binding must remain after an aborted promotion"
        );
        conn.execute("DROP TRIGGER fail_reset_promotion_clear", [])
            .unwrap();
    }
    drop(db);
    run(full_queries, changes).await;
    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    let conn = db.acquire_lock("verify retry promotion").unwrap();
    let present = |key: &str| -> bool {
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM metadata WHERE key=?1)",
            [key],
            |row| row.get(0),
        )
        .unwrap()
    };
    assert!(
        present(&active_key),
        "the retry must promote after the synthetic abort is cleared"
    );
    assert!(
        !present(&pending_key),
        "promotion must consume the staged token"
    );
    let record = epoch_record(&conn, ZONE).expect("epoch record");
    assert_eq!(record.state, "complete");
    assert!(
        record.binding.is_none(),
        "promotion must clear the staged binding"
    );
    let active_rounds: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM source_revalidation_rounds WHERE library=?1 AND phase='active'",
            [ZONE],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(active_rounds, 1);
}

#[tokio::test]
async fn epoch_ready_promotion_survives_peer_stale_plan_reopen() {
    const PEER: &str = "SharedSync-epoch-review-peer";
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    std::fs::create_dir_all(&root).unwrap();
    let db_path = dir.path().join("state.db");
    {
        let stock = SqliteStateDb::create_stock_v36_for_test(&db_path, &baseline_owner());
        let conn = stock.acquire_lock("seed epoch review").unwrap();
        conn.execute(
            "DELETE FROM metadata WHERE key=?1",
            [crate::state::db::sync_epoch::TEST_PREEXISTING_EPOCH_KEY],
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO metadata(key,value) VALUES('enum_config_hash','old-enum')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO metadata(key,value) VALUES(?1,'old-zone')",
            [format!("sync_token:v27:{ZONE}")],
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO metadata(key,value) VALUES(?1,'peer-active')",
            [format!("sync_token:v27:{PEER}")],
        )
        .unwrap();
    }
    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    {
        let conn = db.acquire_lock("mark peer epoch complete").unwrap();
        let epoch = crate::state::db::sync_epoch::validate(&conn).unwrap().epoch;
        let record = json!({
            "version": 2,
            "epoch": epoch,
            "library": PEER,
            "state": "complete",
            "reason": "existing_upgrade",
            "attempt": "peer-attempt",
            "attempts": 1,
            "next_attempt_at": 0,
            "last_outcome": "complete",
            "baseline_scope": "peer-scope",
            "enrolled_at": 0,
            "completed_at": 0
        });
        conn.execute(
            "INSERT INTO metadata(key,value) VALUES(?1,?2)",
            rusqlite::params![
                crate::state::db::sync_epoch::library_key(PEER),
                record.to_string()
            ],
        )
        .unwrap();
    }
    let config = make_run_cycle_config();
    let enum_hash = download::compute_config_hash(&config);
    let pending_key = format!("pending_sync_token:v27:{enum_hash}:{ZONE}");
    let zone_full = Arc::new(AtomicUsize::new(0));
    let zone_changes = Arc::new(AtomicUsize::new(0));
    let peer_full = Arc::new(AtomicUsize::new(0));
    let peer_changes = Arc::new(AtomicUsize::new(0));
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;

    let make_states = |db: Arc<SqliteStateDb>, peer_stale: bool| {
        let zone_session = ResetCycleSession {
            zone: ZONE,
            inventory: Arc::new(Vec::new()),
            full_queries: Arc::clone(&zone_full),
            changes: Arc::clone(&zone_changes),
            fail_full: false,
            auth_expired: false,
            boundary: BOUNDARY,
        };
        let peer_session = ResetCycleSession {
            zone: PEER,
            inventory: Arc::new(Vec::new()),
            full_queries: Arc::clone(&peer_full),
            changes: Arc::clone(&peer_changes),
            fail_full: false,
            auth_expired: false,
            boundary: BOUNDARY,
        };
        async move {
            let mut zone_library = PhotoLibrary::new(
                "https://synthetic.example/database/1/photos/production/private".into(),
                Arc::new(std::collections::HashMap::new()),
                Box::new(zone_session),
                Arc::new(zone()),
                "private".into(),
                crate::retry::RetryConfig::default(),
            )
            .await
            .unwrap();
            zone_library.shadow_capture = Some(crate::icloud::photos::inbox::ShadowCapture::new(
                db.clone(),
                baseline_owner(),
                "com",
            ));
            let mut peer_library = PhotoLibrary::new(
                "https://synthetic.example/database/1/photos/production/private".into(),
                Arc::new(std::collections::HashMap::new()),
                Box::new(peer_session),
                Arc::new(json!({"zoneName":PEER,"ownerRecordName":"_defaultOwner"})),
                "private".into(),
                crate::retry::RetryConfig::default(),
            )
            .await
            .unwrap();
            peer_library.shadow_capture = Some(crate::icloud::photos::inbox::ShadowCapture::new(
                db.clone(),
                baseline_owner(),
                "com",
            ));
            let zone_passes = vec![AlbumPass {
                kind: PassKind::Unfiled,
                album: zone_library.all(),
                exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
            }];
            let peer_passes = vec![AlbumPass {
                kind: PassKind::Unfiled,
                album: peer_library.all(),
                exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
            }];
            let mut zone_state =
                make_run_cycle_library_state_with_passes(ZONE, &sync_token_key(ZONE), zone_passes);
            zone_state.library = zone_library;
            let mut peer_state =
                make_run_cycle_library_state_with_passes(PEER, &sync_token_key(PEER), peer_passes);
            peer_state.library = peer_library;
            peer_state.plan_is_stale = peer_stale;
            (zone_state, peer_state)
        }
    };

    let (zone_state, peer_state) = make_states(Arc::clone(&db), true).await;
    let builder = make_run_cycle_download_config_builder(&root, db.clone());
    let result0 = run_cycle(
        &[&zone_state, &peer_state],
        &config,
        Some(db.as_ref()),
        false,
        &builder,
        download::DownloadControls::download_hidden(),
        &shared,
        &CancellationToken::new(),
    )
    .await
    .expect("stale peer must not abort the cycle");
    assert!(
        !result0.db_sync_token_advance_safe,
        "stale peer must block shared promotion"
    );
    let staged = db.get_metadata(&pending_key).await.unwrap();
    assert!(staged.is_some(), "epoch baseline must stage a token");
    {
        let conn = db.acquire_lock("inspect blocked promotion record").unwrap();
        let record = epoch_record(&conn, ZONE).expect("ready record must remain");
        assert_eq!(record.state, "ready");
        assert!(
            record.binding.is_some(),
            "ready binding must remain after blocked promotion"
        );
    }
    assert!(
        db.get_metadata(&sync_token_key(ZONE))
            .await
            .unwrap()
            .is_none(),
        "blocked promotion must not write the active token"
    );
    drop(db);

    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    let (zone_state, peer_state) = make_states(Arc::clone(&db), false).await;
    let builder = make_run_cycle_download_config_builder(&root, db.clone());
    let (capture, _guard) = crate::test_helpers::TracingCapture::install();
    let result1 = run_cycle(
        &[&zone_state, &peer_state],
        &config,
        Some(db.as_ref()),
        false,
        &builder,
        download::DownloadControls::download_hidden(),
        &shared,
        &CancellationToken::new(),
    )
    .await
    .expect("healthy reopen must promote the ready epoch baseline");
    eprintln!(
        "P1 cycle1 zone_full={} result={result1:?} errors={:?}",
        zone_full.load(Ordering::SeqCst),
        capture
            .events()
            .iter()
            .filter(|event| event.field("error").is_some())
            .map(|event| (
                event.message().unwrap_or("").to_owned(),
                event.field("error").unwrap_or("").to_owned()
            ))
            .collect::<Vec<_>>()
    );
    assert!(result1.db_sync_token_advance_safe);
    assert_eq!(
        zone_full.load(Ordering::SeqCst),
        1,
        "promotion must not run a new epoch full baseline"
    );
    assert_eq!(
        db.get_metadata(&sync_token_key(ZONE)).await.unwrap(),
        staged,
        "the exact staged token must become active"
    );
    {
        let conn = db.acquire_lock("inspect promoted record").unwrap();
        let record = epoch_record(&conn, ZONE).expect("epoch record must remain");
        assert_eq!(record.state, "complete");
        assert!(
            record.binding.is_none(),
            "promotion must consume the staged binding"
        );
    }
    assert!(
        db.get_metadata(&pending_key).await.unwrap().is_none(),
        "promotion must consume the staged token"
    );
    let peer_active = db.get_metadata(&sync_token_key(PEER)).await.unwrap();
    assert!(
        peer_active.is_some(),
        "healthy peer must retain a checkpoint"
    );
}

async fn reset_cycle_state(
    db: Arc<SqliteStateDb>,
    zone: &'static str,
    full_queries: Arc<AtomicUsize>,
    changes: Arc<AtomicUsize>,
    auth_expired: bool,
) -> crate::sync_cycle::LibraryState {
    let session = ResetCycleSession {
        zone,
        inventory: Arc::new(Vec::new()),
        full_queries,
        changes,
        fail_full: false,
        auth_expired,
        boundary: BOUNDARY,
    };
    let mut library = PhotoLibrary::new(
        "https://synthetic.example/database/1/photos/production/private".into(),
        Arc::new(std::collections::HashMap::new()),
        Box::new(session),
        Arc::new(json!({"zoneName":zone,"ownerRecordName":"_defaultOwner"})),
        "private".into(),
        crate::retry::RetryConfig::default(),
    )
    .await
    .unwrap();
    library.shadow_capture = Some(crate::icloud::photos::inbox::ShadowCapture::new(
        db.clone(),
        baseline_owner(),
        "com",
    ));
    let key = sync_token_key(zone);
    let passes = vec![AlbumPass {
        kind: PassKind::Unfiled,
        album: library.all(),
        exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
    }];
    let mut state = make_run_cycle_library_state_with_passes(zone, &key, passes);
    state.library = library;
    state
}

/// Seed a schema-36 database whose next owned open migrates it as an
/// existing upgrade, not a fresh install.
fn seed_existing_upgrade_db(db_path: &std::path::Path) {
    let stock = SqliteStateDb::create_stock_v36_for_test(db_path, &baseline_owner());
    drop(stock);
    let conn = rusqlite::Connection::open(db_path).unwrap();
    conn.execute(
        "DELETE FROM metadata WHERE key=?1",
        [crate::state::db::sync_epoch::TEST_PREEXISTING_EPOCH_KEY],
    )
    .unwrap();
}

#[tokio::test]
async fn epoch_persistent_auth_expiry_must_not_restage_full_each_cycle() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    std::fs::create_dir_all(&root).unwrap();
    let db_path = dir.path().join("state.db");
    seed_existing_upgrade_db(&db_path);
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO metadata(key,value) VALUES(?1,'v36-token')",
            [format!("sync_token:v27:{ZONE}")],
        )
        .unwrap();
    }
    let config = make_run_cycle_config();
    let full_queries = Arc::new(AtomicUsize::new(0));
    let changes = Arc::new(AtomicUsize::new(0));
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;

    let mut last_state = None;
    for cycle in 0..3 {
        // Reopen the database for every cycle so this also covers the
        // systemd-restart shape, not only an in-process reauth loop.
        let db = Arc::new(
            SqliteStateDb::open_owned(&db_path, &baseline_owner())
                .await
                .unwrap(),
        );
        let selected = reset_cycle_state(
            Arc::clone(&db),
            ZONE,
            Arc::clone(&full_queries),
            Arc::clone(&changes),
            true,
        )
        .await;
        let builder = make_run_cycle_download_config_builder(&root, db.clone());
        let result = run_cycle(
            &[&selected],
            &config,
            Some(db.as_ref()),
            false,
            &builder,
            download::DownloadControls::download_hidden(),
            &shared,
            &CancellationToken::new(),
        )
        .await
        .expect("persistent auth failure must surface as a cycle result");
        if cycle == 0 {
            assert!(
                result.session_expired,
                "the first cycle must report the synthetic 401 as session expiry"
            );
        } else {
            assert!(
                !result.session_expired,
                "cycle {cycle} must be deferred by the bounded deadline, not repeat the provider attempt"
            );
        }
        let conn = db.acquire_lock("inspect auth retry state").unwrap();
        let record = crate::state::db::sync_epoch::read_library(&conn, ZONE)
            .unwrap()
            .unwrap();
        last_state = Some((record.attempts, record.next_attempt_at, record.state));
        drop(conn);
        drop(db);
    }
    let (attempts, next_attempt_at, state) = last_state.unwrap();
    let now = chrono::Utc::now().timestamp();
    eprintln!(
        "AUTH full_queries={} attempts={attempts} state={state} next_attempt_at={next_attempt_at} now={now}",
        full_queries.load(Ordering::SeqCst)
    );
    assert_eq!(
        full_queries.load(Ordering::SeqCst),
        1,
        "persistent auth expiry must not run a new full epoch baseline on every cycle"
    );
    assert_eq!(
        attempts, 0,
        "auth expiry must not consume a provider-failure attempt"
    );
    assert!(
        next_attempt_at > now,
        "auth expiry must not immediately rearm the next baseline"
    );
}

#[cfg(feature = "xmp")]
async fn ineligible_run_sidecar_state(
    is_retry_failed: bool,
) -> (bool, usize, Option<String>, usize, usize) {
    use crate::state::SyncTokenStore as _;
    use crate::state::types::AssetMetadata;

    const PRINT_ZONE: &str = "PrimarySync";
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    std::fs::create_dir_all(&root).unwrap();
    let db_path = dir.path().join("state.db");
    seed_existing_upgrade_db(&db_path);
    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    db.sync_epoch_enroll(vec![(PRINT_ZONE.to_owned(), "print-scope".to_owned())])
        .await
        .unwrap();
    db.set_metadata("last_recovery_action", "ineligible-sentinel")
        .await
        .unwrap();

    let media = root.join("queued.jpg");
    std::fs::write(&media, MEDIA).unwrap();
    let checksum = crate::download::file::compute_sha256(&media).await.unwrap();
    db.upsert_seen(
        &crate::test_helpers::TestAssetRecord::new("queued-asset")
            .filename("queued.jpg")
            .metadata(AssetMetadata {
                rating: Some(5),
                metadata_hash: Some("queued-hash".into()),
                ..AssetMetadata::default()
            })
            .build(),
    )
    .await
    .unwrap();
    db.mark_downloaded(
        PRINT_ZONE,
        "queued-asset",
        "original",
        &media,
        &checksum,
        None,
    )
    .await
    .unwrap();
    db.record_metadata_write_failure(PRINT_ZONE, "queued-asset", "original")
        .await
        .unwrap();
    let sidecar = root.join("queued.jpg.xmp");
    assert!(!sidecar.exists(), "fixture must start without a sidecar");

    let mut config = make_run_cycle_config();
    config.runtime.only_print_filenames = !is_retry_failed;
    config.metadata.xmp_sidecar = true;
    let full_queries = Arc::new(AtomicUsize::new(0));
    let changes = Arc::new(AtomicUsize::new(0));
    let selected = reset_cycle_state(
        Arc::clone(&db),
        PRINT_ZONE,
        Arc::clone(&full_queries),
        Arc::clone(&changes),
        false,
    )
    .await;
    let builder = make_run_cycle_download_config_builder(&root, db.clone());
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let run_mode = if is_retry_failed {
        download::DownloadRunMode::Download
    } else {
        download::DownloadRunMode::PrintFilenames
    };
    let result = run_cycle(
        &[&selected],
        &config,
        Some(db.as_ref()),
        is_retry_failed,
        &builder,
        download::DownloadControls::new(run_mode, download::DownloadReporting::hidden()),
        &shared,
        &CancellationToken::new(),
    )
    .await;
    let _ = result;
    let pending = db.get_pending_metadata_rewrites(10).await.unwrap();
    let action = db.get_metadata("last_recovery_action").await.unwrap();
    eprintln!(
        "INELIGIBLE retry_failed={is_retry_failed} sidecar_exists={} pending={} action={action:?} full_queries={} changes={}",
        sidecar.exists(),
        pending.len(),
        full_queries.load(Ordering::SeqCst),
        changes.load(Ordering::SeqCst)
    );
    (
        sidecar.exists(),
        pending.len(),
        action,
        full_queries.load(Ordering::SeqCst),
        changes.load(Ordering::SeqCst),
    )
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn epoch_print_only_defer_must_not_drain_queued_metadata_rewrites() {
    let (sidecar_exists, pending, action, full_queries, changes) =
        Box::pin(ineligible_run_sidecar_state(false)).await;
    assert!(
        !sidecar_exists,
        "print-only must not drain a queued metadata rewrite and write a sidecar"
    );
    assert!(
        pending > 0,
        "print-only must retain the queued metadata rewrite"
    );
    assert_eq!(
        action.as_deref(),
        Some("ineligible-sentinel"),
        "print-only must not write LAST_RECOVERY_ACTION"
    );
    assert_eq!(
        full_queries, 0,
        "print-only must not enumerate a full inventory"
    );
    assert_eq!(changes, 0, "print-only must not enumerate a delta");
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn epoch_retry_failed_defer_must_not_drain_queued_metadata_rewrites() {
    let (sidecar_exists, pending, action, full_queries, changes) =
        Box::pin(ineligible_run_sidecar_state(true)).await;
    assert!(
        !sidecar_exists,
        "retry-failed must not drain a queued metadata rewrite and write a sidecar"
    );
    assert!(
        pending > 0,
        "retry-failed must retain the queued metadata rewrite"
    );
    assert_eq!(
        action.as_deref(),
        Some("ineligible-sentinel"),
        "retry-failed must not write LAST_RECOVERY_ACTION"
    );
    assert_eq!(
        full_queries, 0,
        "retry-failed must not enumerate a full inventory"
    );
    assert_eq!(changes, 0, "retry-failed must not enumerate a delta");
}

async fn seed_abort_ready_state(db_path: &std::path::Path, enum_hash: &str) {
    let db = SqliteStateDb::open_owned(db_path, &baseline_owner())
        .await
        .unwrap();
    {
        let conn = db.acquire_lock("seed aborted ready state").unwrap();
        clear_test_epoch(&conn);
        conn.execute(
            "INSERT INTO source_revalidation_rounds(id,library,scope,enum_hash,boundary,high_id,after_id,phase) VALUES('old-round',?1,'scope','policy','boundary','high','after','active')",
            [ZONE],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO source_revalidation_debt(round,id) VALUES('old-round','asset-reset')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO metadata(key,value) VALUES(?1,'old-token')",
            [format!("sync_token:v27:{ZONE}")],
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO metadata(key,value) VALUES('enum_config_hash',?1)",
            [enum_hash],
        )
        .unwrap();
        conn.execute_batch("PRAGMA user_version=42").unwrap();
    }
    drop(db);
}

struct ReadyEpochEvidence {
    attempt: String,
    pending_key: String,
    baseline_key: String,
}

async fn stage_ready_epoch_with_aborted_promotion(
    db_path: &std::path::Path,
    root: &std::path::Path,
    config: &crate::config::Config,
    full_queries: Arc<AtomicUsize>,
    changes: Arc<AtomicUsize>,
) -> ReadyEpochEvidence {
    let db = Arc::new(
        SqliteStateDb::open_owned(db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    {
        let conn = db.acquire_lock("install epoch promotion abort").unwrap();
        conn.execute_batch(
            "CREATE TRIGGER fail_epoch_promotion_clear BEFORE DELETE ON metadata \
             WHEN OLD.key LIKE 'baseline_cursor:v27:%' \
             BEGIN SELECT RAISE(ABORT,'synthetic epoch promotion abort'); END;",
        )
        .unwrap();
    }
    drop(db);

    let db = Arc::new(
        SqliteStateDb::open_owned(db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    let selected = reset_cycle_state(
        Arc::clone(&db),
        ZONE,
        Arc::clone(&full_queries),
        Arc::clone(&changes),
        false,
    )
    .await;
    let builder = make_run_cycle_download_config_builder(root, db.clone());
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let _ = run_cycle(
        &[&selected],
        config,
        Some(db.as_ref()),
        false,
        &builder,
        download::DownloadControls::download_hidden(),
        &shared,
        &CancellationToken::new(),
    )
    .await;
    drop(db);

    let db = Arc::new(
        SqliteStateDb::open_owned(db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    let conn = db.acquire_lock("read ready epoch evidence").unwrap();
    let record = crate::state::db::sync_epoch::read_library(&conn, ZONE)
        .unwrap()
        .unwrap();
    let binding = record.binding.expect("ready binding must remain");
    let present = |key: &str| -> bool {
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM metadata WHERE key=?1)",
            [key],
            |row| row.get(0),
        )
        .unwrap()
    };
    assert!(
        !present(&sync_token_key(ZONE)),
        "aborted promotion must not write the active token"
    );
    assert!(present(&binding.token_key), "staged token must remain");
    assert_eq!(record.state, "ready");
    ReadyEpochEvidence {
        attempt: record.attempt,
        pending_key: binding.token_key,
        baseline_key: binding.baseline_key,
    }
}

#[tokio::test]
async fn epoch_config_revert_preserves_ready_staged_token_without_promoting() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    std::fs::create_dir_all(&root).unwrap();
    let db_path = dir.path().join("state.db");
    let reverted = make_run_cycle_config();
    let reverted_hash = download::compute_config_hash(&reverted);
    let mut staging = make_run_cycle_config();
    staging.filters.media.videos = false;
    let staging_hash = download::compute_config_hash(&staging);
    assert_ne!(reverted_hash, staging_hash);
    seed_abort_ready_state(&db_path, &reverted_hash).await;
    let full_queries = Arc::new(AtomicUsize::new(0));
    let changes = Arc::new(AtomicUsize::new(0));
    let evidence = stage_ready_epoch_with_aborted_promotion(
        &db_path,
        &root,
        &staging,
        Arc::clone(&full_queries),
        Arc::clone(&changes),
    )
    .await;
    assert_eq!(
        full_queries.load(Ordering::SeqCst),
        1,
        "the staging cycle must run exactly one epoch baseline"
    );

    // The user reverts to the previously active enumeration config while the
    // ready token, marker and proof are still staged after the aborted promotion.
    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    let stale_staged_value: String = {
        let conn = db.acquire_lock("read stale staged value").unwrap();
        conn.query_row(
            "SELECT value FROM metadata WHERE key=?1",
            [&evidence.pending_key],
            |row| row.get(0),
        )
        .unwrap()
    };
    let selected = reset_cycle_state(
        Arc::clone(&db),
        ZONE,
        Arc::clone(&full_queries),
        Arc::clone(&changes),
        false,
    )
    .await;
    let builder = make_run_cycle_download_config_builder(&root, db.clone());
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let _ = run_cycle(
        &[&selected],
        &reverted,
        Some(db.as_ref()),
        false,
        &builder,
        download::DownloadControls::download_hidden(),
        &shared,
        &CancellationToken::new(),
    )
    .await;
    let conn = db.acquire_lock("inspect reverted config state").unwrap();
    let present = |key: &str| -> bool {
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM metadata WHERE key=?1)",
            [key],
            |row| row.get(0),
        )
        .unwrap()
    };
    let record = crate::state::db::sync_epoch::read_library(&conn, ZONE)
        .unwrap()
        .unwrap();
    eprintln!(
        "REVERT staged={:?} reverted={reverted_hash} full_queries={} attempt_rotated={} pending={} binding={} baseline={}",
        staging_hash,
        full_queries.load(Ordering::SeqCst),
        record.attempt != evidence.attempt,
        present(&evidence.pending_key),
        record.binding.is_some(),
        present(&evidence.baseline_key),
    );
    assert_eq!(
        full_queries.load(Ordering::SeqCst),
        2,
        "a genuinely changed policy requires a new bounded baseline; the stale proof must not promote"
    );
    assert!(
        present(&evidence.pending_key),
        "the superseded ready token must survive the config revert as audit evidence"
    );
    assert!(
        record
            .binding
            .as_ref()
            .is_none_or(|binding| binding.token_key != evidence.pending_key),
        "a rotated attempt must not keep the superseded staged binding"
    );
    assert_ne!(
        record.attempt, evidence.attempt,
        "a genuinely changed policy must rotate the attempt nonce"
    );
    let active: Option<String> = conn
        .query_row(
            "SELECT value FROM metadata WHERE key=?1",
            [sync_token_key(ZONE)],
            |row| row.get(0),
        )
        .ok();
    assert_ne!(
        active.as_deref(),
        Some(stale_staged_value.as_str()),
        "the stale staged token must not become active"
    );
}

#[tokio::test]
async fn epoch_baseline_disappearance_is_not_consumption_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    std::fs::create_dir_all(&root).unwrap();
    let db_path = dir.path().join("state.db");
    let config = make_run_cycle_config();
    seed_abort_ready_state(&db_path, "pre-epoch-enum").await;
    let full_queries = Arc::new(AtomicUsize::new(0));
    let changes = Arc::new(AtomicUsize::new(0));
    let evidence = stage_ready_epoch_with_aborted_promotion(
        &db_path,
        &root,
        &config,
        Arc::clone(&full_queries),
        Arc::clone(&changes),
    )
    .await;

    // Simulate the non-atomic bridge/test path that removes the fresh baseline
    // key while the ready binding still exists. This is not the promotion
    // transaction, which deletes the binding and baseline together.
    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    let key = &evidence.baseline_key;
    let pending = &evidence.pending_key;
    let staged = db.get_metadata(pending).await.unwrap();
    {
        let conn = db.acquire_lock("drop injected abort").unwrap();
        conn.execute("DROP TRIGGER fail_epoch_promotion_clear", [])
            .unwrap();
        assert_eq!(
            conn.execute("DELETE FROM metadata WHERE key=?1", [key])
                .unwrap(),
            1
        );
    }
    drop(db);
    let db = SqliteStateDb::open_owned(&db_path, &baseline_owner())
        .await
        .unwrap();
    assert_eq!(db.get_metadata(key).await.unwrap(), None);
    assert!(staged.is_some());
    assert_eq!(db.get_metadata(pending).await.unwrap(), staged);
    let ready = epoch_record(&db.acquire_lock("retained ready binding").unwrap(), ZONE).unwrap();
    assert_eq!(ready.state, "ready");
    let binding = ready.binding.unwrap();
    assert_eq!(binding.baseline_key, evidence.baseline_key);
    assert_eq!(binding.token_key, evidence.pending_key);
    let gate = db
        .sync_epoch_gate(ZONE.to_owned(), "scope".to_owned(), None)
        .await
        .unwrap();
    eprintln!("CONSUMPTION baseline_present=false binding_present=true gate={gate:?}");
    assert!(
        !matches!(gate, crate::state::SyncEpochGate::Promote { .. }),
        "a missing baseline key alone must not authorize promotion"
    );
}

#[tokio::test]
async fn epoch_promotion_atomically_consumes_baseline_and_binding() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    std::fs::create_dir_all(&root).unwrap();
    let db_path = dir.path().join("state.db");
    let config = make_run_cycle_config();
    seed_abort_ready_state(&db_path, "pre-epoch-enum").await;
    let full_queries = Arc::new(AtomicUsize::new(0));
    let changes = Arc::new(AtomicUsize::new(0));
    let evidence = stage_ready_epoch_with_aborted_promotion(
        &db_path,
        &root,
        &config,
        Arc::clone(&full_queries),
        Arc::clone(&changes),
    )
    .await;

    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    {
        let conn = db.acquire_lock("clear epoch promotion abort").unwrap();
        conn.execute("DROP TRIGGER fail_epoch_promotion_clear", [])
            .unwrap();
    }
    let selected = reset_cycle_state(
        Arc::clone(&db),
        ZONE,
        Arc::clone(&full_queries),
        Arc::clone(&changes),
        false,
    )
    .await;
    let builder = make_run_cycle_download_config_builder(&root, db.clone());
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let result = run_cycle(
        &[&selected],
        &config,
        Some(db.as_ref()),
        false,
        &builder,
        download::DownloadControls::download_hidden(),
        &shared,
        &CancellationToken::new(),
    )
    .await
    .expect("healthy promotion must succeed");
    assert!(result.db_sync_token_advance_safe);
    assert_eq!(
        full_queries.load(Ordering::SeqCst),
        1,
        "promotion must reuse the completed epoch baseline"
    );
    let conn = db.acquire_lock("inspect atomic epoch consumption").unwrap();
    let present = |key: &str| -> bool {
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM metadata WHERE key=?1)",
            [key],
            |row| row.get(0),
        )
        .unwrap()
    };
    let record = crate::state::db::sync_epoch::read_library(&conn, ZONE)
        .unwrap()
        .unwrap();
    eprintln!(
        "ATOMIC attempt={} binding_present={} baseline_present={} state={}",
        evidence.attempt,
        record.binding.is_some(),
        present(&evidence.baseline_key),
        record.state
    );
    assert!(
        record.binding.is_none(),
        "promotion must consume the staged binding in the same transaction"
    );
    assert!(
        !present(&evidence.baseline_key),
        "promotion must delete the baseline boundary in the same transaction"
    );
    assert_eq!(record.state, "complete");
}

#[tokio::test]
async fn epoch_no_active_passes_ineligible_modes_do_not_write_defer() {
    use crate::state::SyncTokenStore as _;

    for (only_print, retry_failed) in [(true, false), (false, true)] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("photos");
        std::fs::create_dir_all(&root).unwrap();
        let sidecar = root.join("existing.JPG.xmp");
        std::fs::write(&sidecar, b"existing sidecar").unwrap();
        let sidecar_mtime = std::fs::metadata(&sidecar).unwrap().modified().unwrap();
        let db_path = dir.path().join("state.db");
        seed_existing_upgrade_db(&db_path);
        let db = Arc::new(
            SqliteStateDb::open_owned(&db_path, &baseline_owner())
                .await
                .unwrap(),
        );
        db.sync_epoch_enroll(vec![(ZONE.to_owned(), "scope".to_owned())])
            .await
            .unwrap();
        {
            let conn = db.acquire_lock("seed no-active-pass epoch").unwrap();
            conn.execute(
                "INSERT OR REPLACE INTO metadata(key,value) VALUES(?1,'existing-token')",
                [format!("sync_token:v27:{ZONE}")],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO owned_temp_files(path,claimed_at) VALUES(x'02',0)",
                [],
            )
            .unwrap();
            conn.execute(
                "UPDATE metadata SET value=json_set(value,'$.next_attempt_at',0,'$.last_outcome','seeded') WHERE key=?1",
                [crate::state::db::sync_epoch::library_key(ZONE)],
            )
            .unwrap();
        }
        let full_queries = Arc::new(AtomicUsize::new(0));
        let changes = Arc::new(AtomicUsize::new(0));
        let mut selected = reset_cycle_state(
            Arc::clone(&db),
            ZONE,
            Arc::clone(&full_queries),
            Arc::clone(&changes),
            false,
        )
        .await;
        selected.plan.passes.clear();
        let mut config = make_run_cycle_config();
        config.runtime.only_print_filenames = only_print;
        let run_mode = if only_print {
            download::DownloadRunMode::PrintFilenames
        } else {
            download::DownloadRunMode::Download
        };
        let builder = make_run_cycle_download_config_builder(&root, db.clone());
        let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
        let result = run_cycle(
            &[&selected],
            &config,
            Some(db.as_ref()),
            retry_failed,
            &builder,
            download::DownloadControls::new(run_mode, download::DownloadReporting::hidden()),
            &shared,
            &CancellationToken::new(),
        )
        .await
        .expect("ineligible no-active-pass cycle must not fail");
        let conn = db.acquire_lock("inspect no-active-pass epoch").unwrap();
        let record = crate::state::db::sync_epoch::read_library(&conn, ZONE)
            .unwrap()
            .unwrap();
        let sidecar_after = std::fs::read(&sidecar).unwrap();
        let sidecar_mtime_after = std::fs::metadata(&sidecar).unwrap().modified().unwrap();
        let temp_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM owned_temp_files", [], |row| {
                row.get(0)
            })
            .unwrap();
        eprintln!(
            "NO_PASSES only_print={only_print} retry={retry_failed} full={} changes={} record_next={} record_outcome={:?} token={:?} sidecar_unchanged={} temp={}",
            full_queries.load(Ordering::SeqCst),
            changes.load(Ordering::SeqCst),
            record.next_attempt_at,
            record.last_outcome,
            conn.query_row(
                "SELECT value FROM metadata WHERE key=?1",
                [format!("sync_token:v27:{ZONE}")],
                |row| row.get::<_, String>(0)
            )
            .ok(),
            sidecar_after == b"existing sidecar" && sidecar_mtime_after == sidecar_mtime,
            temp_count,
        );
        assert_eq!(result.failed_count, 0);
        assert_eq!(full_queries.load(Ordering::SeqCst), 0);
        assert_eq!(changes.load(Ordering::SeqCst), 0);
        assert_eq!(record.next_attempt_at, 0);
        assert_eq!(record.last_outcome.as_deref(), Some("seeded"));
        assert_eq!(
            conn.query_row(
                "SELECT value FROM metadata WHERE key=?1",
                [format!("sync_token:v27:{ZONE}")],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
            "existing-token"
        );
        assert_eq!(sidecar_after, b"existing sidecar");
        assert_eq!(sidecar_mtime_after, sidecar_mtime);
        assert_eq!(temp_count, 1);
    }
}

#[derive(Clone)]
struct FlakyBytes {
    calls: Arc<AtomicUsize>,
    bytes: Arc<Vec<u8>>,
    fail_first: usize,
}

impl Respond for FlakyBytes {
    fn respond(&self, _request: &wiremock::Request) -> ResponseTemplate {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call < self.fail_first {
            ResponseTemplate::new(404)
        } else {
            ResponseTemplate::new(200).set_body_bytes(self.bytes.as_slice())
        }
    }
}

#[tokio::test]
async fn genuine_v36_epoch_failed_transfer_receipt_reopen_and_pending_replay() {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    std::fs::create_dir_all(&root).unwrap();
    let db_path = dir.path().join("state.db");
    seed_existing_upgrade_db(&db_path);
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO metadata(key,value) VALUES(?1,'v36-token')",
            [format!("sync_token:v27:{ZONE}")],
        )
        .unwrap();
    }

    let ok_calls = Arc::new(AtomicUsize::new(0));
    let fail_calls = Arc::new(AtomicUsize::new(0));
    let server = wiremock::MockServer::start().await;
    let fail_media = {
        let mut bytes = MEDIA.to_vec();
        bytes.extend_from_slice(b"distinct failed-transfer payload");
        bytes
    };
    Mock::given(method("GET"))
        .and(path("/ok"))
        .respond_with(FlakyBytes {
            calls: Arc::clone(&ok_calls),
            bytes: Arc::new(MEDIA.to_vec()),
            fail_first: 0,
        })
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/fail"))
        .respond_with(FlakyBytes {
            calls: Arc::clone(&fail_calls),
            bytes: Arc::new(fail_media.clone()),
            fail_first: 2,
        })
        .mount(&server)
        .await;

    let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(MEDIA));
    let fail_checksum_provider =
        base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&fail_media));
    let mut ok_page = full_album_page_with_download(
        ZONE,
        "ok",
        "inventory-last-page",
        &format!("{}/ok", server.uri()),
        MEDIA.len() as u64,
        &checksum,
    );
    let mut fail_page = full_album_page_with_download(
        ZONE,
        "fail",
        "inventory-last-page",
        &format!("{}/fail", server.uri()),
        fail_media.len() as u64,
        &fail_checksum_provider,
    );
    ok_page["records"][0]["fields"]["filenameEnc"]["value"] =
        serde_json::json!(base64::engine::general_purpose::STANDARD.encode("ok.JPG"));
    fail_page["records"][0]["fields"]["filenameEnc"]["value"] =
        serde_json::json!(base64::engine::general_purpose::STANDARD.encode("fail.JPG"));
    ok_page["records"][0]["recordChangeTag"] = serde_json::json!("ct-master-ok");
    ok_page["records"][1]["recordChangeTag"] = serde_json::json!("ct-asset-ok");
    fail_page["records"][0]["recordChangeTag"] = serde_json::json!("ct-master-fail");
    fail_page["records"][1]["recordChangeTag"] = serde_json::json!("ct-asset-fail");
    let mut inventory = ok_page["records"].as_array().unwrap().clone();
    inventory.extend(fail_page["records"].as_array().unwrap().clone());

    let full_queries = Arc::new(AtomicUsize::new(0));
    let changes = Arc::new(AtomicUsize::new(0));
    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    let session = ResetCycleSession {
        zone: ZONE,
        inventory: Arc::new(inventory.clone()),
        full_queries: Arc::clone(&full_queries),
        changes: Arc::clone(&changes),
        fail_full: false,
        auth_expired: false,
        boundary: "inventory-last-page",
    };
    let mut library = PhotoLibrary::new(
        "https://synthetic.example/database/1/photos/production/private".into(),
        Arc::new(std::collections::HashMap::new()),
        Box::new(session),
        Arc::new(zone()),
        "private".into(),
        crate::retry::RetryConfig::default(),
    )
    .await
    .unwrap();
    library.shadow_capture = Some(crate::icloud::photos::inbox::ShadowCapture::new(
        db.clone(),
        baseline_owner(),
        "com",
    ));
    let passes = vec![AlbumPass {
        kind: PassKind::Unfiled,
        album: library.all(),
        exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
    }];
    let mut selected =
        make_run_cycle_library_state_with_passes(ZONE, &sync_token_key(ZONE), passes);
    selected.library = library;
    let builder = make_run_cycle_download_config_builder(&root, db.clone());
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let cycle1 = run_cycle(
        &[&selected],
        &make_run_cycle_config(),
        Some(db.as_ref()),
        false,
        &builder,
        download::DownloadControls::download_hidden(),
        &shared,
        &CancellationToken::new(),
    )
    .await
    .expect("epoch baseline must complete with one failed transfer");
    assert_eq!(
        full_queries.load(Ordering::SeqCst),
        1,
        "the existing user must run exactly one epoch baseline"
    );
    assert_eq!(ok_calls.load(Ordering::SeqCst), 1, "{cycle1:?}");
    assert!(fail_calls.load(Ordering::SeqCst) >= 1, "{cycle1:?}");
    let (ok_path, ok_checksum, ok_bytes) = {
        let conn = db.acquire_lock("read successful receipt").unwrap();
        let (path, checksum): (String, String) = conn
            .query_row(
                "SELECT local_path,local_checksum FROM asset_metadata_paths WHERE library=?1 AND id='asset-ok' AND version_size='original'",
                [ZONE],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let bytes = std::fs::read(&path).unwrap();
        (path, checksum, bytes)
    };
    assert_eq!(ok_bytes, MEDIA);
    assert_eq!(std::fs::read(&ok_path).unwrap(), MEDIA);
    let failed_pending = {
        let conn = db.acquire_lock("read admitted failed transfer").unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM provider_active_destinations d JOIN provider_active_generations g ON g.id=d.generation WHERE d.child='asset-fail' AND d.admitted=1 AND d.verified_media=0",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap()
    };
    assert!(
        failed_pending > 0,
        "failed transfer must be durably admitted"
    );
    db.make_selection_retry_due(baseline_owner(), "asset-fail".to_owned())
        .await
        .unwrap();
    drop(db);

    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    let session = ResetCycleSession {
        zone: ZONE,
        inventory: Arc::new(inventory),
        full_queries: Arc::clone(&full_queries),
        changes: Arc::clone(&changes),
        fail_full: false,
        auth_expired: false,
        boundary: "inventory-last-page",
    };
    let mut library = PhotoLibrary::new(
        "https://synthetic.example/database/1/photos/production/private".into(),
        Arc::new(std::collections::HashMap::new()),
        Box::new(session),
        Arc::new(zone()),
        "private".into(),
        crate::retry::RetryConfig::default(),
    )
    .await
    .unwrap();
    library.shadow_capture = Some(crate::icloud::photos::inbox::ShadowCapture::new(
        db.clone(),
        baseline_owner(),
        "com",
    ));
    let passes = vec![AlbumPass {
        kind: PassKind::Unfiled,
        album: library.all(),
        exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
    }];
    let mut selected =
        make_run_cycle_library_state_with_passes(ZONE, &sync_token_key(ZONE), passes);
    selected.library = library;
    let builder = make_run_cycle_download_config_builder(&root, db.clone());
    let cycle2 = run_cycle(
        &[&selected],
        &make_run_cycle_config(),
        Some(db.as_ref()),
        false,
        &builder,
        download::DownloadControls::download_hidden(),
        &shared,
        &CancellationToken::new(),
    )
    .await
    .expect("reopen replay must complete");
    assert_eq!(
        full_queries.load(Ordering::SeqCst),
        1,
        "reopen replay must not run a second epoch baseline"
    );
    assert_eq!(
        ok_calls.load(Ordering::SeqCst),
        1,
        "the successful receipt must not be redownloaded: {cycle2:?}"
    );
    assert!(
        fail_calls.load(Ordering::SeqCst) >= 3,
        "the pending failed asset must be replayed against the provider: fail_calls={} {cycle2:?}",
        fail_calls.load(Ordering::SeqCst)
    );
    let conn = db.acquire_lock("read replayed failed receipt").unwrap();
    let (fail_path, fail_checksum): (String, String) = conn
        .query_row(
            "SELECT local_path,local_checksum FROM asset_metadata_paths WHERE library=?1 AND id='asset-fail' AND version_size='original'",
            [ZONE],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(std::fs::read(&fail_path).unwrap(), fail_media);
    let expected_fail_checksum = format!("{:x}", Sha256::digest(&fail_media));
    assert_eq!(fail_checksum, expected_fail_checksum);
    assert_ne!(ok_checksum, fail_checksum);
    assert_ne!(
        ok_path, fail_path,
        "the replay must target its own pending path"
    );
    let (destination, verified_media, verified_metadata): (String, i64, i64) = conn
        .query_row(
            "SELECT path,verified_media,verified_metadata FROM provider_active_destinations WHERE library=?1 AND child='asset-fail' AND version_size='original'",
            [ZONE],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    let destination_path = serde_json::from_str::<serde_json::Value>(&destination)
        .unwrap()
        .get("path")
        .and_then(serde_json::Value::as_str)
        .unwrap()
        .to_owned();
    assert_eq!(
        destination_path, fail_path,
        "the receipt must describe the admitted destination"
    );
    assert_eq!(verified_media, 1, "the replayed transfer must be verified");
    assert_eq!(
        verified_metadata, 1,
        "the replayed transfer must finish metadata"
    );
    assert!(
        cycle2.stats.downloaded >= 1,
        "the pending transfer must be a real download, not a local skip: {cycle2:?}"
    );
    assert_eq!(cycle1.failed_count, 1);
    let record = crate::state::db::sync_epoch::read_library(&conn, ZONE)
        .unwrap()
        .unwrap();
    assert_eq!(record.state, "complete");
}

#[path = "v027_baseline/epoch_pending.rs"]
mod epoch_pending;
#[path = "v027_baseline/legacy_scale.rs"]
mod legacy_scale;
#[path = "v027_baseline/operational_holds.rs"]
mod operational_holds;
#[path = "v027_baseline/operational_holds_mixed.rs"]
mod operational_holds_mixed;
#[path = "v027_baseline/preservation_hold_owner.rs"]
mod preservation_hold_owner;
pub(super) mod released_schema;
