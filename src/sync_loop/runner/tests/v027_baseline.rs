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
use wiremock::{Mock, ResponseTemplate};

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

#[cfg(feature = "xmp")]
#[tokio::test]
async fn v027_baseline_preserves_queued_existing_metadata_while_new_files_get_sidecars() {
    for database in ["private", "shared"] {
        baseline_contract(database, true, false, false, false, false, false).await;
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
    for (endpoint, expected) in [("/existing", 0), ("/missing", 1), ("/concurrent", 1)] {
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
            .expect(if retained_operation { 0 } else { expected })
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
    for cycle in 0..if retained_operation { 1 } else { 3 } {
        current_cycle.store(cycle, Ordering::SeqCst);
        if initial_failure && cycle == 1 {
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
            let operation = download::prepared_layout_in_library(
                &db,
                &retained_root,
                "retained-independent.JPG",
                ZONE,
            )
            .await;
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
            if recorded_only::enabled() && cycle == 2 {
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
            assert_eq!(
                result.stats.exif_failures, 0,
                "normal sync must not attempt and fail historical metadata work: {result:?}"
            );
            for name in ["missing.JPG.xmp", "concurrent.JPG.xmp"] {
                let output = std::fs::read_to_string(root.join(name)).unwrap();
                assert!(
                    output.contains("xmpmeta") && output.contains("rdf:RDF"),
                    "new download must publish an XMP packet: {name}"
                );
            }
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
            assert!(
                !missing_root.join("existing.JPG").exists(),
                "template changes must retain the verified historical path"
            );
        }
        if let Some(expected) = &original_current_receipts {
            assert!(
                current_content_receipts(&db_path) == *expected,
                "same-zone configuration drift must keep current path and content receipts unchanged"
            );
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
                if initial_failure { 1 } else { 2 },
                "only genuinely missing inventory+concurrent content downloads"
            );
        } else {
            assert_eq!(
                session.inventory_queries.load(Ordering::SeqCst),
                0,
                "quiet reopened cycles must remain incremental"
            );
            assert_eq!(
                result.stats.downloaded,
                usize::from((initial_failure || add_after_baseline) && cycle == 1)
            );
        }
        assert_eq!(
            session.changes.load(Ordering::SeqCst),
            if cycle == 0 { 2 } else { 1 }
        );
        if initial_failure && cycle == 0 {
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
        assert!(
            !root.join("existing.JPG").exists(),
            "renamed receipt must not acquire a duplicate"
        );
        if initial_failure && cycle == 0 {
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
                6
            } else if initial_failure && cycle == 0 {
                2
            } else {
                3 + usize::from(add_after_baseline && cycle > 0)
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
            let retained: bool = db
                .acquire_lock("assert durable failed transfer retry")
                .unwrap()
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM provider_active_decisions d
                     JOIN provider_active_destinations w ON w.generation=d.generation
                       AND w.pass_key=d.pass_key AND w.child=d.child
                     WHERE d.child='asset-current-missing' AND d.next_retry_at>0
                       AND w.admitted=1 AND w.verified_media=0)",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(
                retained,
                "failed transfer must retain its admitted destination and retry deadline"
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
        db.set_metadata(&format!("sync_token:{ZONE}"), OLD_TOKEN)
            .await
            .unwrap();
        let provider = InvalidInventory {
            record,
            base: BaselineSession {
                database: "private",
                cycle: 0,
                inventory: Arc::new(Vec::new()),
                concurrent: Arc::new(Vec::new()),
                incremental_addition: Arc::new(Vec::new()),
                requests: Arc::new(Mutex::new(Vec::new())),
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
        assert_eq!(std::fs::read(&old).unwrap(), MEDIA);
        assert_eq!(std::fs::metadata(&old).unwrap().modified().unwrap(), mtime);
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

mod configuration_safety;
