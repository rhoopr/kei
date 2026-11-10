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
            }
            return Ok(
                json!({"zones":[{"zoneID":source,"syncToken":if self.phase > 0 && self.primary {"fresh-primary".to_owned()} else {format!("{name}-boundary")},"deleted":false}]}),
            );
        }
        if url.contains("/internal/records/query/batch") {
            let count = if self.primary
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
            if self.fault == OriginFault::WrongScope
                && self.primary
                && self.events.lock().unwrap().contains(&"fresh-bridge")
            {
                for record in &mut records {
                    record["zoneID"]["ownerRecordName"] = json!("foreign-owner");
                }
            }
            if matches!(
                self.fault,
                OriginFault::DeletedLookup
                    | OriginFault::DeletedProtected
                    | OriginFault::PreBoundaryDeleted
            ) && self.primary
                && self.events.lock().unwrap().contains(&"fresh-bridge")
            {
                for record in &mut records {
                    record["fields"]["isDeleted"] = json!({"value":1,"type":"INT64"});
                    record["fields"]["isExpunged"] = json!({"value":1,"type":"INT64"});
                    record["fields"]["dateExpunged"] =
                        json!({"value":1792541032000_i64,"type":"TIMESTAMP"});
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
    PreBoundaryDeleted,
    MarkerWriterProbe,
    DeletedProtected,
    WrongScope,
    AuthLookup,
    CancelLookup,
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

#[cfg(feature = "xmp")]
#[tokio::test]
async fn m1_strict_deleted_pair_settles_only_confirmed_source_without_removing_media() {
    exercise_expiry(OriginFault::DeletedLookup).await;
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
        if fault == OriginFault::MarkerWriterProbe {
            mock.mount(&server).await;
        } else {
            mock.expect(1).mount(&server).await;
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
    let mut retained_debt = None;
    let mut sidecar_snapshot = None;
    let mut baseline_marker = None;
    let deferred = matches!(
        fault,
        OriginFault::WrongScope | OriginFault::DeletedProtected
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
        OriginFault::DeletedLookup | OriginFault::PreBoundaryDeleted
    );
    let recovery_phase = if interrupted { 2 } else { 1 };
    for phase in 0..recovery_phase + 3 {
        let db = Arc::new(
            SqliteStateDb::open_owned(&database, &baseline_owner())
                .await
                .unwrap(),
        );
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
            SourceSession {
                primary,
                phase,
                fault,
                db: db.clone(),
                cancel: shutdown.clone(),
                events: events.clone(),
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
        if confirmed_deletion {
            run_config.runtime.refresh_metadata = phase == 0;
            run_config.metadata.xmp_sidecar = true;
        }
        let builder = |mode, exclude, groups, library| {
            let mut config = (*base(mode, exclude, groups, library)).clone();
            config.folder_structure = "none".into();
            if confirmed_deletion {
                config.metadata = crate::config::MetadataConfig::default();
            }
            #[cfg(feature = "xmp")]
            {
                config.metadata.xmp_sidecar = confirmed_deletion;
            }
            Arc::new(config)
        };
        let trace = (fault == OriginFault::MarkerWriterProbe)
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
        if let Some((capture, _guard)) = &trace {
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
        let mut result = result.unwrap();
        if confirmed_deletion && phase == 0 {
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
            (_, 0)
            | (
                OriginFault::None
                | OriginFault::RepeatBridge
                | OriginFault::RankChange
                | OriginFault::DeletedLookup
                | OriginFault::PreBoundaryDeleted
                | OriginFault::CheckpointGuard
                | OriginFault::DeletedProtected
                | OriginFault::WrongScope
                | OriginFault::AuthLookup
                | OriginFault::CancelLookup,
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
            if confirmed_deletion {
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
            let drained = crate::sync_loop::runner::drain_catalog_source_revalidation(
                &[&states[0], &states[1]],
                &builder,
                download::DownloadControls::download_hidden(),
                &download::compute_config_hash(&run_config),
                &shutdown,
            )
            .await
            .unwrap();
            assert_eq!(drained.session_expired, fault == OriginFault::AuthLookup);
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
            } else if fault == OriginFault::AuthLookup {
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
                matches!(fault, OriginFault::AuthLookup | OriginFault::CancelLookup),
                "authentication and cancellation retain due debt; settled or deferred work is not immediately due"
            );
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
            assert_eq!(origins, i64::from(fault != OriginFault::Write));
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
                        || matches!(fault, OriginFault::AuthLookup | OriginFault::CancelLookup)
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
            if deferred || matches!(fault, OriginFault::AuthLookup | OriginFault::CancelLookup) {
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
                    2
                );
                assert_eq!(
                    recorded
                        .iter()
                        .filter(|event| **event == "fresh-boundary")
                        .count(),
                    2
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
