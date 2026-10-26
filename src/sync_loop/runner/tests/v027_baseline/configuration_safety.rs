//! Normal configuration expansion must preserve prior files and pending identities.
use super::{MEDIA, ZONE, baseline_owner, pair, zone};
use crate::commands::{AlbumPass, PassKind};
use crate::download;
use crate::icloud::photos::{PhotoLibrary, PhotosSession};
use crate::state::SparseIdentityStore;
use crate::state::SqliteStateDb;
use crate::sync_cycle::{run_cycle, sync_token_key};
use crate::sync_loop::test_support::{
    album_count_response, make_run_cycle_config, make_run_cycle_download_config_builder,
    make_run_cycle_library_state_with_passes, make_shared_session_for_run_cycle,
};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

#[derive(Clone)]
struct Session {
    records: Arc<Vec<Value>>,
    cycle: usize,
    first_observation: usize,
    interrupted_inventory: Option<std::path::PathBuf>,
    bridge_fault: Option<bool>,
    unresolved: bool,
    observed_historical: bool,
    omit_after_first: bool,
    plain_source: bool,
    resolve_source: bool,
    unrepresented: bool,
    inventories: Arc<AtomicUsize>,
    omissions: Arc<AtomicUsize>,
}
#[async_trait::async_trait]
impl PhotosSession for Session {
    async fn post(
        &self,
        url: &str,
        body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        let request: Value = serde_json::from_str(&body)?;
        if url.contains("/zones/list") {
            return Ok(
                json!({"zones":[{"zoneID":zone(),"syncToken":"expansion-boundary","deleted":false}]}),
            );
        }
        if request["query"]["recordType"] == "CheckIndexingState" {
            return Ok(
                json!({"records":[{"recordName":"index","recordType":"CheckIndexingState","fields":{"state":{"value":"FINISHED"}}}]}),
            );
        }
        if url.contains("/internal/records/query/batch") {
            return Ok(album_count_response(2));
        }
        if url.contains("/records/query?") {
            self.inventories.fetch_add(1, Ordering::SeqCst);
            let offset = request["query"]["filterBy"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|f| f["fieldName"] == "startRank")
                .and_then(|f| f["fieldValue"]["value"].as_u64())
                .unwrap_or(0) as usize;
            if offset > 0
                && let Some(published) = &self.interrupted_inventory
            {
                tokio::time::timeout(std::time::Duration::from_secs(10), async {
                    while !published.exists() {
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                })
                .await
                .expect("partial baseline must publish before injected page failure");
                self.omissions.fetch_add(1, Ordering::SeqCst);
                return Ok(json!({"records":"malformed later inventory page"}));
            }
            let records: Vec<_> = self
                .records
                .chunks(2)
                .skip(offset)
                .take(if self.interrupted_inventory.is_some() {
                    1
                } else {
                    usize::MAX
                })
                .flat_map(|p| p.iter().cloned())
                .collect();
            return Ok(json!({"records":records,"syncToken":"query-not-checkpoint"}));
        }
        if url.contains("/records/lookup?") {
            let requested = request["records"].as_array().unwrap();
            let records: Vec<_> = self
                .records
                .iter()
                .filter(|record| {
                    let selected = requested
                        .iter()
                        .any(|r| r["recordName"] == record["recordName"]);
                    if selected && self.unresolved && record["recordName"] == "asset-january" {
                        self.omissions.fetch_add(1, Ordering::SeqCst);
                        false
                    } else {
                        selected
                    }
                })
                .cloned()
                .collect();
            return Ok(json!({"records":records}));
        }
        if url.contains("/changes/zone?") {
            assert_eq!(request["zones"][0]["zoneID"], zone());
            let records = if self.resolve_source && self.cycle == 2 {
                vec![
                    json!({"recordName":"historical-source","recordType":null,"deleted":true,"recordChangeTag":"deleted-current"}),
                ]
            } else if self.observed_historical
                && self.cycle >= self.first_observation
                && (!self.omit_after_first || self.cycle == self.first_observation)
                && (!self.resolve_source || self.cycle == 1)
            {
                let mut record = crate::test_helpers::sparse_shared_asset_record();
                record["recordName"] = json!("historical-source");
                record["fields"]["linkedShareRecordName"]["value"] = json!("historical-child");
                record["fields"]["linkedShareZoneName"]["value"] = json!("SharedSync-absent");
                record["fields"]["linkedShareZoneOwner"]["value"]["recordName"] =
                    json!("historical-owner");
                if self.plain_source {
                    record = json!({"recordName":"historical-source","recordType":"CPLAsset","fields":{}});
                }
                if self.unrepresented {
                    record["recordName"] = json!("new-unrepresented-source");
                    if !self.plain_source {
                        record["fields"]["linkedShareRecordName"]["value"] = json!(42);
                    }
                }
                vec![record]
            } else {
                Vec::new()
            };
            let mut response = json!({"zones":[{"zoneID":zone(),"records":records,
                "syncToken":format!("expanded-after-{}",self.cycle),"moreComing":false}]});
            if let Some(blank) = self.bridge_fault {
                assert_eq!(request["zones"][0]["syncToken"], "expansion-boundary");
                assert!(self.inventories.load(Ordering::SeqCst) > 0);
                self.omissions.fetch_add(1, Ordering::SeqCst);
                if blank {
                    response["zones"][0]["syncToken"] = json!("");
                } else {
                    response["zones"][0]
                        .as_object_mut()
                        .unwrap()
                        .remove("syncToken");
                }
            }
            return Ok(response);
        }
        anyhow::bail!("unexpected expansion fixture endpoint: {url}");
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

#[tokio::test]
async fn v027_date_expansion_downloads_new_selection_then_two_quiet_reopens() {
    expansion(false, History::None, false, false, false, false, false).await;
}
#[tokio::test]
async fn v027_date_expansion_unconfirmed_identity_is_accounted_then_recovers() {
    expansion(true, History::None, false, false, false, false, false).await;
}
#[tokio::test]
async fn v027_date_expansion_preserves_historical_uncertainty_without_global_hold() {
    expansion(false, History::Linked, false, false, false, false, false).await;
}

#[tokio::test]
async fn v027_current_sparse_source_observed_again_holds_current_cursor() {
    expansion(false, History::None, true, false, false, false, false).await;
}
#[tokio::test]
async fn v027_current_sparse_source_omitted_on_replay_stays_pending() {
    expansion(false, History::None, true, true, false, false, false).await;
}
#[tokio::test]
async fn v027_historical_asset_only_observation_then_omission_retains_current_work() {
    expansion(false, History::Linked, true, true, true, false, false).await;
}
#[tokio::test]
#[tracing_test::traced_test]
async fn v027_mixed_history_current_deletion_resolves_then_reopens_quietly() {
    expansion(false, History::Linked, true, true, true, true, false).await;
}
#[tokio::test]
async fn v027_new_plain_source_with_unrelated_history_survives_omitted_reopen() {
    expansion(false, History::Linked, true, true, true, false, true).await;
}
#[tokio::test]
async fn v027_malformed_new_source_with_history_survives_omitted_reopen() {
    expansion(false, History::Linked, true, true, false, false, true).await;
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum History {
    None,
    Linked,
    MarkerOnly,
    Malformed,
    InterruptedMigration,
    InterruptedBaseline,
    InterruptedInventory,
    MissingBridge,
    BlankBridge,
    CurrentBaseline,
}

#[tokio::test]
async fn v027_stock_marker_only_baseline_expansion_and_quiet_reopens() {
    expansion(
        false,
        History::MarkerOnly,
        false,
        false,
        false,
        false,
        false,
    )
    .await;
}
#[tokio::test]
async fn v027_stock_malformed_history_baseline_expansion_and_quiet_reopens() {
    expansion(false, History::Malformed, false, false, false, false, false).await;
}
#[tokio::test]
async fn v027_stock_malformed_same_source_current_omission_stays_pending() {
    expansion(false, History::Malformed, true, true, true, false, false).await;
}

#[tokio::test]
async fn v027_stock_migration_interruption_then_baseline_and_quiet_reopens() {
    expansion(
        false,
        History::InterruptedMigration,
        false,
        false,
        false,
        false,
        false,
    )
    .await;
}
#[tokio::test]
async fn v027_stock_initial_checkpoint_failure_reopens_without_duplicate_transfer() {
    expansion(
        false,
        History::InterruptedBaseline,
        false,
        false,
        false,
        false,
        false,
    )
    .await;
}
#[tokio::test]
async fn v027_stock_current_unknown_during_first_baseline_survives_omission() {
    expansion(
        false,
        History::CurrentBaseline,
        true,
        true,
        true,
        false,
        false,
    )
    .await;
}

#[tokio::test]
async fn v027_stock_partial_initial_inventory_preserves_admission_then_reopens() {
    expansion(
        false,
        History::InterruptedInventory,
        false,
        false,
        false,
        false,
        false,
    )
    .await;
}

#[tokio::test]
async fn v027_stock_missing_or_blank_bridge_successor_preserves_work_then_reopens() {
    for history in [History::MissingBridge, History::BlankBridge] {
        expansion(false, history, false, false, false, false, false).await;
    }
}

fn snapshot_history(path: &std::path::Path) -> Vec<Vec<rusqlite::types::Value>> {
    let conn = rusqlite::Connection::open(path).unwrap();
    let has_epoch: bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('unresolved_sparse_identities') WHERE name='epoch')", [], |r|r.get(0)).unwrap();
    let query = "SELECT library,source_record_name,original_evidence,observed_evidence,lookup_evidence,generation,first_seen_at,last_attempt_at,attempts,next_retry_at,last_outcome FROM unresolved_sparse_identities WHERE source_record_name IN ('historical-source','other-history') ORDER BY library,source_record_name";
    let query = if has_epoch {
        query.replace(
            "WHERE source_record_name",
            "WHERE epoch=0 AND source_record_name",
        )
    } else {
        query.to_owned()
    };
    let mut statement = conn.prepare(&query).unwrap();
    statement
        .query_map([], |row| (0..11).map(|i| row.get(i)).collect())
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}
fn seed_stock_history(path: &std::path::Path, history: History, extra: bool) {
    let conn = rusqlite::Connection::open(path).unwrap();
    crate::state::schema::migrate_genuine_v36(&conn).unwrap();
    conn.execute_batch("CREATE TABLE account_owner(singleton INTEGER PRIMARY KEY CHECK(singleton=1),version INTEGER NOT NULL,account_key TEXT NOT NULL,provider_key TEXT NOT NULL);").unwrap();
    conn.execute(
        "INSERT INTO account_owner VALUES(1,1,?1,?2)",
        rusqlite::params![
            crate::account::namespace("baseline@example.invalid", "com"),
            crate::account::provider_fingerprint("com", "synthetic-baseline")
        ],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO metadata(key,value) VALUES(?1,'opaque original marker')",
        [format!("unresolved_asset_identity:{ZONE}")],
    )
    .unwrap();
    if history != History::MarkerOnly {
        for source in if extra {
            vec!["historical-source", "other-history"]
        } else {
            vec!["historical-source"]
        } {
            let evidence = if history == History::Malformed {
                "unparsable old evidence"
            } else {
                r#"[1,"historical-child","SharedSync-absent","historical-owner"]"#
            };
            conn.execute("INSERT INTO unresolved_sparse_identities(library,source_record_name,original_evidence,observed_evidence,generation,first_seen_at,last_outcome) VALUES(?1,?2,?3,?3,7,1000,?4)", rusqlite::params![ZONE,source,evidence,if history==History::Malformed { Some("[invalid outcome") } else { None }]).unwrap();
        }
    }
}

async fn expansion(
    unavailable: bool,
    historical: History,
    observed_historical: bool,
    omit_after_first: bool,
    plain_source: bool,
    resolve_source: bool,
    unrepresented: bool,
) {
    let server = crate::start_wiremock_or_skip!();
    for name in ["april", "january"] {
        Mock::given(method("GET"))
            .and(path(format!("/{name}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
            .expect(1)
            .mount(&server)
            .await;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("media");
    std::fs::create_dir(&root).unwrap();
    let database = dir.path().join("state.db");
    let mut records = Vec::new();
    for (name, month, tag) in [("april", 4, 21), ("january", 1, 22)] {
        let mut entry = pair(
            name,
            &format!("{name}.JPG"),
            &format!("{}/{name}", server.uri()),
            tag,
        );
        let date = chrono::NaiveDate::from_ymd_opt(2026, month, 15)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .and_utc();
        entry[1]["fields"]["assetDate"]["value"] = json!(date.timestamp_millis());
        entry[1]["fields"]["addedDate"]["value"] = json!(date.timestamp_millis());
        records.extend(entry);
    }
    let records = Arc::new(records);
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let key = sync_token_key(ZONE);
    let mut april_mtime = None;
    let mut january_mtime = None;
    if historical != History::None {
        seed_stock_history(&database, historical, resolve_source);
    }
    let historical_before = (historical != History::None).then(|| snapshot_history(&database));
    if historical == History::InterruptedMigration {
        let conn = rusqlite::Connection::open(&database).unwrap();
        conn.execute_batch("CREATE TABLE sparse_epoch_upgrade(blocker INTEGER)")
            .unwrap();
        drop(conn);
        assert!(
            SqliteStateDb::open_owned(&database, &baseline_owner())
                .await
                .is_err()
        );
        let conn = rusqlite::Connection::open(&database).unwrap();
        assert_eq!(crate::state::schema::get_schema_version(&conn).unwrap(), 36);
        assert_eq!(
            snapshot_history(&database),
            *historical_before.as_ref().unwrap()
        );
        conn.execute_batch("DROP TABLE sparse_epoch_upgrade")
            .unwrap();
    }
    let first_observation = usize::from(historical != History::CurrentBaseline);
    let bridge_reopen = matches!(historical, History::MissingBridge | History::BlankBridge);
    let expansion_cycle = if bridge_reopen { 2 } else { 1 };
    for cycle in 0..if unavailable {
        5
    } else {
        4 + usize::from(bridge_reopen)
    } {
        let db = Arc::new(
            SqliteStateDb::open_owned(&database, &baseline_owner())
                .await
                .unwrap(),
        );
        if historical == History::InterruptedBaseline && cycle == 0 {
            let sql = format!(
                "CREATE TRIGGER fail_initial_checkpoint BEFORE INSERT ON metadata WHEN NEW.key='{}' BEGIN SELECT RAISE(ABORT,'initial checkpoint failure'); END;",
                key
            );
            db.acquire_lock("install initial checkpoint fault")
                .unwrap()
                .execute_batch(&sql)
                .unwrap();
        }
        if unavailable && cycle == 2 {
            db.make_selection_retry_due(baseline_owner(), "asset-january".into())
                .await
                .unwrap();
        }
        let prior_cursor = db.get_metadata(&key).await.unwrap();
        let session = Session {
            records: records.clone(),
            cycle,
            first_observation,
            interrupted_inventory: (historical == History::InterruptedInventory && cycle == 0)
                .then(|| root.join("april.JPG")),
            bridge_fault: (cycle == 0
                && matches!(historical, History::MissingBridge | History::BlankBridge))
            .then_some(historical == History::BlankBridge),
            unresolved: unavailable && cycle == 1,
            observed_historical,
            omit_after_first,
            plain_source,
            resolve_source,
            unrepresented,
            inventories: Arc::new(AtomicUsize::new(0)),
            omissions: Arc::new(AtomicUsize::new(0)),
        };
        let mut library = PhotoLibrary::new(
            "https://synthetic.example/database/1/photos/production/private".into(),
            Arc::default(),
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
        let pass = AlbumPass {
            kind: PassKind::Unfiled,
            album: library.all(),
            exclude_ids: Arc::default(),
        };
        let mut selected = make_run_cycle_library_state_with_passes(ZONE, &key, vec![pass]);
        selected.library = library;
        let lower = crate::config::CreatedDateFilter::CaptureDate(
            chrono::NaiveDate::from_ymd_opt(2026, if cycle < expansion_cycle { 4 } else { 1 }, 1)
                .unwrap(),
        );
        let mut config = make_run_cycle_config();
        config.filters.skip_created_before = Some(lower);
        let base = make_run_cycle_download_config_builder(&root, db.clone());
        let builder = |mode, exclude, groups, zone| {
            let mut download = (*base(mode, exclude, groups, zone)).clone();
            download.folder_structure = "none".into();
            download.skip_created_before = Some(lower);
            download.retry.max_retries = 0;
            Arc::new(download)
        };
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
        .await;
        if matches!(
            historical,
            History::InterruptedBaseline
                | History::InterruptedInventory
                | History::MissingBridge
                | History::BlankBridge
        ) && cycle == 0
        {
            if historical == History::InterruptedInventory {
                assert!(
                    session.omissions.load(Ordering::SeqCst) > 0,
                    "later malformed page must actually execute after publication"
                );
            }
            if matches!(historical, History::MissingBridge | History::BlankBridge) {
                assert!(
                    session.omissions.load(Ordering::SeqCst) > 0,
                    "bridge fault must execute after inventory"
                );
            }
            assert_eq!(
                db.get_metadata(&format!("observed_selection:v27:{ZONE}"))
                    .await
                    .unwrap(),
                None,
                "query EOF cannot certify an unfinished bridge"
            );
            match &result {
                Ok(outcome) => assert!(!outcome.can_advance_database_checkpoint()),
                Err(error) => assert!(
                    matches!(historical, History::MissingBridge | History::BlankBridge)
                        || format!("{error:#}").contains("initial checkpoint failure"),
                    "{error:#}"
                ),
            }
            assert_eq!(db.get_metadata(&key).await.unwrap(), None);
            assert_eq!(std::fs::read(root.join("april.JPG")).unwrap(), MEDIA);
            april_mtime = Some(
                std::fs::metadata(root.join("april.JPG"))
                    .unwrap()
                    .modified()
                    .unwrap(),
            );
            assert_eq!(
                snapshot_history(&database),
                *historical_before.as_ref().unwrap()
            );
            db.acquire_lock("remove initial checkpoint fault")
                .unwrap()
                .execute_batch("DROP TRIGGER IF EXISTS fail_initial_checkpoint")
                .unwrap();
            continue;
        }
        let result = result.unwrap();
        let april = root.join("april.JPG");
        assert_eq!(std::fs::read(&april).unwrap(), MEDIA);
        let mtime = std::fs::metadata(&april).unwrap().modified().unwrap();
        if cycle == 0 {
            april_mtime = Some(mtime);
        }
        assert_eq!(Some(mtime), april_mtime);
        let january = root.join("january.JPG");
        if observed_historical && cycle >= first_observation && (!resolve_source || cycle == 1) {
            assert!(!result.can_advance_database_checkpoint(), "{result:?}");
            assert!(
                result.stats.identity_incomplete,
                "current sparse source must not be ignored"
            );
            assert_eq!(db.get_metadata(&key).await.unwrap(), prior_cursor);
            if cycle > 0 {
                assert_eq!(
                    std::fs::read(&january).unwrap(),
                    MEDIA,
                    "healthy newly selected item still completes"
                );
            }
        } else if unavailable && cycle == 1 {
            assert!(
                session.omissions.load(Ordering::SeqCst) > 0,
                "current lookup fault must execute"
            );
            assert!(!result.can_advance_database_checkpoint(), "{result:?}");
            assert_eq!(
                db.get_metadata(&key).await.unwrap(),
                Some("expanded-after-1".into()),
                "complete expanded observation accounts for pending confirmation"
            );
            assert!(
                !january.exists(),
                "unconfirmed identity must not be guessed or downloaded"
            );
            let retained: Vec<u8> = db.acquire_lock("expanded current obligation").unwrap().query_row(
                "SELECT manifest FROM provider_active_decisions WHERE child='asset-january' AND admission='deferred' AND next_retry_at>0",[],|r|r.get(0)).unwrap();
            let decision: crate::state::db::provider_generations::ActiveDecision =
                serde_json::from_slice(&retained).unwrap();
            assert!(decision.is_deferred_observation());
            assert!(
                db.get_metadata(&format!("observed_selection:v27:{ZONE}"))
                    .await
                    .unwrap()
                    .is_some()
            );
        } else {
            assert_eq!(result.failed_count, 0, "cycle {cycle}: {result:?}");
            assert!(
                result.can_advance_database_checkpoint(),
                "cycle {cycle}: {result:?}"
            );
            assert_eq!(
                result.stats.downloaded,
                usize::from(cycle == 0 || cycle == if unavailable { 2 } else { expansion_cycle }),
                "{result:?}"
            );
            assert_eq!(
                db.get_metadata(&key).await.unwrap(),
                Some(format!("expanded-after-{cycle}"))
            );
        }
        if cycle < expansion_cycle {
            assert!(!january.exists());
        }
        if bridge_reopen && cycle == 1 {
            assert!(
                db.get_metadata(&format!("observed_selection:v27:{ZONE}"))
                    .await
                    .unwrap()
                    .is_some(),
                "same-selection reopen must establish observation proof before configuration expansion"
            );
        }
        if january.exists() {
            assert_eq!(std::fs::read(&january).unwrap(), MEDIA);
            let mtime = std::fs::metadata(&january).unwrap().modified().unwrap();
            if let Some(prior) = january_mtime {
                assert_eq!(mtime, prior);
            } else {
                january_mtime = Some(mtime);
            }
        }
        if cycle == expansion_cycle {
            assert!(
                session.inventories.load(Ordering::SeqCst) > 0,
                "expanded selection needs current inventory"
            );
        }
        if (!observed_historical || resolve_source && cycle >= 3)
            && cycle >= if unavailable { 3 } else { expansion_cycle + 1 }
        {
            assert_eq!(
                session.inventories.load(Ordering::SeqCst),
                0,
                "quiet reopened configuration must not replay inventory"
            );
        }
        if let Some(before) = &historical_before {
            assert_eq!(
                &snapshot_history(&database),
                before,
                "all old fields stay unchanged"
            );
            let conn = rusqlite::Connection::open(&database).unwrap();
            let marker: String = conn
                .query_row(
                    "SELECT value FROM metadata WHERE key=?1",
                    [format!("unresolved_asset_identity:{ZONE}")],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(marker, "opaque original marker");
            let summary = db.get_summary().await.unwrap();
            assert!(
                summary.unresolved_identity_zones > 0,
                "historical uncertainty stays visible"
            );
            if result.can_advance_database_checkpoint() {
                assert_ne!(
                    summary.provider_checkpoint_status.as_deref(),
                    Some("preserved")
                );
                assert_ne!(
                    summary.last_recovery_action.as_deref(),
                    Some("replay_from_prior_token")
                );
            }
            let rows = db.sparse_identities(ZONE).await.unwrap();
            if observed_historical && cycle >= first_observation && (!resolve_source || cycle == 1)
            {
                assert_eq!(
                    rows.len(),
                    1,
                    "only current obligations enter the runtime retry path"
                );
                assert!(rows[0].origin.is_some());
            }
        }
    }
    server.verify().await;
}

#[tokio::test]
async fn v027_destination_root_replaced_during_transfer_preserves_work_and_reopens() {
    let server = crate::start_wiremock_or_skip!();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("media");
    let held_root = dir.path().join("interrupted-media");
    std::fs::create_dir(&root).unwrap();
    let database = dir.path().join("state.db");
    Mock::given(method("GET"))
        .and(path("/april"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
        .expect(1)
        .mount(&server)
        .await;
    let next_gets = Arc::new(AtomicUsize::new(0));
    let counter = next_gets.clone();
    let original_root = root.clone();
    let displaced_root = held_root.clone();
    Mock::given(method("GET"))
        .and(path("/january"))
        .respond_with(move |_: &wiremock::Request| {
            if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                // A single worker has published April before starting January.
                assert_eq!(
                    std::fs::read(original_root.join("april.JPG")).unwrap(),
                    MEDIA
                );
                std::fs::rename(&original_root, &displaced_root).unwrap();
                std::fs::write(&original_root, b"external replacement, never overwrite").unwrap();
            }
            ResponseTemplate::new(200).set_body_bytes(MEDIA)
        })
        .mount(&server)
        .await;
    let mut records = pair("april", "april.JPG", &format!("{}/april", server.uri()), 21);
    records.extend(pair(
        "january",
        "january.JPG",
        &format!("{}/january", server.uri()),
        22,
    ));
    let records = Arc::new(records);
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let key = sync_token_key(ZONE);
    let mut april_mtime = None;
    let mut completed_gets = None;
    for cycle in 0..3 {
        if cycle == 1 {
            assert_eq!(
                std::fs::read(&root).unwrap(),
                b"external replacement, never overwrite"
            );
            std::fs::remove_file(&root).unwrap();
            std::fs::rename(&held_root, &root).unwrap();
        }
        let db = Arc::new(
            SqliteStateDb::open_owned(&database, &baseline_owner())
                .await
                .unwrap(),
        );
        let session = Session {
            records: records.clone(),
            cycle,
            first_observation: 1,
            interrupted_inventory: None,
            bridge_fault: None,
            unresolved: false,
            observed_historical: false,
            omit_after_first: false,
            plain_source: false,
            resolve_source: false,
            unrepresented: false,
            inventories: Arc::new(AtomicUsize::new(0)),
            omissions: Arc::new(AtomicUsize::new(0)),
        };
        let mut library = PhotoLibrary::new(
            "https://synthetic.example/database/1/photos/production/private".into(),
            Arc::default(),
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
        let mut selected = make_run_cycle_library_state_with_passes(
            ZONE,
            &key,
            vec![AlbumPass {
                kind: PassKind::Unfiled,
                album: library.all(),
                exclude_ids: Arc::default(),
            }],
        );
        selected.library = library;
        let base = make_run_cycle_download_config_builder(&root, db.clone());
        let builder = |mode, exclude, groups, zone| {
            let mut config = (*base(mode, exclude, groups, zone)).clone();
            config.folder_structure = "none".into();
            config.concurrent_downloads = 1;
            config.retry.max_retries = 0;
            Arc::new(config)
        };
        if cycle == 1 {
            db.make_selection_retry_due(baseline_owner(), "asset-january".into())
                .await
                .unwrap();
        }
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
        let original = if cycle == 0 {
            held_root.join("april.JPG")
        } else {
            root.join("april.JPG")
        };
        assert_eq!(
            std::fs::read(&original).unwrap(),
            MEDIA,
            "replacement must not destroy completed bytes"
        );
        let mtime = std::fs::metadata(&original).unwrap().modified().unwrap();
        if cycle == 0 {
            april_mtime = Some(mtime);
        }
        assert_eq!(Some(mtime), april_mtime);
        if cycle == 0 {
            assert_eq!(
                next_gets.load(Ordering::SeqCst),
                1,
                "replacement hook must fire during the real transfer"
            );
            match &result {
                Ok(result) => assert!(!result.can_advance_database_checkpoint(), "{result:?}"),
                Err(error) => assert!(
                    format!("{error:#}").contains("Not a directory")
                        || format!("{error:#}").contains("not a directory"),
                    "{error:#}"
                ),
            }
            assert_eq!(
                std::fs::read(&root).unwrap(),
                b"external replacement, never overwrite"
            );
            let pending:i64=db.acquire_lock("prove interrupted destination ownership").unwrap().query_row(
                "SELECT count(*) FROM provider_active_destinations WHERE library=?1 AND asset_id='asset-january' AND admitted=1 AND verified_media=0",
                [ZONE],|r|r.get(0)).unwrap();
            assert!(
                pending > 0,
                "interrupted destination must retain its admitted owner"
            );
            let cursor = db.get_metadata(&key).await.unwrap();
            assert!(
                cursor.is_none() || cursor.as_deref() == Some("expanded-after-0"),
                "only the accounted source successor can be saved"
            );
            if result.is_err() {
                assert!(cursor.is_none(), "an aborted cycle cannot commit a cursor");
            }
        } else {
            let result = result.unwrap();
            assert_eq!(result.failed_count, 0, "cycle {cycle}: {result:?}");
            assert!(result.can_advance_database_checkpoint(), "{result:?}");
            assert_eq!(std::fs::read(root.join("january.JPG")).unwrap(), MEDIA);
            let pending:i64=db.acquire_lock("prove interrupted destination resolved").unwrap().query_row(
                "SELECT count(*) FROM provider_active_destinations WHERE library=?1 AND asset_id='asset-january' AND admitted=1 AND verified_media=0",
                [ZONE],|r|r.get(0)).unwrap();
            assert_eq!(pending, 0);
            if cycle == 1 {
                completed_gets = Some(next_gets.load(Ordering::SeqCst));
                assert!(
                    completed_gets.unwrap() <= 2,
                    "at most one recovery request after the interrupted transfer"
                );
            } else {
                assert_eq!(Some(next_gets.load(Ordering::SeqCst)), completed_gets);
                assert_eq!(result.stats.downloaded, 0);
                assert_eq!(
                    session.inventories.load(Ordering::SeqCst),
                    0,
                    "quiet recovery must not repeat baseline"
                );
            }
        }
    }
    server.verify().await;
}
