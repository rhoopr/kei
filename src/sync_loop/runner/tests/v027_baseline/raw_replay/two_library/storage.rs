//! Full distinct stock history is not a current admission budget.
use super::super::super::{MEDIA, ZONE, baseline_owner, pair, zone};
use super::{HEALTHY, HealthySession, healthy_zone, selected};
use crate::download;
use crate::icloud::photos::{PhotosSession, inbox::ShadowCapture};
use crate::state::SqliteStateDb;
use crate::state::db::provider_inbox::CapturedPageId;
use crate::state::db::provider_work::{MAX_WORK_BYTES, WorkAdmission, WorkPlan, WorkSource};
use crate::sync_cycle::{run_cycle, sync_token_key};
use crate::sync_loop::test_support::{
    make_run_cycle_config, make_run_cycle_download_config_builder,
    make_shared_session_for_run_cycle,
};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio_util::sync::CancellationToken;
use wiremock::{
    Mock, ResponseTemplate,
    matchers::{method, path},
};

#[derive(Clone)]
struct Unavailable;
#[async_trait::async_trait]
impl PhotosSession for Unavailable {
    async fn post(&self, url: &str, body: String, _: &[(&str, &str)]) -> anyhow::Result<Value> {
        let request: Value = serde_json::from_str(&body)?;
        if request["query"]["recordType"] == "CheckIndexingState" {
            return Ok(
                json!({"records":[{"recordName":"index","fields":{"state":{"value":"FINISHED"}}}]}),
            );
        }
        if url.contains("/records/lookup?") {
            return Ok(json!({"records":[]}));
        }
        if url.contains("/zones/list") {
            return Ok(
                json!({"zones":[{"zoneID":zone(),"syncToken":"old-library-boundary","deleted":false}]}),
            );
        }
        if url.contains("/internal/records/query/batch") {
            return Ok(crate::sync_loop::test_support::album_count_response(0));
        }
        if url.contains("/records/query?") {
            return Ok(json!({"records":[]}));
        }
        if url.contains("/changes/zone?") {
            return Ok(
                json!({"zones":[{"zoneID":zone(),"records":[],"syncToken":"old-library-current","moreComing":false}]}),
            );
        }
        anyhow::bail!("unexpected old-library fixture endpoint")
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

fn configure(config: &mut download::DownloadConfig) {
    config.folder_structure = "none".into();
    config.retry.max_retries = 0;
    #[cfg(feature = "xmp")]
    {
        config.metadata.xmp_sidecar = false;
    }
}

fn old_snapshot(db: &SqliteStateDb) -> Vec<Vec<rusqlite::types::Value>> {
    let conn = db
        .acquire_lock("unchanged old responses and receipts")
        .unwrap();
    let mut out = Vec::new();
    for sql in [
        "SELECT * FROM provider_shadow_pages ORDER BY id",
        "SELECT * FROM provider_shadow_records ORDER BY page_id,ordinal",
        "SELECT * FROM provider_shadow_receipts ORDER BY scope",
        "SELECT * FROM provider_work_receipts ORDER BY page_id,ordinal,config_hash,confirmation_hash",
        "SELECT * FROM assets WHERE id='old-receipt' ORDER BY library,id,version_size",
        "SELECT * FROM asset_metadata_paths WHERE id='old-receipt' ORDER BY library,id,version_size",
    ] {
        let mut stmt = conn.prepare(sql).unwrap();
        let n = stmt.column_count();
        out.extend(
            stmt.query_map([], |r| (0..n).map(|i| r.get(i)).collect())
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
        );
    }
    out
}

async fn stock(database: &Path, root: &Path) -> u64 {
    let db = Arc::new(SqliteStateDb::create_stock_v36_for_test(
        database,
        &baseline_owner(),
    ));
    let capture = ShadowCapture::new(db.clone(), baseline_owner(), "com");
    let scope = capture.scope("private", &zone()).unwrap();
    for index in 0..32 {
        let mut records = pair(
            &format!("old-{index}"),
            &format!("old-{index}.JPG"),
            "https://invalid.example/old",
            index + 30,
        );
        records[0]["fields"]["syntheticRetainedPadding"] = json!({"value":"p".repeat(8192)});
        let raw=serde_json::to_vec(&json!({"zones":[{"zoneID":zone(),"records":records,"syncToken":format!("old-after-{index}"),"moreComing":false}]})).unwrap();
        capture
            .capture(
                crate::icloud::photos::catalog_observed_page(
                    raw,
                    &scope,
                    &format!("old-before-{index}"),
                )
                .unwrap(),
            )
            .await
            .unwrap();
    }
    let blocked = selected(db.clone(), zone(), Box::new(Unavailable)).await;
    let base = make_run_cycle_download_config_builder(root, db.clone());
    let mut config = (*base(
        download::SyncMode::Full,
        Arc::default(),
        Arc::default(),
        Arc::from(ZONE),
    ))
    .clone();
    configure(&mut config);
    let hash = download::retained_work_config_hash_for_test(
        &config.with_pass(&blocked.plan.passes[0]),
        &zone(),
    );
    let source = db
        .catalog_source(baseline_owner(), CapturedPageId(1), 16 * 1024 * 1024)
        .await
        .unwrap();
    assert_eq!(
        db.project_provider_work(
            baseline_owner(),
            WorkPlan {
                source: WorkSource {
                    page: source,
                    ordinal: 1
                },
                scope,
                zone: zone(),
                config_hash: hash,
                confirmation: None,
                master: None,
                records: vec![],
                reason: "current_lookup_unresolved",
            },
            MAX_WORK_BYTES
        )
        .await
        .unwrap(),
        WorkAdmission::Deferred
    );
    // Use the released asset schema; current upsert intentionally requires
    // schema37 writer guards and must not run against this stock database.
    let file = root.join("original.JPG");
    let hash = download::file::compute_sha256(&file).await.unwrap();
    db.acquire_lock("seed genuine old downloaded receipt").unwrap().execute(
        "INSERT INTO assets(library,id,version_size,checksum,filename,created_at,size_bytes,media_type,status,last_seen_at,local_path,local_checksum,download_checksum) VALUES('OldSync','old-receipt','original','old-provider-generation','original.JPG',1000,?1,'photo','downloaded',1000,?2,?3,?3)",
        rusqlite::params![i64::try_from(MEDIA.len()).unwrap(),file.to_str().unwrap(),hash]).unwrap();
    let conn = db.acquire_lock("genuine stock capacity").unwrap();
    assert_eq!(crate::state::schema::get_schema_version(&conn).unwrap(), 36);
    assert_eq!(
        conn.query_row::<i64, _, _>("SELECT count(*) FROM provider_transport_aliases", [], |r| r
            .get(0))
            .unwrap(),
        0
    );
    u64::try_from(
        conn.query_row::<i64, _, _>(
            "SELECT SUM(charged_bytes) FROM provider_shadow_pages",
            [],
            |r| r.get(0),
        )
        .unwrap(),
    )
    .unwrap()
}

fn measure(db: &SqliteStateDb, database: &Path, label: &str) -> Value {
    let conn = db.acquire_lock("storage measurement").unwrap();
    let scalar = |sql: &str| conn.query_row::<i64, _, _>(sql, [], |r| r.get(0)).unwrap();
    // Same measurement phase each cycle: one open WAL connection, no forced checkpoint.
    json!({"phase":label,
        "old_charge":scalar("SELECT SUM(charged_bytes) FROM provider_shadow_pages"),
        "old_pages":scalar("SELECT count(*) FROM provider_shadow_pages"),
        "rank_pages":scalar("SELECT count(*) FROM provider_selection_rank_pages"),
        "root_charge":scalar("SELECT COALESCE(SUM(charged_bytes),0) FROM provider_active_generations WHERE json_extract(CAST(specification AS TEXT),'$.format')=2"),
        "pending_charge":scalar("SELECT COALESCE(SUM(charged_bytes),0) FROM provider_active_decisions WHERE compacted_hash IS NULL"),
        "pending_confirmation_bytes":scalar("SELECT COALESCE(SUM(json_array_length(json_extract(CAST(manifest AS TEXT),'$.decision.confirmation'))),0) FROM provider_active_decisions WHERE compacted_hash IS NULL"),
        "manifest_bytes":scalar("SELECT COALESCE(SUM(length(manifest)),0) FROM provider_active_decisions"),
        "compacted_rows":scalar("SELECT count(*) FROM provider_active_decisions WHERE compacted_hash IS NOT NULL"),
        "pending_rows":scalar("SELECT count(*) FROM provider_active_decisions WHERE compacted_hash IS NULL"),
        "receipts":scalar("SELECT count(*) FROM download_receipts"),
        "page_size":scalar("PRAGMA page_size"),"page_count":scalar("PRAGMA page_count"),"freelist":scalar("PRAGMA freelist_count"),
        "db_bytes":std::fs::metadata(database).unwrap().len(),
        "wal_bytes":std::fs::metadata(database.with_file_name("state.db-wal")).map_or(0,|m|m.len())})
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fault {
    None,
    Admission,
    Retirement,
}
#[tokio::test]
async fn v027_full_distinct_stock_inbox_preserves_work_and_reclaims_current_cycles() {
    storage_contract(Fault::None).await;
}
#[tokio::test]
async fn v027_full_distinct_stock_admission_rollback_reopens_without_lost_work() {
    storage_contract(Fault::Admission).await;
}
#[tokio::test]
async fn v027_full_distinct_stock_retirement_rollback_reopens_without_redownload() {
    storage_contract(Fault::Retirement).await;
}

fn owner_snapshot(db: &SqliteStateDb) -> Vec<Vec<rusqlite::types::Value>> {
    let conn = db.acquire_lock("retirement transaction snapshot").unwrap();
    let mut rows = Vec::new();
    for sql in [
        "SELECT * FROM provider_active_generations WHERE json_extract(scope,'$.zone.zoneName')=?1 ORDER BY id",
        "SELECT d.* FROM provider_active_decisions d JOIN provider_active_generations g ON g.id=d.generation WHERE json_extract(g.scope,'$.zone.zoneName')=?1 ORDER BY d.generation,d.pass_key,d.child",
        "SELECT * FROM provider_active_destinations WHERE library=?1 ORDER BY generation,pass_key,child,version_size,path",
        "SELECT * FROM download_receipts WHERE library=?1 ORDER BY id",
    ] {
        let mut stmt = conn.prepare(sql).unwrap();
        let n = stmt.column_count();
        rows.extend(
            stmt.query_map([HEALTHY], |r| (0..n).map(|i| r.get(i)).collect())
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
        );
    }
    rows
}
#[allow(
    clippy::print_stdout,
    reason = "synthetic storage measurements are test evidence"
)]
async fn storage_contract(fault: Fault) {
    let (trace, _trace_guard) = crate::test_helpers::TracingCapture::install();
    let server = crate::start_wiremock_or_skip!();
    for i in 0..4 {
        Mock::given(method("GET"))
            .and(path(format!("/live-{i}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
            .expect(1)
            .mount(&server)
            .await;
    }
    Mock::given(method("GET"))
        .and(path("/recovered-blocked"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/blocked"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    std::fs::create_dir(&root).unwrap();
    let original = root.join("original.JPG");
    std::fs::write(&original, MEDIA).unwrap();
    let mtime = std::fs::metadata(&original).unwrap().modified().unwrap();
    let database = dir.path().join("state.db");
    let budget = stock(&database, &root).await;
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let mut before = None;
    let mut measurements = Vec::new();
    let mut published = Vec::new();
    let mut stable_pending = None;
    let mut stable_overhead = None;
    for step in 0..if fault == Fault::None { 7 } else { 8 } {
        let cycle = if fault != Fault::None && step >= 3 {
            step - 1
        } else {
            step
        };
        let db = Arc::new(
            SqliteStateDb::open_owned(&database, &baseline_owner())
                .await
                .unwrap(),
        );
        if cycle == 0 {
            before = Some(old_snapshot(&db));
            let capture = ShadowCapture::new(db.clone(), baseline_owner(), "com");
            let scope = capture.scope("private", &zone()).unwrap();
            let raw=serde_json::to_vec(&json!({"zones":[{"zoneID":zone(),"records":pair("new-capacity-probe","probe.JPG","https://invalid.example/probe",99),"syncToken":"probe-after","moreComing":false}]})).unwrap();
            assert!(
                db.capture_shadow_page(
                    baseline_owner(),
                    crate::icloud::photos::catalog_observed_page(raw, &scope, "probe-before")
                        .unwrap(),
                    budget
                )
                .await
                .is_err_and(|error| matches!(
                    error,
                    crate::state::error::StateError::ProviderInboxFull
                )),
                "distinct old-style capture must actually be full"
            );
            db.acquire_lock("no new inbox or rank captures").unwrap().execute_batch("CREATE TRIGGER reject_inbox BEFORE INSERT ON provider_shadow_pages BEGIN SELECT RAISE(ABORT,'normal sync wrote old inbox'); END; CREATE TRIGGER reject_rank BEFORE INSERT ON provider_selection_rank_pages BEGIN SELECT RAISE(ABORT,'normal sync saved a rank page'); END;").unwrap();
            measurements.push(measure(&db, &database, "post-migration-full"));
        }
        let mut records = Vec::new();
        let mut delta = Vec::new();
        for i in 0..=cycle.min(3) {
            let mut entry = pair(
                &format!("live-{i}"),
                &format!("live-{i}.JPG"),
                &format!("{}/live-{i}", server.uri()),
                i as u8 + 70,
            );
            entry[0]["fields"]["syntheticUnusedPadding"] = json!({"value":"u".repeat(4096)});
            entry[1]["fields"]["masterRef"]["value"]["zoneID"] = healthy_zone();
            if cycle > 0 && cycle < 4 && i == cycle {
                delta = entry.clone();
            }
            records.extend(entry);
        }
        let mut pending = pair(
            "live-blocked",
            "blocked.JPG",
            &format!(
                "{}/{}",
                server.uri(),
                if cycle >= 5 {
                    "recovered-blocked"
                } else {
                    "blocked"
                }
            ),
            88,
        );
        pending[0]["fields"]["syntheticUnusedPadding"] = json!({"value":"u".repeat(8192)});
        pending[1]["fields"]["masterRef"]["value"]["zoneID"] = healthy_zone();
        records.extend(pending);
        let session = HealthySession {
            cycle,
            records: Arc::new(records),
            delta: Arc::new(delta),
            inventory: Arc::new(AtomicUsize::new(0)),
            changes: Arc::new(AtomicUsize::new(0)),
            boundaries: Arc::new(AtomicUsize::new(0)),
        };
        let healthy = selected(db.clone(), healthy_zone(), Box::new(session.clone())).await;
        let blocked = selected(db.clone(), zone(), Box::new(Unavailable)).await;
        let base = make_run_cycle_download_config_builder(&root, db.clone());
        let builder = |mode, exclude, groups, name| {
            let mut config = (*base(mode, exclude, groups, name)).clone();
            configure(&mut config);
            Arc::new(config)
        };
        if step == 2 && fault != Fault::None {
            let sql = match fault {
                Fault::Admission => {
                    "CREATE TRIGGER storage_fault BEFORE INSERT ON provider_active_destinations WHEN NEW.child='asset-live-2' BEGIN SELECT RAISE(ABORT,'storage admission fault'); END;"
                }
                Fault::Retirement => {
                    "CREATE TRIGGER storage_fault BEFORE DELETE ON provider_active_decisions WHEN OLD.child='asset-live-1' AND OLD.compacted_hash IS NOT NULL BEGIN SELECT RAISE(ABORT,'storage retirement fault'); END;"
                }
                Fault::None => "",
            };
            db.acquire_lock("install precise storage fault")
                .unwrap()
                .execute_batch(sql)
                .unwrap();
        }
        if step == 3 && fault != Fault::None {
            db.acquire_lock("remove fault after reopen")
                .unwrap()
                .execute_batch("DROP TRIGGER storage_fault")
                .unwrap();
        }
        if cycle == 5 {
            db.make_selection_retry_due(baseline_owner(), "asset-live-blocked".into())
                .await
                .unwrap();
        }
        let prior_owners = owner_snapshot(&db);
        let prior_cursor = db.get_metadata(&sync_token_key(HEALTHY)).await.unwrap();
        let result = run_cycle(
            &[&blocked, &healthy],
            &make_run_cycle_config(),
            Some(db.as_ref()),
            false,
            &builder,
            download::DownloadControls::download_hidden(),
            &shared,
            &CancellationToken::new(),
        )
        .await;
        if step == 2 && fault != Fault::None {
            let expected = if fault == Fault::Admission {
                "storage admission fault"
            } else {
                "storage retirement fault"
            };
            assert!(
                format!("{result:?} {:?}", trace.events()).contains(expected),
                "fault must execute: {result:?}"
            );
            if let Ok(outcome) = &result {
                assert!(
                    !outcome.can_advance_database_checkpoint(),
                    "fault cannot report complete: {outcome:?}"
                );
                assert!(
                    outcome.stats.state_write_failures > 0
                        || (outcome.stats.sync_token_blocked
                            && outcome.stats.sync_token_blocked_zone.as_deref() == Some(HEALTHY)),
                    "explicit affected-library checkpoint/state-write failure required: {outcome:?}"
                );
            }
            assert_eq!(
                db.get_metadata(&sync_token_key(HEALTHY)).await.unwrap(),
                prior_cursor
            );
            assert!(
                !root.join("live-2.JPG").exists(),
                "failed transaction must not publish new work"
            );
            assert_eq!(old_snapshot(&db), *before.as_ref().unwrap());
            for (path, stamp) in &published {
                assert_eq!(std::fs::read(path).unwrap(), MEDIA);
                assert_eq!(std::fs::metadata(path).unwrap().modified().unwrap(), *stamp);
            }
            if fault == Fault::Retirement {
                assert_eq!(
                    owner_snapshot(&db),
                    prior_owners,
                    "retirement rollback must preserve owners, receipts and seals"
                );
            } else {
                let conn = db.acquire_lock("admission rolled back").unwrap();
                assert_eq!(conn.query_row::<i64,_,_>("SELECT count(*) FROM provider_active_destinations WHERE child='asset-live-2'",[],|r|r.get(0)).unwrap(),0);
                assert_eq!(
                    conn.query_row::<i64, _, _>(
                        "SELECT count(*) FROM provider_active_decisions WHERE child='asset-live-2'",
                        [],
                        |r| r.get(0)
                    )
                    .unwrap(),
                    0
                );
            }
            measurements.push(measure(&db, &database, "fault-before-reopen"));
            continue;
        }
        let result = result.unwrap();
        assert_eq!(
            db.get_metadata(&sync_token_key(HEALTHY)).await.unwrap(),
            Some(format!("healthy-cycle-{}", cycle + 1)),
            "cycle {cycle}: {result:?}"
        );
        assert_eq!(
            result.stats.downloaded,
            usize::from(cycle < 4 || cycle == 5)
        );
        if cycle > 0 {
            assert_eq!(session.inventory.load(Ordering::SeqCst), 0);
            assert_eq!(session.boundaries.load(Ordering::SeqCst), 0);
        }
        if cycle < 4 {
            let path = root.join(format!("live-{cycle}.JPG"));
            published.push((
                path.clone(),
                std::fs::metadata(path).unwrap().modified().unwrap(),
            ));
        }
        if cycle == 5 {
            let path = root.join("blocked.JPG");
            published.push((
                path.clone(),
                std::fs::metadata(path).unwrap().modified().unwrap(),
            ));
        }
        for (path, stamp) in &published {
            assert_eq!(std::fs::read(path).unwrap(), MEDIA);
            assert_eq!(std::fs::metadata(path).unwrap().modified().unwrap(), *stamp);
        }
        assert_eq!(old_snapshot(&db), *before.as_ref().unwrap());
        assert_eq!(std::fs::read(&original).unwrap(), MEDIA);
        assert_eq!(
            std::fs::metadata(&original).unwrap().modified().unwrap(),
            mtime
        );
        let conn = db
            .acquire_lock("both unfinished obligations retained")
            .unwrap();
        assert!(
            conn.query_row::<i64, _, _>(
                "SELECT count(*) FROM provider_work_retries WHERE page_id=1 AND ordinal=1",
                [],
                |r| r.get(0)
            )
            .unwrap()
                > 0
        );
        let pending=conn.query_row::<i64,_,_>("SELECT count(*) FROM provider_active_destinations WHERE library=?1 AND child='asset-live-blocked' AND verified_media=0",[HEALTHY],|r|r.get(0)).unwrap();
        assert_eq!(pending, i64::from(cycle < 5));
        drop(conn);
        let sample = measure(&db, &database, &format!("cycle-{cycle}"));
        assert_eq!(sample["old_charge"], json!(budget));
        assert_eq!(sample["rank_pages"], 0);
        assert_eq!(
            sample["pending_rows"],
            json!(usize::from(cycle < 5)),
            "only the failed current transfer retains a bulky task"
        );
        assert_eq!(
            sample["receipts"],
            json!(if cycle >= 5 { 5 } else { cycle.min(3) + 1 })
        );
        assert!(
            sample["compacted_rows"].as_u64().unwrap() <= if cycle >= 5 { 2 } else { 1 },
            "completed bookkeeping must remain bounded, including the newly resolved retry: {sample}"
        );
        let pending = (
            sample["pending_charge"].clone(),
            sample["pending_confirmation_bytes"].clone(),
        );
        if cycle < 5 {
            assert!(
                pending.1.as_u64().unwrap() > 8192,
                "unused provider field must actually be retained"
            );
            if let Some(prior) = &stable_pending {
                assert_eq!(
                    &pending, prior,
                    "quiet task cannot accumulate response copies"
                );
            } else {
                stable_pending = Some(pending);
            }
        } else {
            assert_eq!(
                pending,
                (json!(0), json!(0)),
                "quiet retry completion must retire its retained confirmation"
            );
        }
        let overhead = (
            sample["root_charge"].as_u64().unwrap(),
            sample["manifest_bytes"].as_u64().unwrap(),
        );
        if cycle == 1 {
            stable_overhead = Some(overhead);
        }
        if cycle >= 2 {
            let prior = stable_overhead.unwrap();
            assert!(
                overhead.0 <= prior.0 && overhead.1 <= prior.1,
                "distinct completed changes must not accumulate bulky manifests: {sample}"
            );
        }
        let conn = db
            .acquire_lock("completed response payload retired")
            .unwrap();
        assert_eq!(conn.query_row::<i64,_,_>("SELECT count(*) FROM provider_active_decisions WHERE compacted_hash IS NOT NULL AND json_extract(CAST(manifest AS TEXT),'$.decision.confirmation') IS NOT NULL",[],|r|r.get(0)).unwrap(),0);
        drop(conn);
        measurements.push(sample);
    }
    println!("STORAGE_MEASUREMENTS {fault:?} {}", json!(measurements));
    server.verify().await;
}
