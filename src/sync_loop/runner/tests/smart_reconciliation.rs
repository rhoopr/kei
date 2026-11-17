//! Durable smart-folder path configuration transitions through the production cycle.
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::commands::{AlbumPass, PassKind};
use crate::icloud::photos::PhotosSession;
use crate::sync_cycle::{ENUM_CONFIG_HASH_KEY, PENDING_DOWNLOAD_CONFIG_HASH_KEY, run_cycle};
use crate::sync_loop::test_support::{
    album_count_response, full_album_page_with_download, make_run_cycle_config,
    make_run_cycle_download_config_builder, make_run_cycle_library_state_with_passes,
    make_shared_session_for_run_cycle,
};
use crate::{download, state};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
struct SmartReconciliationSession {
    zone: &'static str,
    records: Arc<Value>,
    extra_records: Arc<std::sync::Mutex<Vec<Value>>>,
    lookups: Arc<AtomicUsize>,
    lookup_names: Arc<std::sync::Mutex<Vec<String>>>,
    queries: Arc<AtomicUsize>,
    incomplete_pages: Arc<AtomicUsize>,
    delta_calls: Arc<AtomicUsize>,
    fault: Arc<std::sync::Mutex<Option<SmartReconciliationFault>>>,
    cancel: Arc<std::sync::Mutex<Option<CancellationToken>>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SmartReconciliationFault {
    UnknownCatalogIdentity,
    MalformedQuery,
    MalformedTail,
    MissingQueryToken,
    StalePlan,
    Interrupted,
    LateInterrupted,
    CheckpointWrite,
    PromotionWrite,
    DownloadWrite,
    IncompleteAlbumSnapshot,
    OtherLibraryFailure,
    HistoricalSelectionDebt,
}

impl SmartReconciliationSession {
    fn current_records(&self) -> Vec<Value> {
        let mut records = self.records["records"].as_array().unwrap().clone();
        records.extend(self.extra_records.lock().unwrap().clone());
        let malformed = match *self.fault.lock().unwrap() {
            Some(SmartReconciliationFault::MalformedQuery) => Some("asset-SMART"),
            Some(SmartReconciliationFault::MalformedTail) => Some("asset-EXCLUDED"),
            _ => None,
        };
        for record in &mut records {
            if malformed.is_some_and(|id| record["recordName"] == id) {
                record["fields"]["assetDate"]["value"] = Value::Null;
            }
        }
        records
    }
}
#[async_trait::async_trait]
impl PhotosSession for SmartReconciliationSession {
    async fn post(
        &self,
        url: &str,
        body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        if url.contains("/records/lookup?") {
            self.lookups.fetch_add(1, Ordering::SeqCst);
            let request: Value = serde_json::from_str(&body)?;
            self.lookup_names.lock().unwrap().extend(
                request["records"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|record| record["recordName"].as_str().unwrap().to_owned()),
            );
            if *self.fault.lock().unwrap() == Some(SmartReconciliationFault::UnknownCatalogIdentity)
            {
                return Ok(json!({"records": []}));
            }
            let request: Value = serde_json::from_str(&body)?;
            let mut records = self.current_records();
            records.retain(|record| {
                request["records"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|requested| requested["recordName"] == record["recordName"])
            });
            return Ok(json!({"records": records}));
        }
        if url.contains("/records/query/batch?") {
            return Ok(album_count_response(
                self.current_records().len() as u64 / 2,
            ));
        }
        if url.contains("/records/query?") {
            self.queries.fetch_add(1, Ordering::SeqCst);
            let late_cancel =
                *self.fault.lock().unwrap() == Some(SmartReconciliationFault::LateInterrupted);
            if let Some(cancel) = self.cancel.lock().unwrap().as_ref()
                && (!late_cancel
                    || serde_json::from_str::<Value>(&body).unwrap()["query"]["filterBy"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|filter| {
                            filter["fieldName"] == "startRank"
                                && filter["fieldValue"]["value"].as_u64().unwrap() > 0
                        }))
            {
                cancel.cancel();
            }
            if late_cancel
                && self
                    .cancel
                    .lock()
                    .unwrap()
                    .as_ref()
                    .is_some_and(CancellationToken::is_cancelled)
            {
                // Only cancellation/stream drop can finish this cycle. The
                // provider never releases the outstanding tail request.
                std::future::pending::<()>().await;
            }
            let request: Value = serde_json::from_str(&body)?;
            let offset = request["query"]["filterBy"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|filter| filter["fieldName"] == "startRank")
                .and_then(|filter| filter["fieldValue"]["value"].as_u64())
                .unwrap_or(0);
            let mut response = json!({"records": if offset == 0 { self.current_records() } else { Vec::new() }, "syncToken": "smart-current"});
            match *self.fault.lock().unwrap() {
                Some(SmartReconciliationFault::IncompleteAlbumSnapshot)
                    if body.contains("incomplete-album") =>
                {
                    // Count remains positive, but this selected album yields no
                    // members and no completion token. Missing query tokens
                    // alone do not invalidate otherwise usable selected assets.
                    response["records"] = json!([]);
                    response.as_object_mut().unwrap().remove("syncToken");
                    self.incomplete_pages.fetch_add(1, Ordering::SeqCst);
                }
                Some(SmartReconciliationFault::MissingQueryToken) => {
                    response.as_object_mut().unwrap().remove("syncToken");
                }
                Some(SmartReconciliationFault::OtherLibraryFailure)
                    if self.zone == "SharedSync-TEST" =>
                {
                    response.as_object_mut().unwrap().remove("syncToken");
                }
                _ => {}
            }
            return Ok(response);
        }
        if url.contains("/changes/zone?") {
            self.delta_calls.fetch_add(1, Ordering::SeqCst);
            let request: Value = serde_json::from_str(&body)?;
            let records = if request["zones"][0]["syncToken"].is_null() {
                self.current_records()
            } else {
                Vec::new()
            };
            return Ok(
                json!({"zones": [{"zoneID": {"zoneName": self.zone, "ownerRecordName": "_defaultOwner"}, "syncToken": "smart-current", "moreComing": false, "records": records}]}),
            );
        }
        anyhow::bail!("unexpected synthetic Photos request: {url}")
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

#[tokio::test]
async fn run_cycle_smart_reconciliation_promotes_after_fresh_query_and_stays_quiet() {
    Box::pin(smart_reconciliation_lifecycle("Hidden", false, false, None)).await;
}

#[tokio::test]
async fn run_cycle_current_debt_commits_successor_and_configuration_with_unknown_history() {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::{
        Mock, ResponseTemplate,
        matchers::{method, path},
    };

    fn evidence(db: &state::SqliteStateDb, hash: &str) -> Vec<Vec<Vec<rusqlite::types::Value>>> {
        let conn = db.acquire_lock("historical selection evidence").unwrap();
        ["provider_active_generations", "provider_active_decisions", "provider_active_sources", "provider_selection_rank_pages"].iter().map(|table| {
            let sql = if *table == "provider_active_generations" {
                "SELECT * FROM provider_active_generations WHERE config_hash=?1 ORDER BY id".to_owned()
            } else {
                format!("SELECT d.* FROM {table} d JOIN provider_active_generations g ON g.id=d.generation WHERE g.config_hash=?1 ORDER BY 1,2,3")
            };
            let mut stmt = conn.prepare(&sql).unwrap();
            let columns = stmt.column_count();
            stmt.query_map([hash], |row| (0..columns).map(|i| row.get(i)).collect()).unwrap().collect::<Result<Vec<_>,_>>().unwrap()
        }).collect()
    }

    let server = crate::start_wiremock_or_skip!();
    let bytes = include_bytes!("../../../../tests/data/media/pattern.jpg");
    let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(bytes));
    Mock::given(method("GET"))
        .and(path("/current.jpg"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.as_slice()))
        .expect(1)
        .mount(&server)
        .await;
    let root = tempfile::tempdir().unwrap();
    let db_path = root.path().join("state.db");
    let media = root.path().join("media");
    std::fs::create_dir(&media).unwrap();
    let config = make_run_cycle_config();
    let owner = state::db::account::AccountOwner::authenticated(
        "runner-debt@example.invalid",
        "com",
        &serde_json::from_value(json!({"dsInfo":{"dsid":"runner-debt-provider"}})).unwrap(),
    )
    .unwrap();
    let (_session_dir, session) = make_shared_session_for_run_cycle().await;
    let names = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut old_hash = String::new();
    let mut old_evidence = None;
    let mut completed_media = None;
    for cycle in 0..4 {
        let db = Arc::new(
            state::SqliteStateDb::open_owned(&db_path, &owner)
                .await
                .unwrap(),
        );
        let mut records = full_album_page_with_download(
            "PrimarySync",
            if cycle == 0 { "HISTORY" } else { "CURRENT" },
            "smart-current",
            &format!("{}/current.jpg", server.uri()),
            bytes.len() as u64,
            &checksum,
        );
        records["records"][1]["fields"]["isHidden"] = json!({"value":1,"type":"INT64"});
        records["records"][1]["fields"]["masterRef"]["value"]["zoneID"]["ownerRecordName"] =
            json!("_defaultOwner");
        let provider = SmartReconciliationSession {
            zone: "PrimarySync",
            records: Arc::new(records),
            extra_records: Arc::default(),
            lookups: Arc::default(),
            lookup_names: names.clone(),
            queries: Arc::default(),
            incomplete_pages: Arc::default(),
            delta_calls: Arc::default(),
            fault: Arc::new(std::sync::Mutex::new(if cycle == 0 {
                Some(SmartReconciliationFault::UnknownCatalogIdentity)
            } else {
                None
            })),
            cancel: Arc::default(),
        };
        let mut album = smart_album("PrimarySync", "Hidden", Box::new(provider));
        album.set_shadow_capture(
            crate::icloud::photos::inbox::ShadowCapture::new(db.clone(), owner.clone(), "com"),
            Arc::from("private"),
        );
        let (_, scope, _) = album.owned_selection_scope().unwrap().unwrap();
        let lib = make_run_cycle_library_state_with_passes(
            "PrimarySync",
            "sync_token:PrimarySync",
            vec![AlbumPass {
                kind: PassKind::SmartFolder,
                album,
                exclude_ids: Arc::default(),
            }],
        );
        let base = make_run_cycle_download_config_builder(&media, db.clone());
        let builder = |mode, excluded, groups, library| {
            let mut result = (*base(mode, excluded, groups, library)).clone();
            result.folder_structure_smart_folders = Arc::from(if cycle == 0 { "A" } else { "B" });
            Arc::new(result)
        };
        let hash = download::hash_download_config(&builder(
            download::SyncMode::Full,
            Arc::default(),
            Arc::default(),
            Arc::from("PrimarySync"),
        ));
        let before = names.lock().unwrap().len();
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
        if cycle == 0 {
            old_hash = db.acquire_lock("source-bound failed configuration").unwrap()
                .query_row("SELECT DISTINCT g.config_hash FROM provider_active_generations g JOIN provider_active_decisions d ON d.generation=g.id WHERE d.child='asset-HISTORY' AND d.admission='deferred'", [], |row| row.get(0)).unwrap();
            assert!(!result.can_advance_database_checkpoint(), "{result:?}");
            assert!(
                db.get_metadata("sync_token:PrimarySync")
                    .await
                    .unwrap()
                    .is_none()
            );
            assert!(
                db.unfinished_selection_debt(owner.clone(), scope, old_hash.clone())
                    .await
                    .unwrap()
            );
            assert!(
                names
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|name| name == "asset-HISTORY")
            );
            let captured = evidence(&db, &old_hash);
            assert!(captured.iter().all(|rows| !rows.is_empty()));
            old_evidence = Some(captured);
            assert_eq!(server.received_requests().await.unwrap().len(), 0);
            continue;
        }
        assert!(
            result.can_advance_database_checkpoint(),
            "cycle {cycle}: {result:?}"
        );
        assert!(!result.stats.sync_token_blocked, "{result:?}");
        assert_eq!(result.failed_count, 0);
        assert!(
            names.lock().unwrap()[before..]
                .iter()
                .all(|name| name != "asset-HISTORY" && name != "HISTORY")
        );
        if cycle > 1 {
            // Smart-folder cycles reconfirm current membership. Only B's exact
            // child and paired master may be looked up; A remains untouched.
            let mut requested = names.lock().unwrap()[before..].to_vec();
            requested.sort();
            assert_eq!(requested, ["CURRENT", "asset-CURRENT", "asset-CURRENT"]);
            assert_eq!(result.stats.downloaded, 0);
        } else {
            assert_eq!(result.stats.downloaded, 1);
        }
        let reopened = state::SqliteStateDb::open_owned(&db_path, &owner)
            .await
            .unwrap();
        assert_eq!(
            reopened
                .get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("smart-current")
        );
        assert_eq!(
            reopened
                .get_metadata(download::DOWNLOAD_CONFIG_HASH_KEY)
                .await
                .unwrap()
                .as_deref(),
            Some(hash.as_str())
        );
        assert_eq!(
            reopened
                .get_metadata(ENUM_CONFIG_HASH_KEY)
                .await
                .unwrap()
                .as_deref(),
            Some(download::compute_config_hash(&config).as_str())
        );
        assert!(
            reopened
                .get_metadata(PENDING_DOWNLOAD_CONFIG_HASH_KEY)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            reopened
                .get_metadata(crate::sync_cycle::PENDING_ENUM_CONFIG_HASH_KEY)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            reopened
                .unfinished_selection_debt(owner.clone(), scope.clone(), old_hash.clone())
                .await
                .unwrap()
        );
        let current_selection_hash = reopened.acquire_lock("completed current configuration").unwrap()
            .query_row("SELECT DISTINCT g.config_hash FROM provider_active_generations g JOIN provider_active_decisions d ON d.generation=g.id WHERE d.child='asset-CURRENT' AND g.config_hash<>?1", [&old_hash], |row| row.get(0)).unwrap();
        assert!(
            !reopened
                .unfinished_selection_debt(owner.clone(), scope, current_selection_hash)
                .await
                .unwrap()
        );
        assert_eq!(Some(evidence(&reopened, &old_hash)), old_evidence);
        let files = media_snapshot(&media);
        if let Some(expected) = &completed_media {
            assert_eq!(&files, expected);
        }
        completed_media = Some(files);
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}

#[tokio::test]
async fn run_cycle_smart_reconciliation_favorites_mixed_selection_stays_quiet() {
    Box::pin(smart_reconciliation_lifecycle(
        "Favorites",
        true,
        false,
        None,
    ))
    .await;
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn run_cycle_smart_reconciliation_sidecars_survive_transition_and_restart() {
    Box::pin(smart_reconciliation_lifecycle("Hidden", false, true, None)).await;
}

#[tokio::test]
async fn run_cycle_smart_reconciliation_other_empty_library_failure_holds_promotion() {
    Box::pin(smart_reconciliation_lifecycle(
        "Hidden",
        false,
        false,
        Some(SmartReconciliationFault::OtherLibraryFailure),
    ))
    .await;
}

#[tokio::test]
async fn run_cycle_smart_reconciliation_retains_incomplete_work_then_recovers() {
    for fault in [
        SmartReconciliationFault::UnknownCatalogIdentity,
        SmartReconciliationFault::MalformedQuery,
        SmartReconciliationFault::MissingQueryToken,
        SmartReconciliationFault::StalePlan,
        SmartReconciliationFault::Interrupted,
        SmartReconciliationFault::LateInterrupted,
        SmartReconciliationFault::CheckpointWrite,
        SmartReconciliationFault::PromotionWrite,
        SmartReconciliationFault::DownloadWrite,
        SmartReconciliationFault::IncompleteAlbumSnapshot,
    ] {
        Box::pin(smart_reconciliation_lifecycle(
            "Hidden",
            false,
            false,
            Some(fault),
        ))
        .await;
    }
}

async fn smart_reconciliation_lifecycle(
    smart_name: &str,
    mixed: bool,
    sidecars: bool,
    failure: Option<SmartReconciliationFault>,
) {
    Box::pin(smart_reconciliation_lifecycle_bounded(
        smart_name, mixed, sidecars, failure, None,
    ))
    .await;
}

#[derive(Clone, Copy, Debug)]
enum SmartBound {
    Date,
    ExistingDate,
}

#[tokio::test]
async fn run_cycle_existing_date_bound_smart_path_drift_keeps_source_hold() {
    for failure in [None, Some(SmartReconciliationFault::MissingQueryToken)] {
        Box::pin(smart_reconciliation_lifecycle_bounded(
            "Hidden",
            false,
            false,
            failure,
            Some(SmartBound::ExistingDate),
        ))
        .await;
    }
}

#[tokio::test]
async fn run_cycle_bounded_smart_reconciliation_converges_without_advancing_source() {
    for bound in [SmartBound::Date] {
        Box::pin(smart_reconciliation_lifecycle_bounded(
            "Hidden",
            false,
            false,
            None,
            Some(bound),
        ))
        .await;
    }
}

#[tokio::test]
async fn run_cycle_bounded_smart_reconciliation_failures_retain_then_recover() {
    for failure in [
        SmartReconciliationFault::MalformedQuery,
        SmartReconciliationFault::MalformedTail,
        SmartReconciliationFault::MissingQueryToken,
        SmartReconciliationFault::StalePlan,
        SmartReconciliationFault::Interrupted,
        SmartReconciliationFault::LateInterrupted,
        SmartReconciliationFault::CheckpointWrite,
        SmartReconciliationFault::PromotionWrite,
        SmartReconciliationFault::DownloadWrite,
        SmartReconciliationFault::OtherLibraryFailure,
    ] {
        Box::pin(smart_reconciliation_lifecycle_bounded(
            "Hidden",
            false,
            false,
            Some(failure),
            Some(SmartBound::Date),
        ))
        .await;
    }
}

#[tokio::test]
async fn run_cycle_bounded_smart_reconciliation_historical_selection_debt_preserves_current_promotion()
 {
    Box::pin(smart_reconciliation_lifecycle_bounded(
        "Hidden",
        false,
        false,
        Some(SmartReconciliationFault::HistoricalSelectionDebt),
        Some(SmartBound::Date),
    ))
    .await;
}

async fn smart_reconciliation_lifecycle_bounded(
    smart_name: &str,
    mixed: bool,
    sidecars: bool,
    failure: Option<SmartReconciliationFault>,
    bound: Option<SmartBound>,
) {
    Box::pin(smart_reconciliation_lifecycle_with_capture(
        smart_name,
        mixed,
        sidecars,
        failure,
        bound,
        CaptureMode::ExistingPublications,
    ))
    .await;
}

enum CaptureMode {
    ExistingPublications,
    NewPublication,
}

#[tokio::test]
async fn run_cycle_captured_smart_folder_retains_destination_after_template_change_and_restart() {
    Box::pin(smart_reconciliation_lifecycle_with_capture(
        "Favorites",
        false,
        false,
        None,
        None,
        CaptureMode::NewPublication,
    ))
    .await;
}

async fn smart_reconciliation_lifecycle_with_capture(
    smart_name: &str,
    mixed: bool,
    sidecars: bool,
    failure: Option<SmartReconciliationFault>,
    bound: Option<SmartBound>,
    capture_mode: CaptureMode,
) {
    let add_new_asset = matches!(capture_mode, CaptureMode::NewPublication)
        || failure == Some(SmartReconciliationFault::DownloadWrite);
    let (capture, _guard) = crate::test_helpers::TracingCapture::install();
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};
    let server = crate::start_wiremock_or_skip!();
    let bytes = include_bytes!("../../../../tests/data/media/pattern.jpg");
    let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(bytes));
    Mock::given(method("GET"))
        .and(path("/smart.jpg"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.as_slice()))
        .mount(&server)
        .await;
    if add_new_asset {
        Mock::given(method("GET"))
            .and(path("/new.jpg"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.as_slice()))
            .expect(1)
            .mount(&server)
            .await;
    }
    let root = tempfile::tempdir().unwrap();
    let db_path = root.path().join("state.db");
    let media = root.path().join("media");
    std::fs::create_dir(&media).unwrap();
    let mut config = make_run_cycle_config();
    let lookups = Arc::new(AtomicUsize::new(0));
    let queries = Arc::new(AtomicUsize::new(0));
    let delta_calls = Arc::new(AtomicUsize::new(0));
    if matches!(bound, Some(SmartBound::ExistingDate)) {
        // Establish a real cursor with the date bound already active and no
        // excluded tail. Cycle 1 changes only paths, then adds the old tail.
        config.filters.skip_created_before = Some(crate::config::CreatedDateFilter::CaptureDate(
            chrono::NaiveDate::from_ymd_opt(2023, 1, 1).unwrap(),
        ));
    }
    let mut records = full_album_page_with_download(
        "PrimarySync",
        "SMART",
        "smart-current",
        &format!("{}/smart.jpg", server.uri()),
        bytes.len() as u64,
        &checksum,
    );
    records["records"][1]["fields"]["isHidden"] =
        json!({"value": i32::from(smart_name == "Hidden"), "type": "INT64"});
    records["records"][1]["fields"]["isFavorite"] = json!({"value": 1, "type": "INT64"});
    records["records"][1]["fields"]["masterRef"]["value"]["zoneID"]["ownerRecordName"] =
        json!("_defaultOwner");
    let fault = Arc::new(std::sync::Mutex::new(None));
    let cancel = Arc::new(std::sync::Mutex::new(None));
    let records = Arc::new(records);
    let extra_records = Arc::new(std::sync::Mutex::new(Vec::new()));
    let incomplete_pages = Arc::new(AtomicUsize::new(0));
    let primary_session = SmartReconciliationSession {
        zone: "PrimarySync",
        records: records.clone(),
        extra_records: extra_records.clone(),
        lookups: lookups.clone(),
        lookup_names: Arc::default(),
        queries: queries.clone(),
        incomplete_pages: incomplete_pages.clone(),
        delta_calls: delta_calls.clone(),
        fault: fault.clone(),
        cancel: cancel.clone(),
    };
    let album = smart_album("PrimarySync", smart_name, Box::new(primary_session.clone()));
    let mut lib = make_run_cycle_library_state_with_passes(
        "PrimarySync",
        "sync_token:PrimarySync",
        vec![AlbumPass {
            kind: PassKind::SmartFolder,
            album,
            exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
        }],
    );
    if mixed {
        lib.plan.passes.insert(
            0,
            AlbumPass {
                kind: PassKind::Unfiled,
                album: smart_album("PrimarySync", "", Box::new(primary_session.clone())),
                exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
            },
        );
    }
    let mut other_library = make_run_cycle_library_state_with_passes(
        "SharedSync-TEST",
        "sync_token:SharedSync-TEST",
        vec![AlbumPass {
            kind: PassKind::SmartFolder,
            album: smart_album(
                "SharedSync-TEST",
                smart_name,
                Box::new(SmartReconciliationSession {
                    zone: "SharedSync-TEST",
                    records: Arc::new(json!({"records": [], "syncToken": "smart-current"})),
                    extra_records: Arc::new(std::sync::Mutex::new(Vec::new())),
                    lookups: Arc::new(AtomicUsize::new(0)),
                    lookup_names: Arc::default(),
                    queries: Arc::new(AtomicUsize::new(0)),
                    incomplete_pages: Arc::new(AtomicUsize::new(0)),
                    delta_calls: Arc::new(AtomicUsize::new(0)),
                    fault: fault.clone(),
                    cancel: Arc::new(std::sync::Mutex::new(None)),
                }),
            ),
            exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
        }],
    );
    let (_session_dir, session) = make_shared_session_for_run_cycle().await;
    let initial_enum_hash = download::compute_config_hash(&config);
    let mut original_path = None;
    #[cfg(unix)]
    let mut original_identity = None;
    let mut initial_hash = String::new();
    let mut current_hash = String::new();
    let mut previous_files = std::collections::BTreeMap::new();
    let mut original_sidecar = None;
    let mut historical_manifest = None;
    let final_cycle = if failure.is_some() || add_new_asset {
        3
    } else {
        2
    };
    for cycle in 0..=final_cycle {
        let historical_debt =
            cycle > 0 && failure == Some(SmartReconciliationFault::HistoricalSelectionDebt);
        let held = cycle == 1 && failure.is_some() && !historical_debt;
        if cycle == 1
            && let Some(bound) = bound
        {
            match bound {
                SmartBound::Date | SmartBound::ExistingDate => {
                    config.filters.skip_created_before =
                        Some(crate::config::CreatedDateFilter::CaptureDate(
                            chrono::NaiveDate::from_ymd_opt(2023, 1, 1).unwrap(),
                        ))
                }
            }
            let mut excluded = full_album_page_with_download(
                "PrimarySync",
                "EXCLUDED",
                "smart-current",
                &format!("{}/excluded.jpg", server.uri()),
                bytes.len() as u64,
                &checksum,
            );
            // Count cases exercise equal-date ties. Date cases have a genuine old tail.
            if matches!(bound, SmartBound::Date | SmartBound::ExistingDate) {
                excluded["records"][1]["fields"]["assetDate"]["value"] =
                    json!(1_500_000_000_000i64);
            }
            excluded["records"][1]["fields"]["masterRef"]["value"]["zoneID"]["ownerRecordName"] =
                json!("_defaultOwner");
            excluded["records"][1]["fields"]["isHidden"] =
                json!({"value": i32::from(smart_name == "Hidden"), "type": "INT64"});
            excluded["records"][1]["fields"]["isFavorite"] = json!({"value": 1, "type": "INT64"});
            *extra_records.lock().unwrap() = excluded["records"].as_array().unwrap().clone();
        }
        if cycle == 1 && add_new_asset {
            let mut new = full_album_page_with_download(
                "PrimarySync",
                "NEW",
                "smart-current",
                &format!("{}/new.jpg", server.uri()),
                bytes.len() as u64,
                &checksum,
            );
            new["records"][0]["fields"]["filenameEnc"]["value"] =
                json!(base64::engine::general_purpose::STANDARD.encode("new.jpg"));
            new["records"][1]["fields"]["masterRef"]["value"]["zoneID"]["ownerRecordName"] =
                json!("_defaultOwner");
            new["records"][1]["fields"]["isHidden"] =
                json!({"value": i32::from(smart_name == "Hidden"), "type": "INT64"});
            new["records"][1]["fields"]["isFavorite"] = json!({"value": 1, "type": "INT64"});
            let _ = extra_records
                .lock()
                .unwrap()
                .splice(0..0, new["records"].as_array().unwrap().clone());
        }
        *fault.lock().unwrap() = if held { failure } else { None };
        let shutdown = CancellationToken::new();
        *cancel.lock().unwrap() = if held
            && matches!(
                failure,
                Some(
                    SmartReconciliationFault::Interrupted
                        | SmartReconciliationFault::LateInterrupted
                )
            ) {
            Some(shutdown.clone())
        } else {
            None
        };
        lib.plan_is_stale = held && failure == Some(SmartReconciliationFault::StalePlan);
        if held && failure == Some(SmartReconciliationFault::IncompleteAlbumSnapshot) {
            let mut album = lib.plan.passes[0].clone();
            album.kind = PassKind::Album;
            album.album = smart_album(
                "PrimarySync",
                "Incomplete",
                Box::new(primary_session.clone()),
            );
            lib.plan.passes.push(album);
        } else if cycle > 1 && failure == Some(SmartReconciliationFault::IncompleteAlbumSnapshot) {
            lib.plan.passes.retain(|pass| pass.kind != PassKind::Album);
        }
        let owner = state::db::account::AccountOwner::authenticated(
            "smart-debt@example.invalid",
            "com",
            &serde_json::from_value(json!({"dsInfo":{"dsid":"smart-debt-provider"}})).unwrap(),
        )
        .unwrap();
        let db = Arc::new(
            state::SqliteStateDb::open_owned(&db_path, &owner)
                .await
                .unwrap(),
        );
        if cycle == 2 && failure == Some(SmartReconciliationFault::UnknownCatalogIdentity) {
            // Provider recovery does not bypass the durable identity retry clock.
            db.make_selection_retry_due(owner.clone(), "asset-SMART".to_owned())
                .await
                .unwrap();
        }
        {
            for pass in lib
                .plan
                .passes
                .iter_mut()
                .chain(other_library.plan.passes.iter_mut())
            {
                pass.album.set_shadow_capture(
                    crate::icloud::photos::inbox::ShadowCapture::new(
                        db.clone(),
                        owner.clone(),
                        "com",
                    ),
                    Arc::from("private"),
                );
                assert!(pass.album.current_selection_scope().unwrap().is_some());
            }
        }
        if historical_debt && cycle == 1 {
            use state::db::provider_generations::{
                ActiveDecision, GenerationSpec, MAX_GENERATION_BYTES,
            };
            use state::db::provider_selection::{SelectionDecision, SelectionOutcome};
            let pass = &mut lib.plan.passes[0];
            pass.album.set_shadow_capture(
                crate::icloud::photos::inbox::ShadowCapture::new(db.clone(), owner.clone(), "com"),
                Arc::from("private"),
            );
            let (_, scope, zone) = pass.album.owned_selection_scope().unwrap().unwrap();
            let key = "b".repeat(64);
            let generation = db.begin_selection_generation(owner.clone(), GenerationSpec {
                format: 1, scope: scope.clone(), zone, config_hash: "a".repeat(64),
                basis: db.selection_basis(owner.clone(), scope.clone()).await.unwrap(),
                profile: json!({"coverage":"observed_selection_window","passes":[{"key":key,"scope":pass.album.selection_scope()}]}),
                metadata_enabled: false, metadata_flags: 0,
            }, MAX_GENERATION_BYTES).await.unwrap();
            let observed = pass.album.clone().with_rank_capture(&generation.id, &key);
            assert_eq!(observed.photos(None).await.unwrap().len(), 2);
            let child = records["records"][1]["recordName"]
                .as_str()
                .unwrap()
                .to_owned();
            let sources = db
                .selection_rank_sources(
                    owner.clone(),
                    generation.id.clone(),
                    key.clone(),
                    child.clone(),
                )
                .await
                .unwrap();
            assert!(!sources.is_empty());
            assert!(
                !db.project_selection_decision(
                    owner.clone(),
                    generation.id,
                    ActiveDecision {
                        decision: SelectionDecision {
                            pass_key: key,
                            child: child.clone(),
                            master: None,
                            confirmation: None,
                            media_source: None,
                            outcome: SelectionOutcome::Deferred,
                            reason: "confirmation_unavailable".into(),
                            destinations: vec![],
                        },
                        sources,
                        state_id: child,
                    },
                    vec![],
                    MAX_GENERATION_BYTES
                )
                .await
                .unwrap()
                .admitted
            );
            assert!(
                db.unfinished_selection_debt(owner.clone(), scope, generation.spec.config_hash)
                    .await
                    .unwrap()
            );
            historical_manifest = Some(db.acquire_lock("historical smart selection manifest").unwrap()
                .query_row::<Vec<u8>,_,_>("SELECT d.manifest FROM provider_active_decisions d JOIN provider_active_generations g ON g.id=d.generation WHERE g.config_hash=?1", ["a".repeat(64)], |row| row.get(0)).unwrap());
        }
        if held
            && matches!(
                failure,
                Some(
                    SmartReconciliationFault::PromotionWrite
                        | SmartReconciliationFault::DownloadWrite
                        | SmartReconciliationFault::CheckpointWrite
                )
            )
        {
            let sql = if failure == Some(SmartReconciliationFault::CheckpointWrite) {
                "CREATE TRIGGER smart_fail BEFORE UPDATE ON metadata WHEN NEW.key = 'sync_token:PrimarySync' OR NEW.key = 'last_checkpoint_status' BEGIN SELECT RAISE(FAIL, 'synthetic checkpoint failure'); END;"
            } else if failure == Some(SmartReconciliationFault::PromotionWrite) {
                "CREATE TRIGGER smart_fail BEFORE UPDATE ON metadata WHEN NEW.key = 'config_hash' BEGIN SELECT RAISE(FAIL, 'synthetic promotion failure'); END;"
            } else {
                "CREATE TRIGGER smart_fail BEFORE UPDATE ON assets WHEN NEW.id = 'asset-NEW' AND NEW.status = 'downloaded' BEGIN SELECT RAISE(FAIL, 'synthetic downloaded write failure'); END;"
            };
            rusqlite::Connection::open(&db_path)
                .unwrap()
                .execute_batch(sql)
                .unwrap();
        }
        let base = make_run_cycle_download_config_builder(&media, db.clone());
        let builder = |mode, excludes, groupings, library| {
            let mut result = (*base(mode, excludes, groupings, library)).clone();
            result.folder_structure_smart_folders = Arc::from(if cycle == 0 {
                "old/{smart-folder}"
            } else {
                "new/{smart-folder}"
            });
            #[cfg(feature = "xmp")]
            {
                result.metadata.xmp_sidecar = sidecars;
            }
            #[cfg(not(feature = "xmp"))]
            assert!(!sidecars);
            result.skip_created_before = config.filters.skip_created_before;
            Arc::new(result)
        };
        let candidate = builder(
            download::SyncMode::Full,
            Arc::new(rustc_hash::FxHashSet::default()),
            Arc::new(download::AssetGroupings::default()),
            Arc::from("PrimarySync"),
        );
        let hash = download::hash_download_config(&candidate);
        current_hash = hash.clone();
        let queries_before = queries.load(Ordering::SeqCst);
        let delta_before = delta_calls.load(Ordering::SeqCst);
        let requests_before = server.received_requests().await.unwrap().len();
        let cursor_before = db.get_metadata("sync_token:PrimarySync").await.unwrap();
        let states = if failure == Some(SmartReconciliationFault::OtherLibraryFailure) {
            vec![&lib, &other_library]
        } else {
            vec![&lib]
        };
        let cycle_future = run_cycle(
            &states,
            &config,
            Some(db.as_ref()),
            &builder,
            download::DownloadControls::download_hidden(),
            &session,
            &shutdown,
        );
        let result = if held && failure == Some(SmartReconciliationFault::DownloadWrite) {
            // Persistent finalization faults exhaust six real SQL attempts.
            // Their exponential waits exceed five seconds, so advance only
            // observed retry sleeps while keeping the wall-clock deadline.
            tokio::time::pause();
            let started = std::time::Instant::now();
            let mut retry_waits = 0;
            tokio::pin!(cycle_future);
            let result = loop {
                tokio::select! {
                    biased;
                    result = &mut cycle_future => break result,
                    () = tokio::task::yield_now() => {
                        assert!(started.elapsed() < std::time::Duration::from_secs(5),
                            "selected SQL fault cycle stalled after {retry_waits} retry waits");
                        let observed = capture.events().iter().filter(|event| {
                            event.message() == Some("State write retry failed, will retry")
                        }).count();
                        if observed > retry_waits {
                            retry_waits = observed;
                            // The largest finalization retry sleep, including jitter,
                            // is below four seconds. No pending provider is released.
                            tokio::time::advance(std::time::Duration::from_secs(4)).await;
                        }
                    }
                }
            };
            tokio::time::resume();
            assert_eq!(
                retry_waits, 5,
                "the persistent trigger must exhaust finalization retries"
            );
            result
        } else {
            tokio::time::timeout(std::time::Duration::from_secs(5), cycle_future)
                .await
                .unwrap_or_else(|_| {
                    panic!("smart reconciliation stalled: {failure:?}, cycle {cycle}")
                })
        };
        let result = match result {
            Err(error)
                if held
                    && bound.is_some()
                    && matches!(
                        failure,
                        Some(
                            SmartReconciliationFault::MalformedQuery
                                | SmartReconciliationFault::MalformedTail
                        )
                    ) =>
            {
                assert!(error.to_string().contains("Malformed"));
                assert_eq!(
                    db.get_metadata(download::DOWNLOAD_CONFIG_HASH_KEY)
                        .await
                        .unwrap()
                        .as_deref(),
                    Some(initial_hash.as_str())
                );
                assert_eq!(
                    db.get_metadata(PENDING_DOWNLOAD_CONFIG_HASH_KEY)
                        .await
                        .unwrap()
                        .as_deref(),
                    Some(hash.as_str())
                );
                assert_eq!(media_snapshot(&media), previous_files);
                continue;
            }
            result => result.unwrap(),
        };
        if held && failure == Some(SmartReconciliationFault::IncompleteAlbumSnapshot) {
            assert!(
                incomplete_pages.load(Ordering::SeqCst) > 0,
                "the selected album's empty, tokenless page must execute"
            );
            assert!(!result.can_advance_database_checkpoint());
            assert_eq!(
                server.received_requests().await.unwrap().len(),
                requests_before,
                "an empty incomplete album cannot authorize a new publication"
            );
        }
        if held && failure == Some(SmartReconciliationFault::LateInterrupted) {
            assert!(shutdown.is_cancelled());
            assert!(result.stats.interrupted, "{result:?}");
            // User interruption is its own checkpoint veto, not a provider
            // enumeration failure or an operational error.
            assert!(!result.stats.enumeration_incomplete, "{result:?}");
            assert_eq!(result.failed_count, 0, "{result:?}");
            assert!(result.stats.sync_token_blocked, "{result:?}");
            assert!(!result.can_advance_database_checkpoint());
            assert_eq!(
                db.get_metadata("sync_token:PrimarySync").await.unwrap(),
                cursor_before
            );
        }
        if !held {
            assert_eq!(
                result.failed_count, 0,
                "{failure:?} cycle {cycle}: {result:?}"
            );
            assert!(
                queries.load(Ordering::SeqCst) > queries_before,
                "current smart membership must be queried"
            );
        }
        if matches!(bound, Some(SmartBound::ExistingDate)) {
            if cycle == 0 {
                assert_eq!(
                    db.get_metadata("sync_token:PrimarySync")
                        .await
                        .unwrap()
                        .as_deref(),
                    Some("smart-current")
                );
            } else {
                assert!(
                    delta_calls.load(Ordering::SeqCst) > delta_before,
                    "path-only drift must exercise source replay"
                );
                assert!(!result.can_advance_database_checkpoint());
                assert_eq!(
                    db.get_metadata("sync_token:PrimarySync")
                        .await
                        .unwrap()
                        .as_deref(),
                    Some("smart-current")
                );
            }
        }
        let rows = db.get_downloaded_page(0, 10).await.unwrap();
        if !(held && failure == Some(SmartReconciliationFault::DownloadWrite)) {
            assert_eq!(
                rows.len(),
                1 + usize::from(add_new_asset && cycle > 0),
                "{failure:?} cycle {cycle}: {rows:?}"
            );
        }
        let current = rows
            .iter()
            .find(|row| row.id.as_ref() == "asset-SMART")
            .and_then(|row| row.local_path.as_ref())
            .cloned()
            .or_else(|| original_path.clone())
            .unwrap();
        assert_eq!(std::fs::read(&current).unwrap(), bytes);
        if cycle == 0 {
            original_path = Some(current.clone());
            initial_hash = hash.clone();
            if sidecars {
                let path = current.with_file_name(format!(
                    "{}.xmp",
                    current.file_name().unwrap().to_string_lossy()
                ));
                original_sidecar = Some((path.clone(), std::fs::read(path).unwrap()));
            }
        } else if held {
            assert_eq!(
                db.get_metadata(download::DOWNLOAD_CONFIG_HASH_KEY)
                    .await
                    .unwrap()
                    .as_deref(),
                Some(initial_hash.as_str()),
                "{failure:?}"
            );
            assert_eq!(
                db.get_metadata(PENDING_DOWNLOAD_CONFIG_HASH_KEY)
                    .await
                    .unwrap()
                    .as_deref(),
                Some(hash.as_str()),
                "{failure:?}"
            );
            assert_eq!(
                std::fs::read(original_path.as_ref().unwrap()).unwrap(),
                bytes
            );
        } else {
            assert_eq!(
                Some(&current),
                original_path.as_ref(),
                "template changes retain the existing destination"
            );
            if add_new_asset {
                let new = rows
                    .iter()
                    .find(|row| row.id.as_ref() == "asset-NEW")
                    .unwrap();
                let path = new.local_path.as_ref().unwrap();
                assert!(
                    path.starts_with(media.join("new")),
                    "new selections use the new template: {path:?}"
                );
                assert_eq!(std::fs::read(path).unwrap(), bytes);
            }
            assert_eq!(
                std::fs::read(original_path.as_ref().unwrap()).unwrap(),
                bytes
            );
            assert_eq!(
                db.get_metadata(download::DOWNLOAD_CONFIG_HASH_KEY)
                    .await
                    .unwrap()
                    .as_deref(),
                Some(hash.as_str())
            );
            assert_eq!(
                db.get_metadata(PENDING_DOWNLOAD_CONFIG_HASH_KEY)
                    .await
                    .unwrap(),
                None
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let metadata = std::fs::metadata(original_path.as_ref().unwrap()).unwrap();
            let identity = (metadata.dev(), metadata.ino());
            if let Some(original) = original_identity {
                assert_eq!(
                    identity, original,
                    "retained publication must not be replaced"
                );
            } else {
                original_identity = Some(identity);
            }
        }
        if let Some((path, bytes)) = &original_sidecar {
            assert_eq!(std::fs::read(path).unwrap(), *bytes);
        }
        let files = media_snapshot(&media);
        if cycle > 0 && !add_new_asset {
            assert_eq!(
                files, previous_files,
                "path drift must not copy or rewrite retained publications"
            );
        }
        if historical_debt {
            assert_eq!(
                result.stats.sync_token_blocked_reason,
                Some("date_bounded_full_enumeration")
            );
            assert!(!result.can_advance_database_checkpoint());
            let (_, scope, _) = lib.plan.passes[0]
                .album
                .owned_selection_scope()
                .unwrap()
                .unwrap();
            assert!(
                db.unfinished_selection_debt(owner.clone(), scope.clone(), "a".repeat(64))
                    .await
                    .unwrap()
            );
            let reopened = state::SqliteStateDb::open_owned(&db_path, &owner)
                .await
                .unwrap();
            assert_eq!(
                reopened
                    .get_metadata(download::DOWNLOAD_CONFIG_HASH_KEY)
                    .await
                    .unwrap()
                    .as_deref(),
                Some(hash.as_str())
            );
            assert_eq!(
                reopened
                    .get_metadata(PENDING_DOWNLOAD_CONFIG_HASH_KEY)
                    .await
                    .unwrap()
                    .as_deref(),
                None
            );
            assert_eq!(
                reopened
                    .get_metadata("sync_token:PrimarySync")
                    .await
                    .unwrap()
                    .as_deref(),
                Some("smart-current")
            );
            assert!(
                reopened
                    .unfinished_selection_debt(owner, scope, "a".repeat(64))
                    .await
                    .unwrap()
            );
            let retained = reopened.acquire_lock("unchanged historical smart selection manifest").unwrap()
                .query_row::<Vec<u8>,_,_>("SELECT d.manifest FROM provider_active_decisions d JOIN provider_active_generations g ON g.id=d.generation WHERE g.config_hash=?1 AND d.admission='deferred'", ["a".repeat(64)], |row| row.get(0)).unwrap();
            assert_eq!(Some(retained), historical_manifest);
            assert_eq!(
                std::fs::read(original_path.as_ref().unwrap()).unwrap(),
                bytes
            );
        }
        if cycle == final_cycle {
            assert_eq!(
                result.stats.downloaded, 0,
                "unchanged final cycle: {failure:?}"
            );
            assert_eq!(
                files, previous_files,
                "no duplicate or temporary files: {failure:?}"
            );
            assert_eq!(
                server.received_requests().await.unwrap().len(),
                requests_before
            );
        }
        if bound.is_some()
            && !matches!(bound, Some(SmartBound::ExistingDate))
            && cycle == final_cycle
        {
            assert_eq!(
                result.stats.full_enumeration_reason,
                Some(download::FullEnumerationReason::EnumConfigHashDrift)
            );
        }
        previous_files = files;
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("smart-current")
        );
        if bound.is_some() && cycle > 0 {
            assert!(
                !result.can_advance_database_checkpoint(),
                "bounded source remains held: bound={bound:?} cycle={cycle} failure={failure:?} result={result:?}"
            );
            let expected_pending_enum = if matches!(bound, Some(SmartBound::ExistingDate)) {
                None
            } else {
                Some(download::compute_config_hash(&config))
            };
            assert_eq!(
                db.get_metadata(crate::sync_cycle::PENDING_ENUM_CONFIG_HASH_KEY)
                    .await
                    .unwrap(),
                expected_pending_enum
            );
        }
        assert_eq!(
            db.get_metadata(ENUM_CONFIG_HASH_KEY)
                .await
                .unwrap()
                .as_deref(),
            Some(initial_enum_hash.as_str())
        );
        if held
            && matches!(
                failure,
                Some(
                    SmartReconciliationFault::PromotionWrite
                        | SmartReconciliationFault::DownloadWrite
                        | SmartReconciliationFault::CheckpointWrite
                )
            )
        {
            let message = match failure.unwrap() {
                SmartReconciliationFault::PromotionWrite => "synthetic promotion failure",
                SmartReconciliationFault::DownloadWrite => "synthetic downloaded write failure",
                SmartReconciliationFault::CheckpointWrite => "synthetic checkpoint failure",
                other => panic!("expected an injected state-write fault, got {other:?}"),
            };
            assert!(
                capture.events().iter().any(|event| event
                    .field("error")
                    .is_some_and(|error| error.contains(message))),
                "native fault must execute: {failure:?}"
            );
            assert!(
                result.failed_count > 0,
                "executed SQL failure must affect operational health: {failure:?}: {result:?}"
            );
            if failure == Some(SmartReconciliationFault::PromotionWrite) {
                assert_eq!(result.stats.state_write_failures, 1, "{result:?}");
                assert_eq!(
                    result.failed_count, 1,
                    "one failed promotion transaction: {result:?}"
                );
                assert!(!result.can_advance_database_checkpoint());
            }
            rusqlite::Connection::open(&db_path)
                .unwrap()
                .execute_batch("DROP TRIGGER smart_fail;")
                .unwrap();
        }
        eprintln!(
            "#770 acceptance case={failure:?} smart={smart_name} mixed={mixed} cycle={cycle} failed={} downloaded={} lookups={} queries={} active_hash_old={} pending={} reason={:?}",
            result.failed_count,
            result.stats.downloaded,
            lookups.load(Ordering::SeqCst),
            queries.load(Ordering::SeqCst),
            db.get_metadata(download::DOWNLOAD_CONFIG_HASH_KEY)
                .await
                .unwrap()
                .as_deref()
                == Some(initial_hash.as_str()),
            db.get_metadata(PENDING_DOWNLOAD_CONFIG_HASH_KEY)
                .await
                .unwrap()
                .is_some(),
            result.stats.full_enumeration_reason
        );
    }
    let owner = state::db::account::AccountOwner::authenticated(
        "smart-debt@example.invalid",
        "com",
        &serde_json::from_value(json!({"dsInfo":{"dsid":"smart-debt-provider"}})).unwrap(),
    )
    .unwrap();
    let reopened = state::SqliteStateDb::open_owned(&db_path, &owner)
        .await
        .unwrap();
    assert_eq!(
        reopened
            .get_metadata(download::DOWNLOAD_CONFIG_HASH_KEY)
            .await
            .unwrap()
            .as_deref(),
        Some(current_hash.as_str())
    );
    assert_eq!(
        reopened
            .get_metadata(PENDING_DOWNLOAD_CONFIG_HASH_KEY)
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        reopened.get_downloaded_page(0, 10).await.unwrap().len(),
        1 + usize::from(add_new_asset)
    );
    assert!(reopened.get_pending().await.unwrap().is_empty());
    // Each selected destination transfers once. Template drift never schedules
    // copies of the old file, including after a native finalization fault.
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        if mixed { 2 } else { 1 } + usize::from(add_new_asset)
    );
    server.verify().await;
}

fn media_snapshot(
    root: &std::path::Path,
) -> std::collections::BTreeMap<std::path::PathBuf, Vec<u8>> {
    fn visit(
        root: &std::path::Path,
        result: &mut std::collections::BTreeMap<std::path::PathBuf, Vec<u8>>,
    ) {
        for entry in std::fs::read_dir(root).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                visit(&path, result);
            } else {
                result.insert(path.clone(), std::fs::read(path).unwrap());
            }
        }
    }
    let mut result = std::collections::BTreeMap::new();
    visit(root, &mut result);
    result
}

fn smart_album(
    zone: &str,
    name: &str,
    session: Box<dyn PhotosSession>,
) -> crate::icloud::photos::PhotoAlbum {
    let (list_type, obj_type, query_filter, container_id) = if name.is_empty() {
        (
            Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
            Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
            None,
            None,
        )
    } else if name == "Incomplete" {
        (
            Arc::from("CPLContainerRelationLiveByAssetDate"),
            Arc::from("CPLContainerRelationNotDeletedByAssetDate:incomplete-album"),
            Some(Arc::new(
                json!([{"fieldName":"parentId","comparator":"EQUALS","fieldValue":{"type":"STRING","value":"incomplete-album"}}]),
            )),
            Some(Arc::from("incomplete-album")),
        )
    } else {
        let def = crate::icloud::photos::smart_folders::smart_folders()
            .into_iter()
            .find(|(candidate, _)| *candidate == name)
            .unwrap()
            .1;
        (
            Arc::from(def.list_type),
            Arc::from(def.obj_type),
            def.query_filter,
            None,
        )
    };
    crate::icloud::photos::PhotoAlbum::new(
        crate::icloud::photos::PhotoAlbumConfig {
            params: Arc::new(std::collections::HashMap::new()),
            service_endpoint: Arc::from("https://example.com"),
            name: Arc::from(name),
            list_type,
            obj_type,
            query_filter,
            page_size: 100,
            zone_id: Arc::new(json!({"zoneName": zone, "ownerRecordName": "_defaultOwner"})),
            retry_config: crate::retry::RetryConfig::default(),
            container_id,
            cross_zone_sources: Vec::new(),
        },
        session,
    )
}
