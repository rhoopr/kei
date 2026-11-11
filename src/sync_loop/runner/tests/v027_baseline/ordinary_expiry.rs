//! Real M1 source plans with one rejected active cursor and a healthy peer.
use super::{MEDIA, baseline_owner, pair};
use crate::commands::{
    PassKind, build_collection_context, collection_libraries, pass_scope_for_zone,
    resolve_passes_for_scope, zone_name_set,
};
use crate::download;
use crate::icloud::photos::{PhotoLibrary, PhotosSession, inbox::ShadowCapture};
use crate::selection::{AlbumSelector, Selection, SmartFolderSelector, parse_library_selector};
use crate::state::SqliteStateDb;
use crate::sync_cycle::{run_cycle, sync_token_key};
use crate::sync_loop::test_support::{
    make_run_cycle_config, make_run_cycle_download_config_builder,
    make_run_cycle_library_state_with_passes, make_shared_session_for_run_cycle,
};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;
use wiremock::{
    Mock, ResponseTemplate,
    matchers::{method, path},
};

const PRIMARY: &str = "PrimarySync-expiry";
const SHARED: &str = "SharedSync-expiry";

fn zone(primary: bool) -> Value {
    json!({"zoneName":if primary { PRIMARY } else { SHARED },
        "ownerRecordName":if primary { "_defaultOwner" } else { "shared-owner" }})
}

fn media(id: &str, url: &str, source: &Value, tag: u8) -> Vec<Value> {
    let mut records = pair(id, &format!("{id}.JPG"), url, tag);
    records[0]["zoneID"] = source.clone();
    records[1]["zoneID"] = source.clone();
    records[1]["fields"]["masterRef"]["value"]["zoneID"] = source.clone();
    records[1]["fields"]["isHidden"] = json!({"value":1,"type":"INT64"});
    records
}

#[derive(Clone)]
struct SourceSession {
    primary: bool,
    phase: usize,
    fault: OriginFault,
    current: Arc<Vec<Value>>,
    added: Arc<Vec<Value>>,
    events: Arc<Mutex<Vec<&'static str>>>,
    db: Arc<SqliteStateDb>,
    cancel: CancellationToken,
}

#[async_trait::async_trait]
impl PhotosSession for SourceSession {
    async fn post(&self, url: &str, body: String, _: &[(&str, &str)]) -> anyhow::Result<Value> {
        let request: Value = serde_json::from_str(&body)?;
        let source = zone(self.primary);
        let name = if self.primary { PRIMARY } else { SHARED };
        if request["query"]["recordType"] == "CheckIndexingState" {
            return Ok(
                json!({"records":[{"recordName":"index","fields":{"state":{"value":"FINISHED"}}}]}),
            );
        }
        if url.contains("/zones/list") {
            if self.primary && self.phase > 0 {
                self.events.lock().unwrap().push("fresh-boundary");
                if matches!(
                    self.fault,
                    OriginFault::ReplacementError | OriginFault::ReplacementHoldError
                ) {
                    let conn = self
                        .db
                        .acquire_lock("inject replacement-only owner failure")?;
                    conn.execute_batch("CREATE TRIGGER IF NOT EXISTS fail_replacement_generation BEFORE INSERT ON provider_active_generations BEGIN SELECT RAISE(ABORT,'synthetic replacement generation failure'); END;")?;
                    if self.fault == OriginFault::ReplacementHoldError {
                        conn.execute_batch("CREATE TRIGGER IF NOT EXISTS fail_replacement_hold BEFORE INSERT ON metadata WHEN NEW.key='retained_checkpoint_hold:v27:PrimarySync-expiry' BEGIN SELECT RAISE(ABORT,'synthetic hold write failure'); END;")?;
                    }
                }
            }
            let fresh = if self.phase > 1
                && matches!(
                    self.fault,
                    OriginFault::RepeatBridge
                        | OriginFault::RankChange
                        | OriginFault::CheckpointGuard
                ) {
                "later-primary-boundary"
            } else {
                "fresh-primary"
            };
            return Ok(
                json!({"zones":[{"zoneID":source,"syncToken":if self.phase > 0 && self.primary {fresh.to_owned()} else {format!("{name}-boundary")},"deleted":false}]}),
            );
        }
        if url.contains("/internal/records/query/batch") {
            let count = if self.primary && self.fault == OriginFault::ManyDebt {
                if self.phase == 0 { 65 } else { 66 }
            } else if self.primary
                && self.phase > 0
                && self.fault != OriginFault::PreBoundaryDeleted
            {
                2
            } else {
                1
            };
            let batch: Vec<_> = request["batch"].as_array().unwrap().iter().map(|query| {
                let hidden = query.to_string().contains("HiddenByAssetDate");
                json!({"records":[{"fields":{"itemCount":{"value":if hidden {count} else {0}}}}]})
            }).collect();
            return Ok(json!({"batch":batch}));
        }
        if url.contains("/records/query?") {
            let kind = request["query"]["recordType"].as_str().unwrap_or("");
            if kind == "CPLAlbumByPositionLive" || kind.contains("Container") {
                return Ok(json!({"records":[],"syncToken":"folders"}));
            }
            if self.primary && self.phase > 0 {
                self.events.lock().unwrap().push("replacement-query");
                let conn = self
                    .db
                    .acquire_lock("verify fresh catalog before inventory")?;
                let installed: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM source_revalidation_rounds r JOIN metadata m ON m.key LIKE 'baseline_cursor:v27:%' WHERE r.library=?1 AND r.mode='catalog_v2' AND r.phase='observing' AND m.value='fresh-primary'",
                    [PRIMARY], |row| row.get(0),
                )?;
                assert_eq!(
                    installed, 1,
                    "fresh origin and cursor must be durable before inventory"
                );
            }
            let offset = request["query"]["filterBy"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|filter| filter["fieldName"] == "startRank")
                .and_then(|filter| filter["fieldValue"]["value"].as_u64())
                .unwrap_or(0);
            if self.primary && self.phase > 0 && offset == 0 {
                self.events.lock().unwrap().push("inventory-start");
            }
            let mut records = Vec::new();
            if offset == 0 && kind.contains("Hidden") && !kind.contains("WithoutHidden") {
                if !(self.primary
                    && self.phase > 0
                    && self.fault == OriginFault::PreBoundaryDeleted)
                {
                    records.extend(self.current.iter().cloned());
                } else {
                    self.events
                        .lock()
                        .unwrap()
                        .push("old-omitted-from-inventory");
                }
                if self.phase > 0 {
                    records.extend(self.added.iter().cloned());
                }
            }
            return Ok(json!({"records":records,"syncToken":"query-token"}));
        }
        if url.contains("/changes/zone?") {
            let cursor = request["zones"][0]["syncToken"].as_str().unwrap();
            if !self.primary {
                self.events.lock().unwrap().push("peer-delta");
            }
            if self.primary && self.phase > 0 && cursor == format!("{PRIMARY}-1") {
                self.events.lock().unwrap().push("rejected");
                return Ok(json!({"zones":[{"zoneID":source,"serverErrorCode":"BAD_REQUEST"}]}));
            }
            if self.primary && cursor == "fresh-primary" {
                if self.fault == OriginFault::RepeatBridge && self.phase == 1 {
                    self.events.lock().unwrap().push("bridge-rejected");
                    return Ok(
                        json!({"zones":[{"zoneID":source,"serverErrorCode":"BAD_REQUEST"}]}),
                    );
                }
                self.events.lock().unwrap().push("fresh-bridge");
            }
            let records = if self.phase == 1
                && (!self.primary
                    || (self.fault == OriginFault::RankChange && cursor == "fresh-primary"))
            {
                self.added.as_ref().clone()
            } else {
                Vec::new()
            };
            return Ok(
                json!({"zones":[{"zoneID":source,"records":records,"syncToken":format!("{name}-{}",self.phase+1),"moreComing":false}]}),
            );
        }
        if url.contains("/records/lookup?") {
            if self.primary && self.phase > 0 {
                self.events.lock().unwrap().push("lookup");
            }
            if self.fault == OriginFault::AuthLookup
                && self.primary
                && self.events.lock().unwrap().contains(&"fresh-bridge")
            {
                return Err(crate::icloud::photos::session::HttpStatusError {
                    status: 401,
                    url: "https://synthetic.invalid".into(),
                    retry_after: None,
                    body: None,
                }
                .into());
            }
            if self.fault == OriginFault::CancelLookup
                && self.primary
                && self.events.lock().unwrap().contains(&"fresh-bridge")
            {
                self.cancel.cancel();
            }
            if self.primary && self.events.lock().unwrap().contains(&"fresh-bridge") {
                let result = match self.fault {
                    OriginFault::RecordAuthMaster => {
                        Some(("primary-old", "AUTHENTICATION_REQUIRED"))
                    }
                    OriginFault::RecordAuthChild => {
                        Some(("asset-primary-old", "AUTHENTICATION_FAILED"))
                    }
                    OriginFault::RecordDenied => Some(("primary-old", "ACCESS_DENIED")),
                    OriginFault::RecordUnknown => Some(("asset-primary-old", "SYNTHETIC_UNKNOWN")),
                    _ => None,
                };
                if let Some((name, code)) = result {
                    return Ok(json!({"records":[{"recordName":name,"serverErrorCode":code}]}));
                }
            }
            let requested = request["records"].as_array().unwrap();
            let mut records: Vec<_> = self
                .current
                .iter()
                .chain(self.added.iter())
                .filter(|record| {
                    requested
                        .iter()
                        .any(|item| item["recordName"] == record["recordName"])
                })
                .cloned()
                .collect();
            if matches!(
                self.fault,
                OriginFault::WrongScope
                    | OriginFault::FutureDebt
                    | OriginFault::CatalogUnknownDebt
                    | OriginFault::ManyDebt
                    | OriginFault::WakeDebt
            ) && self.primary
                && self.events.lock().unwrap().contains(&"fresh-bridge")
            {
                for record in &mut records {
                    record["zoneID"]["ownerRecordName"] = json!("foreign-owner");
                }
            }
            if matches!(
                self.fault,
                OriginFault::DeletedLookup
                    | OriginFault::TopFalseDeleted
                    | OriginFault::DeletedProtected
                    | OriginFault::PreparedPublication
                    | OriginFault::PreBoundaryDeleted
            ) && self.primary
                && self.events.lock().unwrap().contains(&"fresh-bridge")
            {
                for record in &mut records {
                    if self.fault == OriginFault::TopFalseDeleted {
                        record["deleted"] = json!(false);
                    }
                    record["fields"]["isDeleted"] = json!({"value":1,"type":"INT64"});
                    record["fields"]["isExpunged"] = json!({"value":1,"type":"INT64"});
                    record["fields"]["dateExpunged"] =
                        json!({"value":1792541032000_i64,"type":"TIMESTAMP"});
                }
            }
            if self.fault == OriginFault::TopFalseMalformed
                && self.primary
                && self.events.lock().unwrap().contains(&"fresh-bridge")
            {
                for record in &mut records {
                    record["deleted"] = json!(false);
                    record["fields"]["isDeleted"] = json!({"value":"malformed","type":"STRING"});
                }
            }
            return Ok(json!({"records":records}));
        }
        anyhow::bail!("unexpected synthetic expiry endpoint")
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OriginFault {
    None,
    Write,
    CheckpointGuard,
    RepeatBridge,
    RankChange,
    DeletedLookup,
    TopFalseDeleted,
    TopFalseMalformed,
    PreBoundaryDeleted,
    MarkerWriterProbe,
    DeletedProtected,
    PreparedPublication,
    WrongScope,
    FutureDebt,
    CatalogUnknownDebt,
    WakeClean,
    WakeDebt,
    ManyDebt,
    ReplacementError,
    ReplacementHoldError,
    BridgeError,
    BridgeHoldError,
    BridgeBusy,
    AuthLookup,
    CancelLookup,
    RecordAuthMaster,
    RecordAuthChild,
    RecordDenied,
    RecordUnknown,
}

#[tokio::test]
async fn m1_expired_primary_uses_fresh_origin_before_inventory_and_peer_progresses() {
    exercise_expiry(OriginFault::None).await;
}

#[tokio::test]
async fn m1_origin_write_failure_preserves_active_then_recovers_after_reopen() {
    exercise_expiry(OriginFault::Write).await;
}

#[tokio::test]
async fn m1_required_origin_checkpoint_fault_rolls_back_then_recovers_after_reopen() {
    exercise_expiry(OriginFault::CheckpointGuard).await;
}

#[tokio::test]
async fn m1_rejected_fresh_bridge_retains_active_and_recovers_after_reopen() {
    exercise_expiry(OriginFault::RepeatBridge).await;
}

#[tokio::test]
async fn m1_rank_change_holds_without_second_inventory_and_recovers_after_reopen() {
    exercise_expiry(OriginFault::RankChange).await;
}

#[tokio::test]
async fn m1_strict_wrong_scope_retains_catalog_debt_across_quiet_reopens() {
    exercise_expiry(OriginFault::WrongScope).await;
}

#[tokio::test]
async fn m1_future_catalog_debt_remains_visible_after_reopen_without_early_lookup() {
    exercise_expiry(OriginFault::FutureDebt).await;
}

#[tokio::test]
async fn m1_unknown_catalog_mode_keeps_current_owned_debt_visible() {
    exercise_expiry(OriginFault::CatalogUnknownDebt).await;
}

#[tokio::test]
async fn watch_idle_clean_defers_plan_refresh_without_aborting() {
    exercise_expiry(OriginFault::WakeClean).await;
}

#[tokio::test]
async fn watch_idle_future_debt_and_changed_peer_preserve_progress() {
    exercise_expiry(OriginFault::WakeDebt).await;
}

#[tokio::test]
async fn m1_bounded_catalog_page_reports_the_remaining_sixty_fifth_candidate() {
    exercise_expiry(OriginFault::ManyDebt).await;
}

#[tokio::test]
async fn m1_replacement_error_retains_original_cursor_hold() {
    exercise_expiry(OriginFault::ReplacementError).await;
}

#[tokio::test]
async fn m1_replacement_error_reports_hold_write_failure() {
    exercise_expiry(OriginFault::ReplacementHoldError).await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn m1_fresh_bridge_error_retains_original_cursor_hold() {
    exercise_expiry(OriginFault::BridgeError).await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn m1_fresh_bridge_error_reports_hold_write_failure() {
    exercise_expiry(OriginFault::BridgeHoldError).await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn m1_fresh_bridge_busy_replacement_journal_retains_original_hold() {
    exercise_expiry(OriginFault::BridgeBusy).await;
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn m1_strict_deleted_pair_settles_only_confirmed_source_without_removing_media() {
    exercise_expiry(OriginFault::DeletedLookup).await;
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn m1_strict_top_false_does_not_hide_photos_deletion() {
    exercise_expiry(OriginFault::TopFalseDeleted).await;
}

#[tokio::test]
async fn m1_strict_top_false_malformed_flag_retains_debt() {
    exercise_expiry(OriginFault::TopFalseMalformed).await;
}

#[tokio::test]
async fn m1_strict_record_master_auth_without_peer_preserves_reauthentication() {
    exercise_expiry(OriginFault::RecordAuthMaster).await;
}

#[tokio::test]
async fn m1_strict_record_child_auth_without_peer_preserves_reauthentication() {
    exercise_expiry(OriginFault::RecordAuthChild).await;
}

#[tokio::test]
async fn m1_strict_record_denial_without_peer_is_not_authentication() {
    exercise_expiry(OriginFault::RecordDenied).await;
}

#[tokio::test]
async fn m1_strict_record_unknown_without_peer_is_not_authentication() {
    exercise_expiry(OriginFault::RecordUnknown).await;
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn m1_preboundary_deleted_pair_omitted_from_inventory_settles_after_bridge() {
    exercise_expiry(OriginFault::PreBoundaryDeleted).await;
}

#[tokio::test]
async fn m1_strict_deleted_pair_retains_pending_metadata_and_media() {
    exercise_expiry(OriginFault::DeletedProtected).await;
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn m1_strict_deleted_pair_retains_real_prepared_publication() {
    exercise_expiry(OriginFault::PreparedPublication).await;
}

#[tokio::test]
async fn m1_grouping_marker_writer_is_identified_at_baseline() {
    exercise_expiry(OriginFault::MarkerWriterProbe).await;
}

#[tokio::test]
async fn m1_strict_auth_failure_preserves_debt_and_reauthentication_priority() {
    exercise_expiry(OriginFault::AuthLookup).await;
}

#[tokio::test]
async fn m1_strict_lookup_cancellation_preserves_debt_and_interruption_priority() {
    exercise_expiry(OriginFault::CancelLookup).await;
}

async fn exercise_expiry(fault: OriginFault) {
    let server = crate::start_wiremock_or_skip!();
    for id in ["primary-old", "primary-new", "shared-old", "shared-new"] {
        let mock = Mock::given(method("GET"))
            .and(path(format!("/{id}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA));
        if matches!(
            fault,
            OriginFault::MarkerWriterProbe
                | OriginFault::ReplacementError
                | OriginFault::ReplacementHoldError
                | OriginFault::BridgeError
                | OriginFault::BridgeHoldError
                | OriginFault::BridgeBusy
        ) {
            mock.mount(&server).await;
        } else {
            mock.expect(if fault == OriginFault::ManyDebt && id == "primary-old" {
                65
            } else {
                1
            })
            .mount(&server)
            .await;
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("media");
    std::fs::create_dir(&root).unwrap();
    let database = dir.path().join("state.db");
    super::seed_existing_upgrade_db(&database);
    let (_session_dir, session) = make_shared_session_for_run_cycle().await;
    let events = Arc::new(Mutex::new(Vec::new()));
    let selection = Selection {
        albums: AlbumSelector::None,
        albums_explicit: false,
        smart_folders: SmartFolderSelector::Named {
            included: BTreeSet::from(["Hidden".to_owned()]),
            excluded: BTreeSet::new(),
        },
        smart_folders_explicit: true,
        libraries: parse_library_selector(&["shared".to_owned()]).unwrap(),
        unfiled: true,
    };
    let mut attempt = None;
    let mut published = Vec::new();
    let mut original_active = None;
    let mut initial_origin = None;
    let mut retained_debt = None;
    let mut sidecar_snapshot = None;
    let mut baseline_marker = None;
    let mut prepared = None;
    let deferred = matches!(
        fault,
        OriginFault::WrongScope
            | OriginFault::FutureDebt
            | OriginFault::CatalogUnknownDebt
            | OriginFault::WakeDebt
            | OriginFault::ManyDebt
            | OriginFault::DeletedProtected
            | OriginFault::PreparedPublication
            | OriginFault::TopFalseMalformed
            | OriginFault::RecordDenied
            | OriginFault::RecordUnknown
    );
    let interrupted = matches!(
        fault,
        OriginFault::Write
            | OriginFault::CheckpointGuard
            | OriginFault::RepeatBridge
            | OriginFault::RankChange
    );
    let confirmed_deletion = matches!(
        fault,
        OriginFault::DeletedLookup | OriginFault::TopFalseDeleted | OriginFault::PreBoundaryDeleted
    );
    let clean_metadata = confirmed_deletion || fault == OriginFault::PreparedPublication;
    let recovery_phase = if interrupted { 2 } else { 1 };
    for phase in 0..recovery_phase + 3 {
        let db = Arc::new(
            SqliteStateDb::open_owned(&database, &baseline_owner())
                .await
                .unwrap(),
        );
        if phase == 2 && interrupted {
            // The real first attempt created the bounded hold. Move only its
            // deadline to the due boundary, without changing its source,
            // policy binding or attempt count, so the existing recovery and
            // two quiet reopen oracles exercise the permitted next attempt.
            let key = format!(
                "{}{}",
                crate::sync_cycle::RETAINED_CHECKPOINT_HOLD_PREFIX,
                PRIMARY
            );
            let mut hold: Value =
                serde_json::from_str(&db.get_metadata(&key).await.unwrap().unwrap()).unwrap();
            assert_eq!(hold["attempt_count"], 1);
            assert!(hold["next_attempt_at"].as_i64().unwrap() > chrono::Utc::now().timestamp());
            hold["next_attempt_at"] = json!(0);
            db.set_metadata(&key, &hold.to_string()).await.unwrap();
        }
        if fault == OriginFault::Write && phase == 1 {
            db.acquire_lock("inject catalog origin write failure").unwrap().execute_batch(
                "CREATE TRIGGER fail_fresh_catalog_origin BEFORE INSERT ON source_revalidation_rounds WHEN NEW.mode='catalog_v2' BEGIN SELECT RAISE(ABORT,'synthetic catalog write fault'); END;"
            ).unwrap();
        }
        if fault == OriginFault::MarkerWriterProbe && phase == 0 {
            db.acquire_lock("trace marker writer").unwrap().execute_batch(
                "CREATE TEMP TRIGGER trace_marker_writer BEFORE UPDATE OF metadata_write_failed_at ON main.assets WHEN NEW.library='PrimarySync-expiry' AND OLD.metadata_write_failed_at IS NULL AND NEW.metadata_write_failed_at IS NOT NULL BEGIN SELECT RAISE(ABORT,'synthetic marker writer probe'); END;"
            ).unwrap();
        }
        if fault == OriginFault::CheckpointGuard && phase == 1 {
            db.acquire_lock("inject required origin interleave").unwrap().execute_batch(
                "CREATE TRIGGER required_origin_interleave AFTER UPDATE ON metadata WHEN NEW.key='sync_token:v27:PrimarySync-expiry' BEGIN UPDATE source_revalidation_rounds SET boundary='changed' WHERE library='PrimarySync-expiry' AND mode='catalog_v2'; END;"
            ).unwrap();
        }
        let shutdown = CancellationToken::new();
        let library = |primary| {
            let source = zone(primary);
            let name = if primary { "primary" } else { "shared" };
            let old = format!("{name}-old");
            let new = format!("{name}-new");
            let mut current = media(
                &old,
                &format!("{}/{old}", server.uri()),
                &source,
                if primary { 71 } else { 72 },
            );
            if primary && fault == OriginFault::ManyDebt {
                for index in 0..64 {
                    current.extend(media(
                        &format!("primary-extra-{index:02}"),
                        &format!("{}/primary-old", server.uri()),
                        &source,
                        index as u8,
                    ));
                }
            }
            SourceSession {
                primary,
                phase,
                fault,
                db: db.clone(),
                cancel: shutdown.clone(),
                events: events.clone(),
                current: Arc::new(current),
                added: Arc::new(media(
                    &new,
                    &format!("{}/{new}", server.uri()),
                    &source,
                    if primary { 73 } else { 74 },
                )),
            }
        };
        let mut primary = PhotoLibrary::new(
            "https://synthetic.example/database/1/photos/production/private".into(),
            Arc::default(),
            Box::new(library(true)),
            Arc::new(zone(true)),
            "private".into(),
            crate::retry::RetryConfig::default(),
        )
        .await
        .unwrap();
        let mut shared = PhotoLibrary::new(
            "https://synthetic.example/database/1/photos/production/shared".into(),
            Arc::default(),
            Box::new(library(false)),
            Arc::new(zone(false)),
            "shared".into(),
            crate::retry::RetryConfig::default(),
        )
        .await
        .unwrap();
        primary.shadow_capture = Some(ShadowCapture::new(db.clone(), baseline_owner(), "com"));
        shared.shadow_capture = Some(ShadowCapture::new(db.clone(), baseline_owner(), "com"));
        let selected = vec![shared.clone()];
        let all = vec![primary.clone(), shared.clone()];
        let collections = collection_libraries(&selection, &selected, &all);
        let selected_zones = zone_name_set(&selected);
        let collection_zones = zone_name_set(collections);
        let context = build_collection_context(&selection, collections)
            .await
            .unwrap();
        let mut states = Vec::new();
        for library in [primary, shared] {
            let name = library.zone_name().to_owned();
            let scope = pass_scope_for_zone(&selection, &name, &selected_zones, &collection_zones);
            let plan = resolve_passes_for_scope(&library, &selection, scope, &context, &[])
                .await
                .unwrap();
            assert_eq!(
                plan.passes.iter().map(|pass| pass.kind).collect::<Vec<_>>(),
                if name == PRIMARY {
                    vec![PassKind::SmartFolder]
                } else {
                    vec![PassKind::SmartFolder, PassKind::Unfiled]
                }
            );
            let mut state = make_run_cycle_library_state_with_passes(
                &name,
                &sync_token_key(&name),
                plan.passes,
            );
            state.library = library;
            states.push(state);
        }
        let base = make_run_cycle_download_config_builder(&root, db.clone());
        let mut run_config = make_run_cycle_config();
        #[cfg(feature = "xmp")]
        if clean_metadata {
            run_config.runtime.refresh_metadata = phase == 0;
            run_config.metadata.xmp_sidecar = true;
        }
        let bridge_lock = Mutex::new(None::<std::fs::File>);
        let builder = |mode: download::SyncMode, exclude, groups, library: Arc<str>| {
            #[cfg(target_os = "linux")]
            if matches!(
                fault,
                OriginFault::BridgeBusy | OriginFault::BridgeError | OriginFault::BridgeHoldError
            ) && phase == 1
                && library.as_ref() == PRIMARY
                && matches!(&mode, download::SyncMode::Incremental { zone_sync_token } if zone_sync_token == "fresh-primary")
            {
                use fs4::fs_std::FileExt;
                assert!(events.lock().unwrap().contains(&"rejected"));
                {
                    let conn = db
                        .acquire_lock("check pre-bridge source and origin")
                        .unwrap();
                    let cursor: String = conn
                        .query_row(
                            "SELECT value FROM metadata WHERE key=?1",
                            [sync_token_key(PRIMARY)],
                            |row| row.get(0),
                        )
                        .unwrap();
                    assert_eq!(cursor, format!("{PRIMARY}-1"));
                    let origins: i64 = conn.query_row("SELECT COUNT(*) FROM source_revalidation_rounds r JOIN metadata m ON m.key LIKE 'baseline_cursor:v27:%' WHERE r.library=?1 AND r.mode='catalog_v2' AND r.phase='observing' AND m.value='fresh-primary'", [PRIMARY], |row| row.get(0)).unwrap();
                    assert_eq!(origins, 1);
                    if fault == OriginFault::BridgeHoldError {
                        conn.execute_batch("CREATE TRIGGER fail_bridge_hold BEFORE INSERT ON metadata WHEN NEW.key='retained_checkpoint_hold:v27:PrimarySync-expiry' BEGIN SELECT RAISE(ABORT,'synthetic bridge hold write failure'); END;").unwrap();
                    }
                }
                let directory = root.join("bridge-journal-fault");
                std::fs::create_dir(&directory).unwrap();
                download::file::seed_test_replacement_journal(
                    &directory.join("target"),
                    &directory.join("part"),
                    b"original fixture",
                    b"replacement fixture",
                )
                .unwrap();
                let journal = std::fs::read_dir(&directory)
                    .unwrap()
                    .map(Result::unwrap)
                    .map(|entry| entry.path())
                    .find(|path| {
                        path.file_name()
                            .unwrap()
                            .to_string_lossy()
                            .starts_with(".kei-replace-")
                    })
                    .unwrap();
                let manifest = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(journal.join("manifest.json"))
                    .unwrap();
                assert!(manifest.try_lock_exclusive().unwrap());
                assert!(bridge_lock.lock().unwrap().replace(manifest).is_none());
                events.lock().unwrap().push("bridge-call");
            }
            let mut config = (*base(mode, exclude, groups, library)).clone();
            config.folder_structure = "none".into();
            if clean_metadata {
                config.metadata = crate::config::MetadataConfig::default();
            }
            #[cfg(feature = "xmp")]
            {
                config.metadata.xmp_sidecar = clean_metadata;
            }
            Arc::new(config)
        };
        let trace = matches!(
            fault,
            OriginFault::MarkerWriterProbe
                | OriginFault::BridgeBusy
                | OriginFault::BridgeError
                | OriginFault::BridgeHoldError
        )
        .then(crate::test_helpers::TracingCapture::install);
        let result = run_cycle(
            &[&states[0], &states[1]],
            &run_config,
            Some(db.as_ref()),
            false,
            &builder,
            download::DownloadControls::download_hidden(),
            &session,
            &shutdown,
        )
        .await;
        if fault == OriginFault::MarkerWriterProbe
            && let Some((capture, _guard)) = &trace
        {
            assert!(
                capture.contains_event(|event| {
                    event.message() == Some("Failed to record asset")
                        && event.field("error").is_some_and(|error| {
                            error.contains("upsert_seen::album_marker")
                                && error.contains("synthetic marker writer probe")
                        })
                }),
                "the diagnostic must identify the pending album marker writer"
            );
            let result = result.unwrap();
            assert!(result.stats.state_write_failures > 0);
            assert!(!result.can_advance_database_checkpoint());
            let conn = db
                .acquire_lock("verify marker probe peer progress")
                .unwrap();
            let peer: String = conn
                .query_row(
                    "SELECT value FROM metadata WHERE key=?1",
                    [sync_token_key(SHARED)],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(peer, format!("{SHARED}-1"));
            return;
        }
        if phase == 1
            && matches!(
                fault,
                OriginFault::ReplacementError
                    | OriginFault::ReplacementHoldError
                    | OriginFault::BridgeError
                    | OriginFault::BridgeHoldError
                    | OriginFault::BridgeBusy
            )
        {
            let error = result
                .expect_err("replacement generation must fail after typed original rejection");
            let text = format!("{error:#}");
            if matches!(
                fault,
                OriginFault::BridgeBusy | OriginFault::BridgeError | OriginFault::BridgeHoldError
            ) {
                assert!(text.contains("Replacement journal is busy"), "{text}");
                assert!(events.lock().unwrap().contains(&"bridge-call"));
                let captured = trace.as_ref().unwrap().0.events();
                let index = |message| {
                    captured
                        .iter()
                        .position(|event| event.message() == Some(message))
                        .unwrap_or_else(|| panic!("missing {message}: {captured:?}"))
                };
                let typed = index("test typed retained main rejection");
                let inventory = index("test fresh bridge inventory accepted");
                let returned = index("test fresh bridge returned error");
                assert!(typed < inventory && inventory < returned, "{captured:?}");
                assert_eq!(captured[inventory].field("full"), Some("true"));
                assert_eq!(captured[inventory].field("advance"), Some("true"));
                assert_eq!(
                    captured[inventory].field("enumeration_incomplete"),
                    Some("false")
                );
                assert_eq!(captured[inventory].field("enumeration_errors"), Some("0"));
                assert_eq!(captured[inventory].field("state_write_failures"), Some("0"));
                assert_eq!(captured[inventory].field("observation"), Some("true"));
                assert_eq!(
                    captured[inventory].field("retained_main_rejected"),
                    Some("true")
                );
                assert_eq!(captured[returned].field("fresh_origin"), Some("true"));
                eprintln!(
                    "typed main rejection -> complete observed inventory -> fresh bridge returned busy-journal error before checkpoint"
                );
            } else {
                assert!(
                    text.contains("synthetic replacement generation failure"),
                    "{text}"
                );
            }
            assert!(events.lock().unwrap().contains(&"rejected"));
            let (key, raw, original) = {
                let conn = db.acquire_lock("verify replacement error hold").unwrap();
                let original: String = conn
                    .query_row(
                        "SELECT value FROM metadata WHERE key=?1",
                        [sync_token_key(PRIMARY)],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert_eq!(original, format!("{PRIMARY}-1"));
                let origin: (String, i64) = conn.query_row("SELECT phase,(SELECT COUNT(*) FROM source_revalidation_debt d WHERE d.round=r.id) FROM source_revalidation_rounds r WHERE library=?1 AND mode='catalog_v2' AND phase='observing'", [PRIMARY], |row| Ok((row.get(0)?, row.get(1)?))).unwrap();
                assert_eq!(origin, ("observing".to_owned(), 1));
                let key = format!(
                    "{}{}",
                    crate::sync_cycle::RETAINED_CHECKPOINT_HOLD_PREFIX,
                    PRIMARY
                );
                let raw: Option<String> = conn
                    .query_row("SELECT value FROM metadata WHERE key=?1", [&key], |row| {
                        row.get(0)
                    })
                    .ok();
                if matches!(
                    fault,
                    OriginFault::ReplacementHoldError | OriginFault::BridgeHoldError
                ) {
                    assert!(raw.is_none());
                    assert!(
                        text.contains(if fault == OriginFault::BridgeHoldError {
                            "synthetic bridge hold write failure"
                        } else {
                            "synthetic hold write failure"
                        }),
                        "hold-write failure must be visible: {text}"
                    );
                } else {
                    let hold: Value = serde_json::from_str(
                        raw.as_ref()
                            .expect("the original rejected cursor must retain its fenced hold"),
                    )
                    .unwrap();
                    use sha2::{Digest, Sha256};
                    assert_eq!(
                        hold["prior_cursor_sha256"],
                        data_encoding::HEXLOWER.encode(&Sha256::digest(original.as_bytes()))
                    );
                    assert_eq!(hold["attempt_count"], 1);
                    assert!(
                        hold["next_attempt_at"].as_i64().unwrap() > chrono::Utc::now().timestamp()
                    );
                }
                for (path, modified) in &published {
                    assert_eq!(std::fs::read(path).unwrap(), MEDIA);
                    assert_eq!(
                        &std::fs::metadata(path).unwrap().modified().unwrap(),
                        modified
                    );
                }
                (key, raw, original)
            };
            drop(states);
            drop(context);
            drop(all);
            drop(selected);
            drop(base);
            drop(bridge_lock);
            assert_eq!(
                Arc::strong_count(&db),
                1,
                "all source owners must close before reopen"
            );
            drop(db);
            let reopened = Arc::new(
                SqliteStateDb::open_owned(&database, &baseline_owner())
                    .await
                    .unwrap(),
            );
            assert_eq!(reopened.get_metadata(&key).await.unwrap(), raw);
            assert_eq!(
                reopened
                    .get_metadata(&sync_token_key(PRIMARY))
                    .await
                    .unwrap()
                    .as_deref(),
                Some(original.as_str())
            );
            let debt: i64 = {
                let conn = reopened
                    .acquire_lock("verify replacement origin after reopen")
                    .unwrap();
                conn.query_row("SELECT COUNT(*) FROM source_revalidation_debt d JOIN source_revalidation_rounds r ON r.id=d.round WHERE r.library=?1 AND r.phase='observing'", [PRIMARY], |row| row.get(0)).unwrap()
            };
            assert_eq!(debt, 1);
            if matches!(
                fault,
                OriginFault::BridgeBusy | OriginFault::BridgeError | OriginFault::BridgeHoldError
            ) {
                assert_eq!(
                    std::fs::read(root.join("bridge-journal-fault/target")).unwrap(),
                    b"original fixture"
                );
                assert_eq!(
                    std::fs::read(root.join("bridge-journal-fault/part")).unwrap(),
                    b"replacement fixture"
                );
                if fault == OriginFault::BridgeHoldError {
                    eprintln!(
                        "reopened: original cursor and absent failed hold unchanged; origin debt=1; journal fixture bytes retained"
                    );
                } else {
                    eprintln!(
                        "reopened: original cursor and fenced hold unchanged; origin debt=1; journal fixture bytes retained"
                    );
                }
            }
            if fault == OriginFault::BridgeError {
                let peer_before = reopened
                    .get_metadata(&sync_token_key(SHARED))
                    .await
                    .unwrap();
                let rejected_before = events
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|event| **event == "rejected")
                    .count();
                let peer_calls_before = events
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|event| **event == "peer-delta")
                    .count();
                let follow_cancel = CancellationToken::new();
                let mut follow_libraries = Vec::new();
                for primary in [true, false] {
                    let source = zone(primary);
                    let name = if primary { "primary" } else { "shared" };
                    let old = format!("{name}-old");
                    let new = format!("{name}-new");
                    let mock = SourceSession {
                        primary,
                        phase: 2,
                        fault: OriginFault::None,
                        current: Arc::new(media(
                            &old,
                            &format!("{}/{old}", server.uri()),
                            &source,
                            if primary { 71 } else { 72 },
                        )),
                        added: Arc::new(media(
                            &new,
                            &format!("{}/{new}", server.uri()),
                            &source,
                            if primary { 73 } else { 74 },
                        )),
                        events: events.clone(),
                        db: reopened.clone(),
                        cancel: follow_cancel.clone(),
                    };
                    let mut library = PhotoLibrary::new(
                        format!(
                            "https://synthetic.example/database/1/photos/production/{}",
                            if primary { "private" } else { "shared" }
                        ),
                        Arc::default(),
                        Box::new(mock),
                        Arc::new(source),
                        if primary { "private" } else { "shared" }.into(),
                        crate::retry::RetryConfig::default(),
                    )
                    .await
                    .unwrap();
                    library.shadow_capture = Some(ShadowCapture::new(
                        reopened.clone(),
                        baseline_owner(),
                        "com",
                    ));
                    follow_libraries.push(library);
                }
                let follow_selected = vec![follow_libraries[1].clone()];
                let follow_context = build_collection_context(
                    &selection,
                    collection_libraries(&selection, &follow_selected, &follow_libraries),
                )
                .await
                .unwrap();
                let follow_selected_zones = zone_name_set(&follow_selected);
                let follow_collection_zones = zone_name_set(collection_libraries(
                    &selection,
                    &follow_selected,
                    &follow_libraries,
                ));
                let mut follow_states = Vec::new();
                for library in follow_libraries {
                    let name = library.zone_name().to_owned();
                    let scope = pass_scope_for_zone(
                        &selection,
                        &name,
                        &follow_selected_zones,
                        &follow_collection_zones,
                    );
                    let plan =
                        resolve_passes_for_scope(&library, &selection, scope, &follow_context, &[])
                            .await
                            .unwrap();
                    let mut state = make_run_cycle_library_state_with_passes(
                        &name,
                        &sync_token_key(&name),
                        plan.passes,
                    );
                    state.library = library;
                    follow_states.push(state);
                }
                let follow_base = make_run_cycle_download_config_builder(&root, reopened.clone());
                let follow_builder = |mode, exclude, groups, library| {
                    let mut config = (*follow_base(mode, exclude, groups, library)).clone();
                    config.folder_structure = "none".into();
                    Arc::new(config)
                };
                let follow = run_cycle(
                    &[&follow_states[0], &follow_states[1]],
                    &run_config,
                    Some(reopened.as_ref()),
                    false,
                    &follow_builder,
                    download::DownloadControls::download_hidden(),
                    &session,
                    &follow_cancel,
                )
                .await
                .unwrap();
                assert_eq!(follow.failed_count, 0, "{follow:?}");
                assert!(
                    follow.holds.iter().any(|hold| hold.source == PRIMARY
                        && hold.kind == crate::sync_cycle::LibraryHoldKind::RetainedCheckpoint),
                    "{follow:?}"
                );
                assert_eq!(
                    reopened.get_metadata(&key).await.unwrap(),
                    raw,
                    "natural before-due wake must not rewrite deadline"
                );
                assert_eq!(
                    reopened
                        .get_metadata(&sync_token_key(PRIMARY))
                        .await
                        .unwrap()
                        .as_deref(),
                    Some(original.as_str())
                );
                let peer_after = reopened
                    .get_metadata(&sync_token_key(SHARED))
                    .await
                    .unwrap();
                assert_ne!(peer_after, peer_before, "healthy peer must advance");
                assert_eq!(peer_after.as_deref(), Some(format!("{SHARED}-3").as_str()));
                assert_eq!(
                    events
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|event| **event == "rejected")
                        .count(),
                    rejected_before
                );
                assert!(
                    events
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|event| **event == "peer-delta")
                        .count()
                        > peer_calls_before
                );
                for (path, modified) in &published {
                    assert_eq!(std::fs::read(path).unwrap(), MEDIA);
                    assert_eq!(
                        &std::fs::metadata(path).unwrap().modified().unwrap(),
                        modified
                    );
                }
            }
            return;
        }
        let mut result = result.unwrap();
        if fault == OriginFault::None {
            assert!(
                db.get_metadata(&format!(
                    "{}{}",
                    crate::sync_cycle::RETAINED_CHECKPOINT_HOLD_PREFIX,
                    PRIMARY
                ))
                .await
                .unwrap()
                .is_none(),
                "a completed fresh bridge must not create a retained checkpoint hold"
            );
        }
        if clean_metadata && phase == 0 {
            let failures = download::drain_pending_metadata_rewrites(
                db.as_ref(),
                &root,
                &run_config.metadata,
                download::CaptureTimestampRepair::Preserve,
                &[PRIMARY, SHARED],
                Arc::from(".kei-tmp"),
                &shutdown,
            )
            .await;
            assert_eq!(
                failures, 0,
                "the explicitly enabled metadata tail must complete"
            );
        }
        let expected_downloads = match (fault, phase) {
            (OriginFault::ManyDebt, 0) => 66,
            (_, 0)
            | (
                OriginFault::None
                | OriginFault::RepeatBridge
                | OriginFault::RankChange
                | OriginFault::DeletedLookup
                | OriginFault::TopFalseDeleted
                | OriginFault::TopFalseMalformed
                | OriginFault::PreBoundaryDeleted
                | OriginFault::CheckpointGuard
                | OriginFault::DeletedProtected
                | OriginFault::PreparedPublication
                | OriginFault::WrongScope
                | OriginFault::FutureDebt
                | OriginFault::CatalogUnknownDebt
                | OriginFault::WakeClean
                | OriginFault::WakeDebt
                | OriginFault::ManyDebt
                | OriginFault::AuthLookup
                | OriginFault::CancelLookup
                | OriginFault::RecordAuthMaster
                | OriginFault::RecordAuthChild
                | OriginFault::RecordDenied
                | OriginFault::RecordUnknown,
                1,
            ) => 2,
            (OriginFault::Write, 1 | 2) => 1,
            _ => 0,
        };
        assert_eq!(result.stats.downloaded, expected_downloads, "{result:?}");
        if fault == OriginFault::DeletedProtected && phase <= 1 {
            let conn = db.acquire_lock("inspect retained metadata owner").unwrap();
            let snapshot: (String, Option<i64>, Option<i64>, i64) = conn.query_row(
                "SELECT a.status, a.metadata_write_failed_at, p.metadata_write_failed_at, (SELECT COUNT(*) FROM asset_albums g WHERE g.library=a.library AND g.asset_id=a.id) FROM assets a JOIN asset_metadata_paths p ON p.library=a.library AND p.id=a.id AND p.version_size=a.version_size AND p.local_path=a.local_path WHERE a.library=?1 AND a.id='asset-primary-old'",
                [PRIMARY], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            ).unwrap();
            assert_eq!(snapshot.0, "downloaded");
            assert!(snapshot.1.is_some() && snapshot.1 == snapshot.2);
            assert_eq!(snapshot.3, 1);
            if phase == 0 {
                baseline_marker = snapshot.1;
            } else {
                assert_eq!(
                    snapshot.1, baseline_marker,
                    "replacement inventory must preserve the initial grouping marker"
                );
            }
        }
        if interrupted && phase == 1 {
            assert!(!result.can_advance_database_checkpoint(), "{result:?}");
            assert!(
                db.get_metadata(&format!(
                    "{}{}",
                    crate::sync_cycle::RETAINED_CHECKPOINT_HOLD_PREFIX,
                    PRIMARY
                ))
                .await
                .unwrap()
                .is_some(),
                "an incomplete typed main rejection must retain its bounded hold"
            );
            if matches!(fault, OriginFault::Write | OriginFault::CheckpointGuard) {
                assert!(result.stats.state_write_failures > 0, "{result:?}");
            }
            if matches!(
                fault,
                OriginFault::RepeatBridge | OriginFault::RankChange | OriginFault::CheckpointGuard
            ) {
                let probe = builder(
                    download::SyncMode::Full,
                    Arc::new(rustc_hash::FxHashSet::default()),
                    Arc::new(download::AssetGroupings::default()),
                    Arc::from(PRIMARY),
                );
                assert!(
                    crate::sync_loop::precheck::has_due_catalog(
                        &states[0],
                        &probe,
                        download::DownloadControls::download_hidden(),
                    )
                    .await
                    .unwrap(),
                    "the unactivated origin must wake its source"
                );
            }
        } else {
            assert!(result.can_advance_database_checkpoint(), "{result:?}");
        }
        if matches!(fault, OriginFault::WakeClean | OriginFault::WakeDebt) && phase > recovery_phase
        {
            let before_lookups = events
                .lock()
                .unwrap()
                .iter()
                .filter(|event| **event == "lookup")
                .count();
            let debt_before: Vec<(String, i64)> = {
                let conn = db.acquire_lock("read wake debt before inspection").unwrap();
                let mut statement = conn.prepare("SELECT d.id,d.next_retry_at FROM source_revalidation_debt d JOIN source_revalidation_rounds r ON r.id=d.round WHERE r.library=?1 AND r.phase='active' ORDER BY d.id").unwrap();
                statement
                    .query_map([PRIMARY], |row| Ok((row.get(0)?, row.get(1)?)))
                    .unwrap()
                    .collect::<rusqlite::Result<_>>()
                    .unwrap()
            };
            crate::sync_loop::runner::defer_watch_plan_refresh(&mut states);
            let mut precheck = crate::sync_loop::precheck::WatchPrecheck::SkipAll;
            crate::sync_loop::precheck::include_pending_provider_work(
                &mut precheck,
                &states,
                &builder,
                download::DownloadControls::download_hidden(),
            )
            .await;
            assert!(matches!(
                precheck,
                crate::sync_loop::precheck::WatchPrecheck::SkipAll
            ));
            let quiet = crate::sync_loop::runner::quiet_catalog_cycle(
                &[&states[0], &states[1]],
                &builder,
                download::DownloadControls::download_hidden(),
                &download::compute_config_hash(&run_config),
                &shutdown,
            )
            .await
            .expect("a normal post-idle wake must not abort");
            assert!(
                states
                    .iter()
                    .all(|state| state.plan_needs_refresh && !state.plan_is_stale)
            );
            if fault == OriginFault::WakeClean {
                assert!(
                    quiet.is_none(),
                    "fully settled quiet source remains healthy"
                );
                assert!(debt_before.is_empty());
                return;
            }
            let quiet = quiet.expect("future selected debt must remain a visible partial");
            assert_eq!(quiet.failed_count, 0);
            assert!(!quiet.holds.is_empty());
            assert!(!quiet.operational_safe_holds);
            let notifier = crate::notifications::Notifier::new(None);
            let report_path = dir.path().join("post-idle-report.json");
            let reporter = crate::cycle_reporter::CycleReporter::new(
                crate::cycle_reporter::CycleReporterConfig {
                    watch_mode: true,
                    report_path: Some(&report_path),
                    run_options: crate::report::RunOptions::from_config(&run_config),
                    health_dir: dir.path(),
                    personality_mode: crate::personality::Mode::Off,
                    state_db: Some(db.as_ref()),
                    metrics_handle: None,
                    notifier: &notifier,
                },
            );
            let mut health = crate::health::HealthStatus::new();
            reporter
                .report_completed_cycle(
                    &mut health,
                    crate::cycle_reporter::CycleFacts::from_cycle_result(
                        &quiet,
                        std::time::Duration::ZERO,
                    ),
                )
                .await;
            let report: Value =
                serde_json::from_slice(&std::fs::read(report_path).unwrap()).unwrap();
            assert_eq!(report["status"], "partial_failure");
            assert_eq!(report["stats"]["failed"], 0);
            assert!(health.last_success_at.is_none());
            let changed = rustc_hash::FxHashSet::from_iter([SHARED.to_owned()]);
            crate::sync_loop::planning::refresh_needed_library_plans(
                &mut states,
                &selection,
                &context,
                Some(&changed),
                &mut 0,
            )
            .await;
            assert!(states[0].plan_needs_refresh && !states[0].plan_is_stale);
            assert!(!states[1].plan_needs_refresh && !states[1].plan_is_stale);
            let mut peer = run_cycle(
                &[&states[1]],
                &run_config,
                Some(db.as_ref()),
                false,
                &builder,
                download::DownloadControls::download_hidden(),
                &session,
                &shutdown,
            )
            .await
            .unwrap();
            assert_eq!(peer.failed_count, 0);
            let drained = crate::sync_loop::runner::drain_catalog_source_revalidation(
                &[&states[0], &states[1]],
                &builder,
                download::DownloadControls::download_hidden(),
                &download::compute_config_hash(&run_config),
                &shutdown,
            )
            .await
            .expect("unchanged source must not abort a changed peer");
            crate::sync_loop::runner::apply_catalog_drain_outcome(
                &mut peer,
                &[&states[0], &states[1]],
                drained,
                false,
            );
            assert!(crate::sync_loop::runner::one_shot_partial_error(&peer).is_some());
            assert_eq!(
                events
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|event| **event == "lookup")
                    .count(),
                before_lookups
            );
            let debt_after: Vec<(String, i64)> = {
                let conn = db.acquire_lock("read wake debt after inspection").unwrap();
                let mut statement = conn.prepare("SELECT d.id,d.next_retry_at FROM source_revalidation_debt d JOIN source_revalidation_rounds r ON r.id=d.round WHERE r.library=?1 AND r.phase='active' ORDER BY d.id").unwrap();
                statement
                    .query_map([PRIMARY], |row| Ok((row.get(0)?, row.get(1)?)))
                    .unwrap()
                    .collect::<rusqlite::Result<_>>()
                    .unwrap()
            };
            assert_eq!(debt_after, debt_before);
            return;
        }
        if fault == OriginFault::CatalogUnknownDebt && phase > recovery_phase {
            let before_lookups = events
                .lock()
                .unwrap()
                .iter()
                .filter(|event| **event == "lookup")
                .count();
            let snapshot = |db: &SqliteStateDb| -> (String, String, String, String, i64) {
                db.acquire_lock("snapshot current catalog-mode debt").unwrap().query_row(
                    "SELECT r.id,r.scope,r.enum_hash,d.id,d.next_retry_at FROM source_revalidation_rounds r JOIN source_revalidation_debt d ON d.round=r.id WHERE r.library=?1 AND r.phase='active' AND r.mode IN ('catalog_v2','future_catalog_mode')",
                    [PRIMARY], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?)),
                ).unwrap()
            };
            let before = snapshot(&db);
            assert!(before.4 > chrono::Utc::now().timestamp());
            let origin =
                crate::state::db::source_revalidation::CatalogOrigin::parse(&before.1).unwrap();
            assert_eq!(origin.nonce, before.0);
            let epoch: Value = serde_json::from_str(
                &db.get_metadata(&crate::state::db::sync_epoch::library_key(PRIMARY))
                    .await
                    .unwrap()
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(epoch["epoch"], origin.epoch);
            assert_eq!(epoch["attempt"], origin.attempt);
            let current_config = builder(
                download::SyncMode::Full,
                Arc::new(rustc_hash::FxHashSet::default()),
                Arc::new(download::AssetGroupings::default()),
                Arc::from(PRIMARY),
            );
            let current_plan = download::catalog_plan_context_for_source(
                &states[0].plan.passes,
                &current_config,
                download::DownloadControls::download_hidden(),
                None,
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(
                origin.plan_digest,
                download::catalog_plan_digest_from_source_context(&current_plan)
            );
            assert_eq!(current_plan.zone["zoneName"], PRIMARY);
            assert_eq!(origin.scope(), before.1);
            db.acquire_lock("simulate future mode on owner-created catalog round").unwrap().execute(
                "UPDATE source_revalidation_rounds SET mode='future_catalog_mode' WHERE id=?1 AND mode='catalog_v2'", [&before.0],
            ).unwrap();
            let quiet = crate::sync_loop::runner::quiet_catalog_cycle(
                &[&states[0], &states[1]],
                &builder,
                download::DownloadControls::download_hidden(),
                &download::compute_config_hash(&run_config),
                &shutdown,
            )
            .await
            .unwrap()
            .expect("current unknown catalog-mode debt must be visible");
            assert_eq!(quiet.failed_count, 0);
            assert!(!quiet.operational_safe_holds);
            let notifier = crate::notifications::Notifier::new(None);
            let report_path = dir.path().join("unknown-catalog-report.json");
            let reporter = crate::cycle_reporter::CycleReporter::new(
                crate::cycle_reporter::CycleReporterConfig {
                    watch_mode: true,
                    report_path: Some(&report_path),
                    run_options: crate::report::RunOptions::from_config(&run_config),
                    health_dir: dir.path(),
                    personality_mode: crate::personality::Mode::Off,
                    state_db: Some(db.as_ref()),
                    metrics_handle: None,
                    notifier: &notifier,
                },
            );
            let mut health = crate::health::HealthStatus::new();
            reporter
                .report_completed_cycle(
                    &mut health,
                    crate::cycle_reporter::CycleFacts::from_cycle_result(
                        &quiet,
                        std::time::Duration::ZERO,
                    ),
                )
                .await;
            let report: Value =
                serde_json::from_slice(&std::fs::read(report_path).unwrap()).unwrap();
            assert_eq!(report["status"], "partial_failure");
            assert_eq!(report["stats"]["failed"], 0);
            assert!(health.last_success_at.is_none());
            let remaining = crate::sync_loop::runner::drain_catalog_source_revalidation(
                &[&states[0], &states[1]],
                &builder,
                download::DownloadControls::download_hidden(),
                &download::compute_config_hash(&run_config),
                &shutdown,
            )
            .await
            .unwrap();
            assert_eq!(remaining.retained_sources, vec![PRIMARY]);
            assert_eq!(
                snapshot(&db),
                before,
                "unknown mode must preserve exact identity and deadline"
            );
            assert_eq!(
                events
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|event| **event == "lookup")
                    .count(),
                before_lookups
            );
            for (path, modified) in &published {
                assert_eq!(std::fs::read(path).unwrap(), MEDIA);
                assert_eq!(
                    &std::fs::metadata(path).unwrap().modified().unwrap(),
                    modified
                );
            }
            return;
        }
        if fault == OriginFault::FutureDebt && phase > recovery_phase {
            let lookups_before = events
                .lock()
                .unwrap()
                .iter()
                .filter(|event| **event == "lookup")
                .count();
            states[0].plan_is_stale = true;
            let no_db = |mode, exclude, groups, library| {
                let mut config = (*builder(mode, exclude, groups, library)).clone();
                config.state_db = None;
                Arc::new(config)
            };
            assert!(
                crate::sync_loop::runner::quiet_catalog_cycle(
                    &[&states[0]],
                    &no_db,
                    download::DownloadControls::download_hidden(),
                    &download::compute_config_hash(&run_config),
                    &shutdown,
                )
                .await
                .unwrap()
                .is_none(),
                "absent state must keep its previous shortcut"
            );
            let preview = download::DownloadControls::new(
                download::DownloadRunMode::DryRun,
                download::DownloadReporting::hidden(),
            );
            assert!(
                crate::sync_loop::runner::quiet_catalog_cycle(
                    &[&states[0]],
                    &builder,
                    preview,
                    &download::compute_config_hash(&run_config),
                    &shutdown,
                )
                .await
                .unwrap()
                .is_none(),
                "preview must not acquire catalog health policy"
            );
            let retry_only = |mode, exclude, groups, library| {
                let mut config = (*builder(mode, exclude, groups, library)).clone();
                config.retry_only = true;
                Arc::new(config)
            };
            assert!(
                crate::sync_loop::runner::quiet_catalog_cycle(
                    &[&states[0]],
                    &retry_only,
                    download::DownloadControls::download_hidden(),
                    &download::compute_config_hash(&run_config),
                    &shutdown,
                )
                .await
                .unwrap()
                .is_none(),
                "manual retry keeps its existing selection policy"
            );
            let capture_repair = |mode, exclude, groups, library| {
                let mut config = (*builder(mode, exclude, groups, library)).clone();
                config.capture_timestamp_repair =
                    download::CaptureTimestampRepair::ReplaceWithCaptureLocal;
                Arc::new(config)
            };
            assert!(
                crate::sync_loop::runner::quiet_catalog_cycle(
                    &[&states[0]],
                    &capture_repair,
                    download::DownloadControls::download_hidden(),
                    &download::compute_config_hash(&run_config),
                    &shutdown,
                )
                .await
                .unwrap()
                .is_none(),
                "explicit capture repair keeps its existing selection policy"
            );
            let stale = crate::sync_loop::runner::quiet_catalog_cycle(
                &[&states[0]],
                &builder,
                download::DownloadControls::download_hidden(),
                &download::compute_config_hash(&run_config),
                &shutdown,
            )
            .await
            .unwrap()
            .expect("a stale selected catalog plan must remain a visible hold");
            assert!(!stale.holds.is_empty());
            assert!(!stale.operational_safe_holds);
            states[0].plan_is_stale = false;
            let quiet = crate::sync_loop::runner::quiet_catalog_cycle(
                &[&states[0], &states[1]],
                &builder,
                download::DownloadControls::download_hidden(),
                &download::compute_config_hash(&run_config),
                &shutdown,
            )
            .await
            .unwrap()
            .expect("selected future debt must veto skipped-watch success");
            assert_eq!(quiet.failed_count, 0);
            let notifier = crate::notifications::Notifier::new(None);
            let report_path = dir.path().join("quiet-catalog-report.json");
            let reporter = crate::cycle_reporter::CycleReporter::new(
                crate::cycle_reporter::CycleReporterConfig {
                    watch_mode: true,
                    report_path: Some(&report_path),
                    run_options: crate::report::RunOptions::from_config(&run_config),
                    health_dir: dir.path(),
                    personality_mode: crate::personality::Mode::Off,
                    state_db: Some(db.as_ref()),
                    metrics_handle: None,
                    notifier: &notifier,
                },
            );
            let mut health = crate::health::HealthStatus::new();
            reporter
                .report_completed_cycle(
                    &mut health,
                    crate::cycle_reporter::CycleFacts::from_cycle_result(
                        &quiet,
                        std::time::Duration::ZERO,
                    ),
                )
                .await;
            let report: Value =
                serde_json::from_slice(&std::fs::read(report_path).unwrap()).unwrap();
            assert_eq!(report["status"], "partial_failure");
            assert_eq!(report["stats"]["failed"], 0);
            assert!(health.last_success_at.is_none());
            let remaining = crate::sync_loop::runner::drain_catalog_source_revalidation(
                &[&states[0], &states[1]],
                &builder,
                download::DownloadControls::download_hidden(),
                &download::compute_config_hash(&run_config),
                &shutdown,
            )
            .await
            .unwrap();
            assert_eq!(
                events
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|event| **event == "lookup")
                    .count(),
                lookups_before,
                "future debt must not trigger an early provider lookup"
            );
            assert_eq!(
                remaining.retained_sources,
                vec![PRIMARY],
                "future selected debt remains visible after reopen"
            );
            crate::sync_loop::runner::apply_catalog_drain_outcome(
                &mut result,
                &[&states[0], &states[1]],
                remaining,
                false,
            );
            assert_eq!(result.failed_count, 0);
            assert!(crate::sync_loop::runner::one_shot_partial_error(&result).is_some());
        }
        if phase == recovery_phase {
            let probe = builder(
                download::SyncMode::Full,
                Arc::new(rustc_hash::FxHashSet::default()),
                Arc::new(download::AssetGroupings::default()),
                Arc::from(PRIMARY),
            );
            assert!(
                crate::sync_loop::precheck::has_due_catalog(
                    &states[0],
                    &probe,
                    download::DownloadControls::download_hidden(),
                )
                .await
                .unwrap(),
                "the installed due debt must wake its exact source"
            );
            let mut precheck = crate::sync_loop::precheck::WatchPrecheck::SkipAll;
            crate::sync_loop::precheck::include_pending_provider_work(
                &mut precheck,
                &states,
                &builder,
                download::DownloadControls::download_hidden(),
            )
            .await;
            assert!(precheck.should_sync_zone(PRIMARY));
            if fault == OriginFault::PreparedPublication {
                use crate::state::db::provider_selection::SelectionPath;
                use crate::state::db::{PreparedPublication, TempFileOwnershipStore};
                use sha2::{Digest, Sha256};
                let (version, checksum, size): (String, String, i64) = {
                    let conn = db
                        .acquire_lock("read downloaded publication candidate")
                        .unwrap();
                    conn.query_row("SELECT version_size,checksum,size_bytes FROM assets WHERE library=?1 AND id='asset-primary-old' AND status='downloaded' AND is_deleted=0", [PRIMARY], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?))).unwrap()
                };
                let destination = root.join("prepared-recovery.JPG");
                let temporary = crate::download::test_temp_download_path(
                    &destination,
                    &checksum,
                    ".catalog-protected",
                )
                .unwrap();
                std::fs::write(&temporary, MEDIA).unwrap();
                db.claim_temp_file(&temporary).await.unwrap();
                let digest = data_encoding::HEXLOWER.encode(&Sha256::digest(MEDIA));
                let proof = PreparedPublication {
                    binding: None,
                    library: PRIMARY.to_owned(),
                    id: "asset-primary-old".to_owned(),
                    version,
                    provider_checksum: checksum,
                    provider_size: size.try_into().unwrap(),
                    temporary: SelectionPath::from_path(&temporary),
                    destination: SelectionPath::from_path(&destination),
                    local_checksum: digest.clone(),
                    source_checksum: digest,
                };
                db.prepare_temp_publication(&temporary, &proof)
                    .await
                    .unwrap();
                prepared = Some(proof);
            }
            if fault == OriginFault::DeletedProtected {
                let conn = db.acquire_lock("verify metadata protection").unwrap();
                let protected: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM assets WHERE library=?1 AND id='asset-primary-old' AND metadata_write_failed_at IS NOT NULL",
                    [PRIMARY], |row| row.get(0),
                ).unwrap();
                assert_eq!(
                    protected, 1,
                    "the pending metadata owner must protect the family"
                );
            }
            if clean_metadata {
                let conn = db
                    .acquire_lock("verify unprotected deletion fixture")
                    .unwrap();
                let markers: (i64, i64) = conn.query_row(
                    "SELECT (SELECT COUNT(*) FROM assets WHERE library=?1 AND id='asset-primary-old' AND metadata_write_failed_at IS NOT NULL), (SELECT COUNT(*) FROM asset_metadata_paths WHERE library=?1 AND id='asset-primary-old' AND metadata_write_failed_at IS NOT NULL)",
                    [PRIMARY], |row| Ok((row.get(0)?, row.get(1)?)),
                ).unwrap();
                assert_eq!(
                    markers,
                    (0, 0),
                    "positive fixture must naturally have no metadata failure"
                );
                let native: String = conn
                    .query_row(
                        "SELECT native_path FROM download_receipts WHERE child='asset-primary-old'",
                        [],
                        |row| row.get(0),
                    )
                    .unwrap();
                let media = serde_json::from_str::<
                    crate::state::db::provider_selection::SelectionPath,
                >(&native)
                .unwrap()
                .to_path();
                let mut sidecar = media.into_os_string();
                sidecar.push(".xmp");
                let sidecar = std::path::PathBuf::from(sidecar);
                sidecar_snapshot = Some((
                    sidecar.clone(),
                    std::fs::read(&sidecar).unwrap(),
                    std::fs::metadata(&sidecar).unwrap().modified().unwrap(),
                ));
            }
            let lookups_before = events
                .lock()
                .unwrap()
                .iter()
                .filter(|event| **event == "lookup")
                .count();
            let drained = crate::sync_loop::runner::drain_catalog_source_revalidation(
                &[&states[0], &states[1]],
                &builder,
                download::DownloadControls::download_hidden(),
                &download::compute_config_hash(&run_config),
                &shutdown,
            )
            .await
            .unwrap();
            if fault == OriginFault::ManyDebt {
                assert_eq!(drained.retained, 65);
                assert_eq!(drained.retained_sources, vec![PRIMARY]);
                assert_eq!(
                    events
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|event| **event == "lookup")
                        .count()
                        - lookups_before,
                    64
                );
                let conn = db
                    .acquire_lock("verify 65 actual catalog candidates")
                    .unwrap();
                let (debt, due, intact): (i64, i64, i64) = conn.query_row(
                    "SELECT (SELECT COUNT(*) FROM source_revalidation_debt d JOIN source_revalidation_rounds r ON r.id=d.round WHERE r.library=?1 AND r.phase='active'), (SELECT COUNT(*) FROM source_revalidation_debt d JOIN source_revalidation_rounds r ON r.id=d.round WHERE r.library=?1 AND r.phase='active' AND d.next_retry_at<=?2), (SELECT COUNT(*) FROM assets WHERE library=?1 AND status='downloaded' AND is_deleted=0)",
                    rusqlite::params![PRIMARY, chrono::Utc::now().timestamp()], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
                ).unwrap();
                assert_eq!((debt, due, intact), (65, 1, 66));
                drop(conn);
                crate::sync_loop::runner::apply_catalog_drain_outcome(
                    &mut result,
                    &[&states[0], &states[1]],
                    drained,
                    false,
                );
                assert_eq!(result.failed_count, 0);
                assert!(crate::sync_loop::runner::one_shot_partial_error(&result).is_some());
                return;
            }
            if fault == OriginFault::PreparedPublication {
                let conn = db
                    .acquire_lock("verify deletion and publication custody")
                    .unwrap();
                let state: (i64, i64, i64) = conn.query_row(
                    "SELECT (SELECT is_deleted FROM assets WHERE library=?1 AND id='asset-primary-old'), (SELECT COUNT(*) FROM source_revalidation_debt d JOIN source_revalidation_rounds r ON r.id=d.round WHERE r.library=?1 AND r.phase='active' AND d.id='asset-primary-old'), (SELECT COUNT(*) FROM owned_temp_files WHERE publication IS NOT NULL AND json_extract(publication,'$.library')=?1 AND json_extract(publication,'$.id')='asset-primary-old')",
                    [PRIMARY], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
                ).unwrap();
                assert_eq!(
                    state,
                    (0, 1, 1),
                    "the downloaded asset, active debt and prepared custody must remain"
                );
            }
            assert_eq!(
                drained.session_expired,
                matches!(
                    fault,
                    OriginFault::AuthLookup
                        | OriginFault::RecordAuthMaster
                        | OriginFault::RecordAuthChild
                )
            );
            assert_eq!(drained.retained, usize::from(deferred));
            assert_eq!(
                drained.retained_sources,
                if deferred { vec![PRIMARY] } else { Vec::new() },
                "the hold must identify the source with retained work"
            );
            crate::sync_loop::runner::apply_catalog_drain_outcome(
                &mut result,
                &[&states[0], &states[1]],
                drained,
                shutdown.is_cancelled(),
            );
            if deferred {
                assert_eq!(
                    result.failed_count, 0,
                    "a catalog hold is not a failed asset"
                );
                assert!(crate::sync_loop::runner::one_shot_partial_error(&result).is_some());
                assert!(!result.can_advance_database_checkpoint());
            } else if matches!(
                fault,
                OriginFault::AuthLookup
                    | OriginFault::RecordAuthMaster
                    | OriginFault::RecordAuthChild
            ) {
                assert!(result.session_expired);
                assert!(!result.can_advance_database_checkpoint());
                assert!(
                    crate::sync_loop::runner::one_shot_partial_error(&result).is_none(),
                    "reauthentication must retain priority"
                );
            } else if fault == OriginFault::CancelLookup {
                assert!(result.stats.interrupted);
                assert!(!result.can_advance_database_checkpoint());
                assert!(!result.operational_safe_holds);
                assert!(
                    crate::sync_loop::runner::one_shot_partial_error(&result).is_none(),
                    "interruption must retain priority"
                );
            }
            assert_eq!(
                crate::sync_loop::precheck::has_due_catalog(
                    &states[0],
                    &probe,
                    download::DownloadControls::download_hidden(),
                )
                .await
                .unwrap(),
                matches!(
                    fault,
                    OriginFault::AuthLookup
                        | OriginFault::CancelLookup
                        | OriginFault::RecordAuthMaster
                        | OriginFault::RecordAuthChild
                ),
                "authentication and cancellation retain due debt; settled or deferred work is not immediately due"
            );
        }
        if let Some(proof) = &prepared {
            use crate::state::db::TempFileOwnershipStore;
            let version = crate::state::VersionSizeKey::from_str(&proof.version).unwrap();
            assert_eq!(
                db.get_prepared_publications(PRIMARY, &proof.id, version)
                    .await
                    .unwrap(),
                vec![proof.clone()]
            );
            assert_eq!(std::fs::read(proof.temporary.to_path()).unwrap(), MEDIA);
        }
        let conn = db.acquire_lock("verify expiry checkpoint").unwrap();
        let epoch = crate::state::db::sync_epoch::read_library(&conn, PRIMARY)
            .unwrap()
            .unwrap();
        assert_eq!(epoch.state, "complete");
        if let Some(previous) = &attempt {
            assert_eq!(&epoch.attempt, previous);
        } else {
            attempt = Some(epoch.attempt);
        }
        if phase == 0 {
            let token: String = conn
                .query_row(
                    "SELECT value FROM metadata WHERE key=?1",
                    [sync_token_key(PRIMARY)],
                    |row| row.get(0),
                )
                .unwrap();
            let descriptor: String = conn
                .query_row(
                    "SELECT value FROM metadata WHERE key=?1",
                    [format!("observed_selection:v27:{PRIMARY}")],
                    |row| row.get(0),
                )
                .unwrap();
            original_active = Some((token, descriptor));
            let origin: (String, String, String, String) = conn.query_row(
                "SELECT r.id,r.scope,r.boundary,r.phase FROM source_revalidation_rounds r JOIN metadata m ON m.key=?2 AND json_extract(m.value,'$.revalidation_round')=r.id WHERE r.library=?1 AND r.mode='catalog_v2'",
                rusqlite::params![PRIMARY, format!("observed_selection:v27:{PRIMARY}")],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            ).unwrap();
            assert_eq!(
                origin.3, "active",
                "initial pending baseline must activate its origin"
            );
            initial_origin = Some(origin);
            if matches!(
                fault,
                OriginFault::ReplacementError
                    | OriginFault::ReplacementHoldError
                    | OriginFault::BridgeError
                    | OriginFault::BridgeHoldError
            ) {
                for id in ["primary-old", "shared-old"] {
                    let native: String = conn
                        .query_row(
                            "SELECT native_path FROM download_receipts WHERE child=?1",
                            [format!("asset-{id}")],
                            |row| row.get(0),
                        )
                        .unwrap();
                    let path = serde_json::from_str::<
                        crate::state::db::provider_selection::SelectionPath,
                    >(&native)
                    .unwrap()
                    .to_path();
                    assert_eq!(std::fs::read(&path).unwrap(), MEDIA);
                    let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
                    published.push((path, modified));
                }
            }
        }
        if interrupted && phase == 1 {
            let token: String = conn
                .query_row(
                    "SELECT value FROM metadata WHERE key=?1",
                    [sync_token_key(PRIMARY)],
                    |row| row.get(0),
                )
                .unwrap();
            let descriptor: String = conn
                .query_row(
                    "SELECT value FROM metadata WHERE key=?1",
                    [format!("observed_selection:v27:{PRIMARY}")],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(Some((token, descriptor)), original_active);
            let origins: i64 = conn.query_row("SELECT COUNT(*) FROM source_revalidation_rounds WHERE library=?1 AND mode='catalog_v2'", [PRIMARY], |row| row.get(0)).unwrap();
            assert_eq!(origins, 1 + i64::from(fault != OriginFault::Write));
            let (id, scope, boundary, _) = initial_origin.as_ref().unwrap();
            let saved: (String, String, String) = conn
                .query_row(
                    "SELECT scope,boundary,phase FROM source_revalidation_rounds WHERE id=?1",
                    [id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .unwrap();
            assert_eq!(
                (&saved.0, &saved.1),
                (scope, boundary),
                "the original catalog origin must remain intact"
            );
            assert_eq!(
                saved.2,
                if fault == OriginFault::Write {
                    "active"
                } else {
                    "abandoned"
                }
            );
            let shared_token: String = conn
                .query_row(
                    "SELECT value FROM metadata WHERE key=?1",
                    [sync_token_key(SHARED)],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                shared_token,
                format!("{SHARED}-2"),
                "the healthy peer must progress"
            );
            if fault == OriginFault::Write {
                assert!(!events.lock().unwrap().contains(&"replacement-query"));
                conn.execute_batch("DROP TRIGGER fail_fresh_catalog_origin")
                    .unwrap();
            } else {
                if fault == OriginFault::CheckpointGuard {
                    conn.execute_batch("DROP TRIGGER required_origin_interleave")
                        .unwrap();
                    let boundary: String = conn.query_row("SELECT boundary FROM source_revalidation_rounds WHERE library=?1 AND mode='catalog_v2' AND phase='observing'", [PRIMARY], |row| row.get(0)).unwrap();
                    assert_eq!(
                        boundary,
                        crate::state::db::source_revalidation::CatalogOrigin::boundary(
                            "fresh-primary"
                        ),
                        "checkpoint interleave must roll back"
                    );
                    let debt: i64 = conn.query_row("SELECT COUNT(*) FROM source_revalidation_debt d JOIN source_revalidation_rounds r ON r.id=d.round WHERE r.library=?1 AND r.phase='observing'", [PRIMARY], |row| row.get(0)).unwrap();
                    assert_eq!(
                        debt, 1,
                        "checkpoint rollback must retain the observing census debt"
                    );
                }
                if fault == OriginFault::RepeatBridge {
                    assert!(events.lock().unwrap().contains(&"bridge-rejected"));
                } else {
                    let recorded = events.lock().unwrap();
                    assert!(recorded.contains(&"fresh-bridge"));
                    assert_eq!(
                        recorded
                            .iter()
                            .filter(|event| **event == "inventory-start")
                            .count(),
                        1
                    );
                }
                let observing: i64 = conn.query_row("SELECT COUNT(*) FROM source_revalidation_rounds WHERE library=?1 AND mode='catalog_v2' AND phase='observing'", [PRIMARY], |row| row.get(0)).unwrap();
                assert_eq!(
                    observing, 1,
                    "rejected bridge must retain its unactivated origin"
                );
            }
        }
        if phase == recovery_phase {
            let (mode, current, nonce): (String, String, String) = conn.query_row(
                "SELECT r.mode,r.phase,r.id FROM source_revalidation_rounds r JOIN metadata m ON m.key=?2 AND json_extract(m.value,'$.revalidation_round')=r.id WHERE r.library=?1 AND r.phase='active'", rusqlite::params![PRIMARY, format!("observed_selection:v27:{PRIMARY}")],
                |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?))).unwrap();
            assert_eq!((mode.as_str(), current.as_str()), ("catalog_v2", "active"));
            assert!(!nonce.is_empty());
            let debt: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM source_revalidation_debt WHERE round=?1",
                    [&nonce],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                debt,
                i64::from(
                    deferred
                        || matches!(
                            fault,
                            OriginFault::AuthLookup
                                | OriginFault::CancelLookup
                                | OriginFault::RecordAuthMaster
                                | OriginFault::RecordAuthChild
                        )
                )
            );
            if confirmed_deletion {
                let (old_deleted, new_deleted): (i64, i64) = conn
                    .query_row(
                        "SELECT (SELECT is_deleted FROM assets WHERE library=?1 AND id='asset-primary-old'), (SELECT is_deleted FROM assets WHERE library=?1 AND id='asset-primary-new')",
                        [PRIMARY],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .unwrap();
                assert_eq!(
                    (old_deleted, new_deleted),
                    (1, 0),
                    "only the confirmed old source is marked deleted"
                );
                let peer_deleted: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM assets WHERE library=?1 AND is_deleted=1",
                        [SHARED],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert_eq!(peer_deleted, 0);
            }
            if deferred
                || matches!(
                    fault,
                    OriginFault::AuthLookup
                        | OriginFault::CancelLookup
                        | OriginFault::RecordAuthMaster
                        | OriginFault::RecordAuthChild
                )
            {
                let deadline: i64 = conn
                    .query_row(
                        "SELECT next_retry_at FROM source_revalidation_debt WHERE round=?1",
                        [&nonce],
                        |row| row.get(0),
                    )
                    .unwrap();
                if deferred {
                    assert!(deadline > chrono::Utc::now().timestamp());
                } else {
                    assert_eq!(
                        deadline, 0,
                        "authentication or cancellation must not schedule or acknowledge the candidate"
                    );
                }
                retained_debt = Some((nonce.clone(), deadline));
                let deleted: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM assets WHERE library=?1 AND is_deleted=1",
                        [PRIMARY],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert_eq!(
                    deleted, 0,
                    "protected or unconfirmed source evidence must not authorize deletion"
                );
            }
            let shared_token: String = conn
                .query_row(
                    "SELECT value FROM metadata WHERE key=?1",
                    [sync_token_key(SHARED)],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(shared_token, format!("{SHARED}-{}", phase + 1));
            let recorded = events.lock().unwrap();
            let rejected = recorded
                .iter()
                .position(|event| *event == "rejected")
                .unwrap();
            let fresh = recorded
                .iter()
                .position(|event| *event == "fresh-boundary")
                .unwrap();
            let inventory = recorded
                .iter()
                .position(|event| *event == "replacement-query")
                .unwrap();
            let bridge = recorded
                .iter()
                .position(|event| *event == "fresh-bridge")
                .unwrap();
            assert!(rejected < fresh && fresh < inventory && inventory < bridge);
            if fault == OriginFault::PreBoundaryDeleted {
                let omitted = recorded
                    .iter()
                    .position(|event| *event == "old-omitted-from-inventory")
                    .unwrap();
                assert!(
                    inventory <= omitted && omitted < bridge,
                    "pre-boundary deletion must be absent from replacement inventory"
                );
            }
            assert_eq!(
                recorded
                    .iter()
                    .filter(|event| **event == "inventory-start")
                    .count(),
                if matches!(
                    fault,
                    OriginFault::RepeatBridge
                        | OriginFault::RankChange
                        | OriginFault::CheckpointGuard
                ) {
                    2
                } else {
                    1
                },
                "each wake must run at most one replacement inventory"
            );
            if interrupted {
                assert_eq!(
                    recorded
                        .iter()
                        .filter(|event| **event == "rejected")
                        .count(),
                    if fault == OriginFault::Write { 2 } else { 1 },
                    "an installed origin must not replay the rejected retained cursor"
                );
                assert_eq!(
                    recorded
                        .iter()
                        .filter(|event| **event == "fresh-boundary")
                        .count(),
                    if fault == OriginFault::Write { 2 } else { 1 },
                    "an installed origin must use its saved boundary, not request a later one"
                );
            }
            assert!(
                recorded
                    .iter()
                    .rposition(|event| *event == "lookup")
                    .unwrap()
                    > bridge
            );
            for id in ["primary-old", "primary-new", "shared-old", "shared-new"] {
                let native: String = conn
                    .query_row(
                        "SELECT native_path FROM download_receipts WHERE child=?1",
                        [format!("asset-{id}")],
                        |row| row.get(0),
                    )
                    .unwrap();
                let path = serde_json::from_str::<
                    crate::state::db::provider_selection::SelectionPath,
                >(&native)
                .unwrap()
                .to_path();
                assert!(path.starts_with(&root));
                assert_eq!(std::fs::read(&path).unwrap(), MEDIA);
                let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
                published.push((path, modified));
            }
        } else if phase > recovery_phase {
            if let Some((nonce, deadline)) = &retained_debt {
                let saved: i64 = conn
                    .query_row(
                        "SELECT next_retry_at FROM source_revalidation_debt WHERE round=?1",
                        [nonce],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert_eq!(
                    &saved, deadline,
                    "quiet reopens must preserve refused debt and its deadline"
                );
            }
            for (path, modified) in &published {
                assert_eq!(std::fs::read(path).unwrap(), MEDIA);
                assert_eq!(
                    &std::fs::metadata(path).unwrap().modified().unwrap(),
                    modified
                );
            }
            if let Some(proof) = &prepared {
                assert_eq!(std::fs::read(proof.temporary.to_path()).unwrap(), MEDIA);
                let deleted: i64 = conn
                    .query_row(
                        "SELECT is_deleted FROM assets WHERE library=?1 AND id=?2",
                        rusqlite::params![PRIMARY, proof.id],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert_eq!(deleted, 0);
            }
        }
        if let Some((path, bytes, modified)) = &sidecar_snapshot {
            assert_eq!(&std::fs::read(path).unwrap(), bytes);
            assert_eq!(
                &std::fs::metadata(path).unwrap().modified().unwrap(),
                modified
            );
        }
        drop(conn);
    }
    assert_eq!(published.len(), 4);
    server.verify().await;
}

const EOF_ZONE: &str = "PrimarySync-eof-bootstrap";

#[derive(Clone)]
struct EofBootstrapSession {
    db: Arc<SqliteStateDb>,
    events: Arc<Mutex<Vec<&'static str>>>,
    zone: Value,
    successor: &'static str,
}

#[async_trait::async_trait]
impl PhotosSession for EofBootstrapSession {
    async fn post(&self, url: &str, body: String, _: &[(&str, &str)]) -> anyhow::Result<Value> {
        let request: Value = serde_json::from_str(&body)?;
        let zone = self.zone.clone();
        if request["query"]["recordType"] == "CheckIndexingState" {
            return Ok(
                json!({"records":[{"recordName":"index","fields":{"state":{"value":"FINISHED"}}}]}),
            );
        }
        if url.contains("/zones/list") {
            self.events.lock().unwrap().push("zones-list");
            return Ok(
                json!({"zones":[{"zoneID":zone,"syncToken":"initial-boundary","deleted":false}]}),
            );
        }
        if url.contains("/internal/records/query/batch") {
            let batch: Vec<_> = request["batch"]
                .as_array()
                .unwrap()
                .iter()
                .map(|_| json!({"records":[{"fields":{"itemCount":{"value":0}}}]}))
                .collect();
            return Ok(json!({"batch":batch}));
        }
        if url.contains("/records/query?") {
            if request["query"]["recordType"] == "CPLAlbumByPositionLive" {
                return Ok(json!({"records":[]}));
            }
            let mut events = self.events.lock().unwrap();
            if events.contains(&"delta-eof") {
                let conn = self.db.acquire_lock("check EOF census before inventory")?;
                let observing: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM source_revalidation_rounds WHERE library=?1 AND mode='catalog_v2' AND phase='observing'",
                    [self.zone["zoneName"].as_str().unwrap()], |row| row.get(0),
                )?;
                assert_eq!(observing, 1, "EOF origin must be seeded before inventory");
                events.push("bootstrap-inventory");
            } else {
                events.push("initial-inventory");
            }
            return Ok(json!({"records":[],"syncToken":"inventory-page"}));
        }
        if url.contains("/changes/zone?") {
            let token = request["zones"][0]["syncToken"].as_str().unwrap();
            let mut events = self.events.lock().unwrap();
            let next = if token == "initial-boundary" {
                "prior-T"
            } else if events.contains(&"bootstrap-inventory") && token == self.successor {
                events.push("bootstrap-bridge");
                "bridged-final"
            } else if token == "prior-T" {
                events.push("delta-eof");
                self.successor
            } else if token == "bridged-final" {
                events.push("quiet-delta");
                "bridged-final"
            } else {
                anyhow::bail!("unexpected synthetic EOF cursor")
            };
            return Ok(
                json!({"zones":[{"zoneID":zone,"records":[],"syncToken":next,"moreComing":false}]}),
            );
        }
        anyhow::bail!("unexpected synthetic EOF endpoint")
    }

    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

#[tokio::test]
async fn valid_token_bootstraps_from_actual_eof_and_reopens_quietly() {
    for (zone_name, database_kind, zone, successor) in [
        (
            EOF_ZONE,
            "private",
            json!({"zoneName":EOF_ZONE,"ownerRecordName":"_defaultOwner"}),
            "after-D",
        ),
        (
            EOF_ZONE,
            "private",
            json!({"zoneName":EOF_ZONE,"ownerRecordName":"_defaultOwner"}),
            "prior-T",
        ),
        (
            "SharedSync-eof-bootstrap",
            "shared",
            json!({"zoneName":"SharedSync-eof-bootstrap","ownerRecordName":"shared-owner","zoneType":"synthetic-shared-zone"}),
            "after-D",
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("media");
        std::fs::create_dir(&root).unwrap();
        let database = dir.path().join("state.db");
        let events = Arc::new(Mutex::new(Vec::new()));
        let (_session_dir, session) = make_shared_session_for_run_cycle().await;
        let mut initial_descriptor = None;
        for phase in 0..4 {
            let db = Arc::new(
                SqliteStateDb::open_owned(&database, &baseline_owner())
                    .await
                    .unwrap(),
            );
            let mut library = PhotoLibrary::new(
                format!("https://synthetic.example/database/1/photos/production/{database_kind}"),
                Arc::default(),
                Box::new(EofBootstrapSession {
                    db: db.clone(),
                    events: events.clone(),
                    zone: zone.clone(),
                    successor,
                }),
                Arc::new(zone.clone()),
                database_kind.into(),
                crate::retry::RetryConfig::default(),
            )
            .await
            .unwrap();
            library.shadow_capture = Some(ShadowCapture::new(db.clone(), baseline_owner(), "com"));
            if phase == 0 {
                db.enroll_sync_epoch(vec![(zone_name.into(), library.baseline_scope())])
                    .await
                    .unwrap();
            }
            let hidden = library.albums().await.unwrap().remove("Hidden").unwrap();
            let pass = crate::commands::AlbumPass {
                kind: PassKind::SmartFolder,
                album: hidden,
                exclude_ids: Arc::default(),
            };
            let mut state = make_run_cycle_library_state_with_passes(
                zone_name,
                &sync_token_key(zone_name),
                vec![pass],
            );
            state.library = library;
            let builder = make_run_cycle_download_config_builder(&root, db.clone());
            let result = run_cycle(
                &[&state],
                &make_run_cycle_config(),
                Some(db.as_ref()),
                false,
                &builder,
                download::DownloadControls::download_hidden(),
                &session,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
            assert!(
                !result.session_expired && !result.stats.interrupted,
                "{result:?}"
            );
            if phase == 0 {
                let raw = db
                    .get_metadata(&format!("observed_selection:v27:{zone_name}"))
                    .await
                    .unwrap()
                    .unwrap();
                let descriptor: Value = serde_json::from_str(&raw).unwrap();
                assert!(
                    descriptor.get("revalidation_round").is_none(),
                    "the initial real baseline must have no catalog link: {:?}",
                    descriptor.get("revalidation_round")
                );
                initial_descriptor = Some(raw);
                assert_eq!(
                    db.get_metadata(&sync_token_key(zone_name))
                        .await
                        .unwrap()
                        .as_deref(),
                    Some("prior-T")
                );
            }
            if phase == 1 {
                let observed = db
                    .get_metadata(&format!("observed_selection:v27:{zone_name}"))
                    .await
                    .unwrap()
                    .unwrap();
                assert_ne!(
                    Some(&observed),
                    initial_descriptor.as_ref(),
                    "the completed delta must persist a new observation even when T equals D"
                );
                assert_eq!(
                    db.get_metadata(&sync_token_key(zone_name))
                        .await
                        .unwrap()
                        .as_deref(),
                    Some(successor),
                    "the current checkpoint must be the actual completed EOF"
                );
                let conn = db.acquire_lock("verify observed EOF origin").unwrap();
                let marking: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM source_revalidation_rounds r JOIN metadata m ON m.key LIKE 'baseline_cursor:v27:%' AND m.value=?2 WHERE r.library=?1 AND r.mode='catalog_v2' AND r.phase='marking' AND r.boundary=?3",
                    rusqlite::params![zone_name, successor, crate::state::db::source_revalidation::CatalogOrigin::boundary(successor)], |row| row.get(0),
                ).unwrap();
                assert_eq!(
                    marking, 1,
                    "the actual EOF and marking origin must be durable together"
                );
            }
            if phase == 3 {
                let active: i64 = {
                    let conn = db.acquire_lock("verify EOF origin after reopen").unwrap();
                    conn.query_row(
                        "SELECT COUNT(*) FROM source_revalidation_rounds r JOIN metadata m ON m.key=?2 AND json_extract(m.value,'$.revalidation_round')=r.id WHERE r.library=?1 AND r.mode='catalog_v2' AND r.phase='active'",
                        rusqlite::params![zone_name, format!("observed_selection:v27:{zone_name}")], |row| row.get(0),
                    ).unwrap()
                };
                assert_eq!(
                    active, 1,
                    "actual EOF must lead to a current active catalog origin"
                );
                assert_eq!(
                    db.get_metadata(&sync_token_key(zone_name))
                        .await
                        .unwrap()
                        .as_deref(),
                    Some("bridged-final")
                );
            }
        }
        let events = events.lock().unwrap();
        let eof = events
            .iter()
            .position(|event| *event == "delta-eof")
            .unwrap();
        let inventory = events
            .iter()
            .position(|event| *event == "bootstrap-inventory")
            .unwrap();
        let bridge = events
            .iter()
            .position(|event| *event == "bootstrap-bridge")
            .unwrap();
        assert!(eof < inventory && inventory < bridge);
        assert!(events.contains(&"quiet-delta"));
        assert_eq!(
            events
                .iter()
                .filter(|event| **event == "zones-list")
                .count(),
            1,
            "EOF bootstrap must not request a replacement boundary"
        );
    }
}
