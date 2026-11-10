//! Normal-cycle reachability of an existing preservation hold, not health certification.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use wiremock::{
    Mock, ResponseTemplate,
    matchers::{method, path},
};

use super::{MEDIA, ZONE, baseline_owner, pair, zone};
use crate::commands::{AlbumPass, PassKind};
use crate::download;
use crate::icloud::photos::{PhotoLibrary, PhotosSession};
use crate::state::SqliteStateDb;
use crate::state::db::LegacyFileEvidence;
use crate::sync_cycle::{CycleResult, LibraryHoldKind, run_cycle, sync_token_key};
use crate::sync_loop::test_support::{
    album_count_response, make_run_cycle_config, make_run_cycle_download_config_builder,
    make_run_cycle_library_state_with_passes,
};

const OLD: &str = "old-preservation-master";

#[derive(Clone)]
struct Session {
    baseline: Arc<Vec<Value>>,
    cycle: usize,
    changes: Arc<Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl PhotosSession for Session {
    async fn post(&self, url: &str, body: String, _: &[(&str, &str)]) -> anyhow::Result<Value> {
        let request: Value = serde_json::from_str(&body)?;
        if request["query"]["recordType"] == "CheckIndexingState" {
            return Ok(
                json!({"records":[{"recordName":"index","fields":{"state":{"value":"FINISHED"}}}]}),
            );
        }
        if url.contains("/zones/list") {
            return Ok(
                json!({"zones":[{"zoneID":zone(),"syncToken":"preservation-boundary","deleted":false}]}),
            );
        }
        if url.contains("/internal/records/query/batch") {
            return Ok(album_count_response(1));
        }
        if url.contains("/records/query?") {
            let offset = request["query"]["filterBy"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|item| item["fieldName"] == "startRank")
                .and_then(|item| item["fieldValue"]["value"].as_u64())
                .unwrap_or(0);
            let records = if offset == 0 {
                self.baseline.as_ref().clone()
            } else {
                Vec::new()
            };
            return Ok(json!({"records":records,"syncToken":"preservation-query"}));
        }
        if url.contains("/records/lookup?") {
            let requested = request["records"].as_array().unwrap();
            let records: Vec<_> = self
                .baseline
                .iter()
                .filter(|record| {
                    requested
                        .iter()
                        .any(|item| item["recordName"] == record["recordName"])
                })
                .cloned()
                .collect();
            return Ok(json!({"records":records}));
        }
        if url.contains("/changes/zone?") {
            let cursor = request["zones"][0]["syncToken"].as_str().unwrap();
            let expected = if self.cycle == 0 {
                "preservation-boundary".to_string()
            } else {
                format!("preservation-after-{}", self.cycle - 1)
            };
            assert_eq!(cursor, expected);
            self.changes.lock().unwrap().push(cursor.to_owned());
            return Ok(
                json!({"zones":[{"zoneID":zone(),"records":[],"syncToken":format!("preservation-after-{}",self.cycle),"moreComing":false}]}),
            );
        }
        anyhow::bail!("unexpected preservation reachability endpoint")
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

async fn cycle(
    db: &Arc<SqliteStateDb>,
    root: &Path,
    shared: &crate::auth::session::SharedSession,
    baseline: &Arc<Vec<Value>>,
    number: usize,
) -> (
    CycleResult,
    String,
    String,
    Vec<String>,
    Option<download::OperationalJournalGuards>,
) {
    let changes = Arc::new(Mutex::new(Vec::new()));
    let mut library = PhotoLibrary::new(
        "https://synthetic.example/database/1/photos/production/private".into(),
        Arc::default(),
        Box::new(Session {
            baseline: Arc::clone(baseline),
            cycle: number,
            changes: Arc::clone(&changes),
        }),
        Arc::new(zone()),
        "private".into(),
        crate::retry::RetryConfig::default(),
    )
    .await
    .unwrap();
    let capture =
        crate::icloud::photos::inbox::ShadowCapture::new(Arc::clone(db), baseline_owner(), "com");
    let scope = capture.scope("private", &zone()).unwrap();
    library.shadow_capture = Some(capture);
    let baseline_scope = library.baseline_scope();
    let pass = AlbumPass {
        kind: PassKind::Unfiled,
        album: library.all(),
        exclude_ids: Arc::default(),
    };
    let mut state =
        make_run_cycle_library_state_with_passes(ZONE, &sync_token_key(ZONE), vec![pass]);
    state.library = library;
    let base = make_run_cycle_download_config_builder(root, db.clone());
    let builder = |mode, exclude, groups, library| {
        let mut config = (*base(mode, exclude, groups, library)).clone();
        config.folder_structure = "none".into();
        config.retry.max_retries = 0;
        #[cfg(feature = "xmp")]
        {
            config.metadata.xmp_sidecar = false;
        }
        Arc::new(config)
    };
    let mut config = make_run_cycle_config();
    config.auth.username = "baseline@example.invalid".to_owned();
    let mut result = run_cycle(
        &[&state],
        &config,
        Some(db.as_ref()),
        false,
        &builder,
        download::DownloadControls::download_hidden(),
        shared,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    let shutdown = CancellationToken::new();
    result
        .prepare_operational_tail(shared, &shutdown)
        .await
        .unwrap();
    crate::sync_loop::runner::drain_source_revalidation(&[&state], &shutdown)
        .await
        .unwrap();
    result.revalidate_sparse_holds(Some(db.as_ref())).await;
    let guards = result
        .finish_operational_tail(shared, &shutdown)
        .await
        .unwrap();
    let cursors = changes.lock().unwrap().clone();
    (result, scope, baseline_scope, cursors, guards)
}

fn retained_snapshot(db: &SqliteStateDb) -> Vec<(&'static str, Vec<Vec<rusqlite::types::Value>>)> {
    let conn = db.acquire_lock("preservation exact retained rows").unwrap();
    [
        ("preservation", "SELECT * FROM unattributed_legacy WHERE library=?1 AND asset_id=?2 ORDER BY library,asset_id"),
        ("paths", "SELECT * FROM unattributed_legacy_paths WHERE library=?1 AND asset_id=?2 ORDER BY path_key"),
        ("proofs", "SELECT * FROM unattributed_legacy_proofs WHERE library=?1 AND asset_id=?2 ORDER BY generation"),
        ("assets", "SELECT * FROM assets WHERE library=?1 AND id=?2 ORDER BY version_size"),
        ("metadata_paths", "SELECT * FROM asset_metadata_paths WHERE library=?1 AND id=?2 ORDER BY version_size,local_path"),
        ("capture_revisions", "SELECT * FROM asset_metadata_capture_revisions WHERE library=?1 AND asset_id=?2 ORDER BY revision"),
        ("retries", "SELECT * FROM metadata_capture_retries WHERE library=?1 AND asset_id=?2 ORDER BY target_revision"),
        ("mappings", "SELECT * FROM asset_master_mappings WHERE library=?1 AND master_record_name=?2 ORDER BY asset_record_name"),
        ("albums", "SELECT * FROM asset_albums WHERE library=?1 AND asset_id=?2 ORDER BY 1,2,3"),
        ("people", "SELECT * FROM asset_people WHERE library=?1 AND asset_id=?2 ORDER BY 1,2,3"),
        ("memberships", "SELECT * FROM asset_album_memberships WHERE library=?1 AND asset_record_name=?2 ORDER BY 1,2,3"),
        ("verifications", "SELECT * FROM asset_verifications WHERE library=?1 AND id=?2 ORDER BY 1,2,3"),
    ].into_iter().map(|(label, sql)| {
        let mut stmt = conn.prepare(sql).unwrap();
        let count = stmt.column_count();
        let rows = stmt.query_map([ZONE, OLD], |row| (0..count).map(|index| row.get(index)).collect())
            .unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
        (label, rows)
    }).collect()
}

async fn retain_legacy(
    db: &SqliteStateDb,
    root: &Path,
    blocked_metadata: bool,
) -> Vec<(PathBuf, Vec<u8>, std::time::SystemTime)> {
    let media = root.join("protected-old.JPG");
    let sidecar = root.join("protected-old.JPG.xmp");
    let contents = [
        (media.clone(), MEDIA.to_vec()),
        (sidecar.clone(), br#"<?xpacket begin=""?><x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about=""/></rdf:RDF></x:xmpmeta><?xpacket end="w"?>"#.to_vec()),
    ];
    let mut files = Vec::new();
    let mut snapshots = Vec::new();
    for (path, bytes) in contents {
        std::fs::write(&path, &bytes).unwrap();
        let hash = download::file::compute_sha256(&path).await.unwrap();
        let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
        files.push(LegacyFileEvidence {
            root: root.to_path_buf(),
            path: path.clone(),
            sha256: Some(hash),
            size: Some(bytes.len() as u64),
        });
        snapshots.push((path, bytes, mtime));
    }
    let record = crate::test_helpers::TestAssetRecord::new(OLD)
        .library(ZONE)
        .checksum("legacy-provider")
        .size(MEDIA.len() as u64)
        .build();
    db.upsert_seen(&record).await.unwrap();
    db.mark_downloaded(
        ZONE,
        OLD,
        "original",
        &media,
        files[0].sha256.as_ref().unwrap(),
        None,
    )
    .await
    .unwrap();
    db.set_metadata_capture_revision_for_test(ZONE, OLD, 0);
    for child in ["historical-child-a", "historical-child-b"] {
        db.upsert_asset_master_mapping(ZONE, child, OLD)
            .await
            .unwrap();
    }
    let candidate = db
        .get_metadata_capture_candidates(ZONE, 1, 64)
        .await
        .unwrap()
        .into_iter()
        .find(|candidate| candidate.asset_id == OLD)
        .expect("legacy metadata candidate");
    assert!(
        db.defer_metadata_capture_ambiguity(&candidate, 1)
            .await
            .unwrap()
    );
    if blocked_metadata {
        db.record_metadata_write_failure(ZONE, OLD, "original")
            .await
            .unwrap();
    }
    let snapshot = db
        .legacy_preparation_snapshots(ZONE, 64)
        .await
        .unwrap()
        .into_iter()
        .find(|snapshot| snapshot.asset_id == OLD)
        .expect("legacy preservation candidate");
    assert!(snapshot.historical_claims.is_empty());
    assert!(
        db.prepare_legacy_preservation(&snapshot, &files)
            .await
            .unwrap()
    );
    snapshots
}

fn assert_files(files: &[(PathBuf, Vec<u8>, std::time::SystemTime)]) {
    for (path, bytes, mtime) in files {
        assert_eq!(&std::fs::read(path).unwrap(), bytes);
        assert_eq!(std::fs::metadata(path).unwrap().modified().unwrap(), *mtime);
    }
}

async fn owner_read(
    db: &SqliteStateDb,
    scope: &str,
    baseline_scope: &str,
    epoch: &str,
    attempt: &str,
) {
    let (zone_name, owner, database) = PhotoLibrary::provider_scope_projection(scope).unwrap();
    let proof = db
        .preservation_hold_test(
            ZONE,
            baseline_owner(),
            epoch,
            attempt,
            scope,
            &zone_name,
            &owner,
            &database,
            baseline_scope,
        )
        .await
        .unwrap();
    assert_eq!(
        proof,
        Ok((1, 2)),
        "actual owner must account exact retained rows and paths"
    );
}

#[allow(
    clippy::print_stdout,
    reason = "bounded synthetic reachability facts are test evidence"
)]
async fn scenario(blocked_metadata: bool) -> Option<(crate::health::HealthStatus, Value)> {
    let server =
        crate::test_helpers::start_wiremock_or_skip("operational_preservation_hold").await?;
    Mock::given(method("GET"))
        .and(path("/current"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/protected-old"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("media");
    std::fs::create_dir(&root).unwrap();
    let database = dir.path().join("state.db");
    let db = Arc::new(
        SqliteStateDb::open_owned(&database, &baseline_owner())
            .await
            .unwrap(),
    );
    let baseline = Arc::new(pair(
        "preservation-current",
        "current.JPG",
        &format!("{}/current", server.uri()),
        41,
    ));
    let mut session = crate::auth::session::Session::new(
        dir.path(),
        "baseline@example.invalid",
        "https://www.icloud.com",
        None,
    )
    .await
    .unwrap();
    session
        .bind_principal(
            &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-baseline"}})).unwrap(),
            None,
        )
        .unwrap();
    let shared = Arc::new(tokio::sync::RwLock::new(session));
    let (initial, scope, baseline_scope, cursors, initial_guards) =
        cycle(&db, &root, &shared, &baseline, 0).await;
    assert_eq!(initial.stats.downloaded, 1, "{initial:?}");
    assert_eq!(initial.failed_count, 0);
    assert!(initial.can_advance_database_checkpoint());
    assert_eq!(cursors, ["preservation-boundary"]);
    let epoch = db.sync_epoch_id().await.unwrap().unwrap();
    let before_epoch = {
        let conn = db.acquire_lock("complete pre-preservation epoch").unwrap();
        crate::state::db::sync_epoch::read_library(&conn, ZONE)
            .unwrap()
            .unwrap()
    };
    assert_eq!(before_epoch.state, "complete");
    assert_eq!(
        db.get_metadata(&format!("download_ledger_scope:{ZONE}"))
            .await
            .unwrap()
            .as_deref(),
        Some(scope.as_str())
    );
    let files = retain_legacy(&db, &root, blocked_metadata).await;
    let retained = retained_snapshot(&db);
    assert_eq!(retained[0].1.len(), 1);
    assert_eq!(retained[1].1.len(), 2);
    assert!(retained[2].1.is_empty());
    assert!(
        retained
            .iter()
            .find(|(name, _)| *name == "retries")
            .is_some_and(|(_, rows)| !rows.is_empty()),
        "preserved legacy retry evidence must remain present"
    );
    {
        let conn = db
            .acquire_lock("no fabricated current legacy proof")
            .unwrap();
        assert_eq!(conn.query_row::<i64, _, _>("SELECT count(*) FROM download_receipts WHERE library=?1 AND (child=?2 OR master=?2 OR state_id=?2)", [ZONE, OLD], |row| row.get(0)).unwrap(), 0);
        assert_eq!(conn.query_row::<i64, _, _>("SELECT count(*) FROM provider_active_destinations WHERE library=?1 AND asset_id=?2", [ZONE, OLD], |row| row.get(0)).unwrap(), 0);
    }
    owner_read(&db, &scope, &baseline_scope, &epoch, &before_epoch.attempt).await;

    let (held, actual_scope, actual_baseline, cursors, held_guards) =
        cycle(&db, &root, &shared, &baseline, 1).await;
    assert_eq!(actual_scope, scope);
    assert_eq!(actual_baseline, baseline_scope);
    assert_eq!(cursors, ["preservation-after-0"]);
    assert_eq!(held.stats.unattributed_legacy_pending, 1, "{held:?}");
    assert_eq!(held.stats.failed, 0, "{held:?}");
    assert_eq!(held.failed_count, 0, "{held:?}");
    assert_eq!(held.stats.state_write_failures, 0);
    assert_eq!(held.stats.enumeration_errors, 0);
    assert!(held.holds.iter().any(|hold| hold.source == ZONE
        && hold.kind == LibraryHoldKind::PreservationPending
        && hold.reason == "legacy_preservation_pending"));
    assert_eq!(held.sources.len(), 1);
    assert_eq!(held.sources[0].zone_name, ZONE);
    assert!(
        held.sources[0].reached && !held.sources[0].genuine_error,
        "{:?}",
        held.sources[0]
    );
    assert_eq!(
        held.sources[0].operational_prework.is_some(),
        !blocked_metadata,
        "prework must distinguish retained preservation evidence from pending metadata failure"
    );
    assert_eq!(held.sources[0].baseline_scope, baseline_scope);
    assert!(
        held.sources[0].ordinary_committed,
        "actual source must complete and commit the ordinary delta"
    );
    let partial = crate::sync_loop::runner::one_shot_partial_error(&held)
        .expect("preservation hold has partial exit category");
    assert_eq!(partial.failures, 0);
    assert_eq!(partial.held_libraries, 1);
    assert!(partial.to_string().contains("legacy_preservation_pending"));
    assert_eq!(
        crate::classify_exit_error(&anyhow::Error::new(partial)).exit_code(),
        2
    );
    owner_read(&db, &scope, &baseline_scope, &epoch, &before_epoch.attempt).await;
    assert_eq!(retained_snapshot(&db), retained);
    assert_files(&files);

    let report_path = root.join("preservation-report.json");
    let notifier = crate::notifications::Notifier::new(None);
    let config = make_run_cycle_config();
    let reporter =
        crate::cycle_reporter::CycleReporter::new(crate::cycle_reporter::CycleReporterConfig {
            watch_mode: false,
            report_path: Some(&report_path),
            run_options: crate::report::RunOptions::from_config(&config),
            health_dir: &root,
            personality_mode: crate::personality::Mode::Off,
            state_db: Some(db.as_ref()),
            metrics_handle: None,
            notifier: &notifier,
        });
    let mut health = crate::health::HealthStatus::new();
    reporter
        .report_completed_cycle(
            &mut health,
            crate::cycle_reporter::CycleFacts::from_cycle_result(
                &held,
                std::time::Duration::from_secs(1),
            ),
        )
        .await;
    let report: Value =
        serde_json::from_str(&std::fs::read_to_string(&report_path).unwrap()).unwrap();
    assert_eq!(report["status"], "partial_failure");
    assert_eq!(report["stats"]["failed"], 0);
    println!(
        "PRESERVATION_HELD pending={} failures={} sources={} reached={} ordinary_committed={} owner_rows=1 owner_paths=2 report=partial_failure exit=2 operational_safe={}",
        held.stats.unattributed_legacy_pending,
        held.failed_count,
        held.sources.len(),
        held.sources[0].reached,
        held.sources[0].ordinary_committed,
        held.operational_safe_holds
    );
    drop(reporter);
    drop(held_guards);
    drop(initial_guards);
    drop(held);
    drop(initial);
    drop(db);
    let reopened = Arc::new(
        SqliteStateDb::open_owned(&database, &baseline_owner())
            .await
            .unwrap(),
    );
    let (quiet, quiet_scope, quiet_baseline, cursors, _quiet_guards) =
        cycle(&reopened, &root, &shared, &baseline, 2).await;
    assert_eq!(cursors, ["preservation-after-1"]);
    assert_eq!(quiet.stats.downloaded, 0, "{quiet:?}");
    assert_eq!(quiet.stats.failed, 0, "{quiet:?}");
    assert_eq!(quiet.stats.unattributed_legacy_pending, 1);
    assert_eq!(quiet.sources.len(), 1);
    assert!(
        quiet.sources[0].reached
            && quiet.sources[0].ordinary_committed
            && !quiet.sources[0].genuine_error
    );
    assert_eq!(quiet_scope, scope);
    assert_eq!(quiet_baseline, baseline_scope);
    let after_epoch = {
        let conn = reopened
            .acquire_lock("reopened preservation epoch")
            .unwrap();
        crate::state::db::sync_epoch::read_library(&conn, ZONE)
            .unwrap()
            .unwrap()
    };
    assert_eq!(
        (&after_epoch.state, &after_epoch.epoch, &after_epoch.attempt),
        (
            &before_epoch.state,
            &before_epoch.epoch,
            &before_epoch.attempt
        )
    );
    owner_read(
        &reopened,
        &scope,
        &baseline_scope,
        &epoch,
        &before_epoch.attempt,
    )
    .await;
    assert_eq!(retained_snapshot(&reopened), retained);
    assert_files(&files);
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        1,
        "only the baseline media GET is allowed"
    );
    println!(
        "PRESERVATION_QUIET downloads=0 failed=0 pending=1 owner_rows=1 owner_paths=2 historical_rows_and_files_unchanged=true total_media_gets=1"
    );
    server.verify().await;
    Some((health, report))
}

#[tokio::test]
async fn normal_cycle_reports_preservation_hold_and_owner_accounts_after_reopen() {
    let _ = Box::pin(scenario(false)).await;
}

#[tokio::test]
async fn accounted_preservation_hold_should_advance_operational_health() {
    let (health, report) = Box::pin(scenario(false))
        .await
        .expect("preservation operational fixture must execute");
    assert_eq!(report["status"], "partial_failure");
    assert!(
        health.last_success_at.is_some(),
        "desired preservation health requires a future owner composition"
    );
}

#[tokio::test]
async fn protected_pending_metadata_refuses_operational_health() {
    let (health, report) = Box::pin(scenario(true))
        .await
        .expect("protected metadata fixture must execute");
    assert_eq!(report["status"], "partial_failure");
    assert!(health.last_success_at.is_none());
}
