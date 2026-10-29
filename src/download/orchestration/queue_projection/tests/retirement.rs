//! Completion ownership survives body compaction and predecessor retirement.
use super::{CURRENT, Fixture, controls, destination_pass, destination_pass_with, owner, records};
use crate::download::orchestration::models::DownloadStore;
use crate::download::planner::{TaskPlanner, pending_record_for_task};
use crate::icloud::photos::inbox::ShadowCapture;
use crate::state::DownloadStateStore;
use crate::state::MetadataRewriteStore;
use crate::state::SqliteStateDb;
use crate::state::db::provider_generations::{
    ActiveDecision, ActiveGeneration, GenerationSpec, MAX_GENERATION_BYTES, ReuseAuthorization,
};
use crate::state::db::provider_selection::{SelectionDecision, SelectionDestination};
use crate::state::db::provider_selection::{SelectionOutcome, SelectionPath};
use crate::state::db::{MetadataRewriteCompletion, MetadataRewriteQueue};
use rusqlite::OptionalExtension as _;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio_util::sync::CancellationToken;

tokio::task_local! { static REFERENCE_RAW: bool; }

const RAW: &[u8] = b"raw fixture bytes";
pub(super) const BUDGET: u64 = 64 * 1024;
fn authorization(
    scope: &str,
    generation: &str,
    child: &str,
    flags: u8,
    explicit: bool,
) -> ReuseAuthorization {
    ReuseAuthorization {
        current_metadata_flags: flags,
        explicit_refresh_authorized: explicit,
        scope: scope.to_owned(),
        pass_key: "pass".into(),
        child: child.to_owned(),
        generation: Some(generation.to_owned()),
    }
}

pub(super) fn spec() -> GenerationSpec {
    let zone = json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"});
    GenerationSpec {
        format:2,
        scope:serde_json::to_string(&json!({"format":1,"realm":"com","container":"com.apple.photos.cloud","environment":"production","database":"private","zone":zone})).unwrap(),
        zone:zone.clone(),config_hash:"a".repeat(64),basis:"b".repeat(64),
        profile:json!({"coverage":"selected_downloads","passes":[{"key":"pass","scope":{"database":"private","zone":zone,"kind":"unfiled"}}]}),
        metadata_enabled:false,metadata_flags:0,
    }
}
pub(super) async fn admission(
    db: &Arc<SqliteStateDb>,
    root: &str,
    media: &std::path::Path,
    index: usize,
    companion: bool,
    capacity: u64,
) -> (
    ActiveDecision,
    Vec<crate::download::filter::DownloadTask>,
    Vec<crate::state::AssetRecord>,
) {
    admission_with_legacy_state(db, root, media, index, companion, capacity, false).await
}
async fn admission_with_legacy_state(
    db: &Arc<SqliteStateDb>,
    root: &str,
    media: &std::path::Path,
    index: usize,
    companion: bool,
    capacity: u64,
    legacy: bool,
) -> (
    ActiveDecision,
    Vec<crate::download::filter::DownloadTask>,
    Vec<crate::state::AssetRecord>,
) {
    let mut planner = TaskPlanner::for_download(Some(db.as_ref())).await.unwrap();
    admission_with_planner(
        db,
        root,
        media,
        index,
        (companion, legacy),
        capacity,
        &mut planner,
    )
    .await
}

async fn admission_with_planner(
    db: &Arc<SqliteStateDb>,
    root: &str,
    media: &std::path::Path,
    index: usize,
    options: (bool, bool),
    capacity: u64,
    planner: &mut TaskPlanner,
) -> (
    ActiveDecision,
    Vec<crate::download::filter::DownloadTask>,
    Vec<crate::state::AssetRecord>,
) {
    let (companion, legacy) = options;
    let master = format!("compact-{index}");
    let child = format!("asset-{master}");
    let mut values = records(&master, CURRENT, "compact fixture");
    values[0]["fields"]["resOriginalRes"]["value"]["size"] = json!(RAW.len());
    if companion {
        values[0]["fields"]["resOriginalVidComplRes"] = json!({"value":{"downloadURL":"https://p01.icloud-content.com/motion.mov","size":RAW.len(),"fileChecksum":CURRENT}});
        values[0]["fields"]["resOriginalVidComplFileType"] =
            json!({"value":"com.apple.quicktime-movie"});
    }
    if REFERENCE_RAW.try_get().unwrap_or(false) {
        values[0]["fields"]["resOriginalAltRes"] = json!({"value":{"downloadURL":"https://p01.icloud-content.com/alternate.dng","size":RAW.len(),"fileChecksum":super::OLD}});
        values[0]["fields"]["resOriginalAltFileType"] = json!({"value":"com.adobe.raw-image"});
    }
    let body =
        serde_json::to_vec(&json!({"records":values,"synthetic_padding":"x".repeat(4*1024)}))
            .unwrap();
    let asset = crate::icloud::photos::current_asset(&body, &child, &master, &spec().zone).unwrap();
    if companion {
        assert!(
            asset.is_live_photo(),
            "fixture motion URL must pass provider resource validation"
        );
    }
    let mut config = super::super::DownloadConfig::test_default();
    config.directory = media.join(index.to_string()).into();
    config.folder_structure = "none".into();
    config.media.live_photos = companion;
    if REFERENCE_RAW.try_get().unwrap_or(false) {
        config.raw_policy = crate::types::RawPolicy::PreferRaw;
    }
    config.state_db = Some(db.clone() as Arc<dyn DownloadStore>);
    let mut plan = planner.plan_download_asset(&asset, &config).await.unwrap();
    if legacy {
        db.acquire_lock("valid historical master state owner").unwrap().execute(
            "INSERT INTO legacy_master_state_owners(library,master_record_name,asset_record_name,claimed_at) VALUES('PrimarySync',?1,?2,1)",
            rusqlite::params![master,child],
        ).unwrap();
        for task in &mut plan.tasks {
            task.asset_id = master.clone().into();
        }
    }
    let rows: Vec<_> = plan
        .tasks
        .iter()
        .map(|task| pending_record_for_task(&config, &asset, task))
        .collect();
    let destinations = plan
        .tasks
        .iter()
        .map(|task| SelectionDestination {
            version_size: task.version_size.as_str().into(),
            path: SelectionPath::from_path(&std::path::absolute(&task.download_path).unwrap()),
            checksum: task.checksum.to_string(),
            size: task.size,
            metadata_hash: crate::download::filter::metadata_for_selected_version(
                &asset,
                &config,
                task.version_size,
            )
            .metadata_hash
            .clone()
            .unwrap(),
        })
        .collect();
    let format:i64=db.acquire_lock("fixture generation format").unwrap().query_row(
        "SELECT json_extract(CAST(specification AS TEXT),'$.format') FROM provider_active_generations WHERE id=?1",[root],|row|row.get(0)).unwrap();
    let sources = if format == 1 {
        db.capture_selection_rank_page(
            owner(),
            root.into(),
            "pass".into(),
            legacy_rank_request(),
            body.clone(),
            capacity,
        )
        .await
        .unwrap();
        db.selection_rank_sources(owner(), root.into(), "pass".into(), child.clone())
            .await
            .unwrap()
    } else {
        Vec::new()
    };
    let manifest = ActiveDecision {
        decision: SelectionDecision {
            pass_key: "pass".into(),
            child: child.clone(),
            master: Some(master.clone()),
            confirmation: Some(body),
            outcome: SelectionOutcome::Selected,
            reason: String::new(),
            destinations,
        },
        sources,
        state_id: if legacy { master } else { child },
    };
    assert!(
        serde_json::to_vec(&manifest).unwrap().len() < capacity as usize,
        "each atomic observation must fit before testing cumulative retirement"
    );
    assert!(
        db.project_selection_decision(
            owner(),
            root.into(),
            manifest.clone(),
            rows.clone(),
            capacity
        )
        .await
        .unwrap()
        .admitted
    );
    (manifest, plan.tasks, rows)
}
async fn publish(db: &SqliteStateDb, task: &crate::download::filter::DownloadTask) {
    std::fs::create_dir_all(task.download_path.parent().unwrap()).unwrap();
    std::fs::write(&task.download_path, RAW).unwrap();
    let sha = data_encoding::HEXLOWER.encode(&Sha256::digest(RAW));
    db.mark_verified_download(
        "PrimarySync",
        &task.asset_id,
        task.version_size.as_str(),
        &task.download_path,
        &sha,
        Some(&sha),
        false,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn v027_compacted_current_pass_cannot_settle_independent_unfiled_destination() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(
        SqliteStateDb::open_owned(&dir.path().join("state.db"), &owner())
            .await
            .unwrap(),
    );
    let mut specification = spec();
    specification.profile["passes"].as_array_mut().unwrap().push(json!({
        "key":"unfiled", "kind":"Unfiled", "scope":{"database":"private","zone":specification.zone.clone(),"kind":"unfiled"}
    }));
    let root = db
        .begin_selection_generation(owner(), specification.clone(), BUDGET)
        .await
        .unwrap();
    let (_, tasks, _) = admission(&db, &root.id, &dir.path().join("media"), 1, false, BUDGET).await;
    publish(&db, &tasks[0]).await;
    db.seal_selection_generation(owner(), root.id.clone(), true, None)
        .await
        .unwrap();
    {
        let conn = db.acquire_lock("prove current owner compacted").unwrap();
        assert_eq!(conn.query_row::<i64,_,_>("SELECT count(*) FROM provider_active_decisions WHERE generation=?1 AND compacted_hash IS NOT NULL", [&root.id], |r|r.get(0)).unwrap(), 1);
    }
    let path = SelectionPath::from_path(&tasks[0].download_path);
    assert!(
        !db.selection_path_reusable_for_pass(
            owner(),
            root.id.clone(),
            specification.scope.clone(),
            "unfiled".into(),
            "asset-compact-1".into(),
            path.clone()
        )
        .await
        .unwrap()
    );
    assert!(
        db.selection_path_reusable_for_pass(
            owner(),
            root.id,
            specification.scope.clone(),
            "pass".into(),
            "asset-compact-1".into(),
            path.clone()
        )
        .await
        .unwrap()
    );
    // A later observation may reuse the earlier verified file at its stable path.
    let next = db
        .begin_selection_generation(owner(), specification.clone(), BUDGET)
        .await
        .unwrap();
    assert!(
        db.selection_path_reusable_for_pass(
            owner(),
            next.id,
            specification.scope,
            "unfiled".into(),
            "asset-compact-1".into(),
            path
        )
        .await
        .unwrap()
    );
    assert_eq!(std::fs::read(&tasks[0].download_path).unwrap(), RAW);
}

#[tokio::test]
async fn v027_active_baseline_compacts_above_budget_and_preserves_pending_companion() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(
        SqliteStateDb::open_owned(&dir.path().join("state.db"), &owner())
            .await
            .unwrap(),
    );
    let root = db
        .begin_selection_generation(owner(), spec(), BUDGET)
        .await
        .unwrap();
    let (pending, tasks, _) =
        admission(&db, &root.id, &dir.path().join("media"), 0, true, BUDGET).await;
    assert_eq!(tasks.len(), 2, "fixture must include its companion");
    publish(&db, &tasks[0]).await;
    let first = tasks[0].download_path.clone();
    let mtime = std::fs::metadata(&first).unwrap().modified().unwrap();
    let mut last = None;
    let mut observed_bytes = serde_json::to_vec(&pending).unwrap().len();
    for index in 1..14 {
        let (manifest, tasks, rows) = admission(
            &db,
            &root.id,
            &dir.path().join("media"),
            index,
            false,
            BUDGET,
        )
        .await;
        assert_eq!(tasks.len(), 1);
        observed_bytes += serde_json::to_vec(&manifest).unwrap().len();
        publish(&db, &tasks[0]).await;
        last = Some((manifest, rows));
    }
    assert!(
        observed_bytes > BUDGET as usize,
        "the fixture must exceed the unresolved-evidence budget cumulatively"
    );
    assert!(
        !db.current_download_generation(owner(), spec().scope, spec().config_hash)
            .await
            .unwrap()
            .unwrap()
            .sealed
    );
    {
        let conn = db.acquire_lock("bounded active baseline").unwrap();
        let compacted: i64 = conn
            .query_row(
                "SELECT count(*) FROM provider_active_decisions WHERE compacted_hash IS NOT NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(compacted > 0, "compaction must happen before baseline seal");
        let raw:i64=conn.query_row("SELECT sum(length(manifest)) FROM provider_active_decisions WHERE compacted_hash IS NULL",[],|r|r.get(0)).unwrap();
        assert!(raw < BUDGET as i64);
    }
    db.seal_selection_generation(owner(), root.id.clone(), true, None)
        .await
        .unwrap();
    let (last, rows) = last.unwrap();
    assert!(
        db.project_selection_decision(owner(), root.id.clone(), last, rows, BUDGET)
            .await
            .unwrap()
            .admitted,
        "identical repeated observation must retain compact completion"
    );
    let stored = db
        .selection_decision(
            owner(),
            root.id.clone(),
            "pass".into(),
            pending.decision.child.clone(),
        )
        .await
        .unwrap()
        .unwrap();
    assert!(
        stored.decision.confirmation.is_some(),
        "pending companion still owns its useful original evidence"
    );
    db.acquire_lock("completed predecessor is the interrupted replay cursor")
        .unwrap()
        .execute(
            "UPDATE provider_active_generations SET replay_after=?2 WHERE id=?1",
            rusqlite::params![
                root.id,
                serde_json::to_string(&("pass", "asset-compact-13")).unwrap()
            ],
        )
        .unwrap();
    let next = db
        .begin_selection_generation(owner(), spec(), BUDGET)
        .await
        .unwrap();
    {
        let conn = db
            .acquire_lock("predecessor retains only unfinished work")
            .unwrap();
        let previous: i64 = conn
            .query_row(
                "SELECT count(*) FROM provider_active_decisions WHERE generation=?1",
                [&root.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(previous, 1);
        assert_eq!(
            conn.query_row("SELECT count(*) FROM download_receipts", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            13
        );
        assert_eq!(std::fs::read(&first).unwrap(), RAW);
        assert_eq!(
            std::fs::metadata(&first).unwrap().modified().unwrap(),
            mtime
        );
        assert_ne!(root.id, next.id);
        assert!(
            conn.query_row(
                "SELECT replay_after IS NULL FROM provider_active_generations WHERE id=?1",
                [&root.id],
                |r| r.get::<_, bool>(0)
            )
            .unwrap()
        );
    }
    drop(db);
    let reopened = SqliteStateDb::open_owned(&dir.path().join("state.db"), &owner())
        .await
        .unwrap();
    assert!(
        reopened
            .has_pending_selection(owner(), root.id.clone())
            .await
            .unwrap()
    );
    publish(&reopened, &tasks[1]).await;
    reopened
        .complete_selection_metadata(owner(), root.id.clone())
        .await
        .unwrap();
    let completed = reopened
        .selection_decision(owner(), root.id, "pass".into(), pending.decision.child)
        .await
        .unwrap()
        .unwrap();
    assert!(
        completed.decision.confirmation.is_none(),
        "quiet-provider completion must retire response evidence without a new observation"
    );
    assert_eq!(
        reopened
            .acquire_lock("quiet completed companion receipt")
            .unwrap()
            .query_row("SELECT count(*) FROM download_receipts", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        15
    );
    assert_eq!(std::fs::read(&first).unwrap(), RAW);
}

#[tokio::test]
async fn v027_compact_receipt_survives_repeated_successors_and_scope_conflict() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(
        SqliteStateDb::open_owned(&dir.path().join("state.db"), &owner())
            .await
            .unwrap(),
    );
    let root = db
        .begin_selection_generation(owner(), spec(), BUDGET)
        .await
        .unwrap();
    let (_, tasks, _) = admission(&db, &root.id, &dir.path().join("media"), 1, false, BUDGET).await;
    publish(&db, &tasks[0]).await;
    let sha = data_encoding::HEXLOWER.encode(&Sha256::digest(RAW));
    let path = SelectionPath::from_path(&tasks[0].download_path);
    db.seal_selection_generation(owner(), root.id, true, None)
        .await
        .unwrap();
    for _ in 0..8 {
        let next = db
            .begin_selection_generation(owner(), spec(), BUDGET)
            .await
            .unwrap();
        db.seal_selection_generation(owner(), next.id, true, None)
            .await
            .unwrap();
        assert!(
            db.selection_path_matches_local(
                owner(),
                spec().scope,
                "pass".into(),
                "asset-compact-1".into(),
                path.clone(),
                (sha.clone(), RAW.len() as u64)
            )
            .await
            .unwrap()
        );
        let conn = db.acquire_lock("bounded completed ledger").unwrap();
        assert_eq!(
            conn.query_row("SELECT count(*) FROM download_receipts", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM provider_active_generations",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
        assert_eq!(
            conn.query_row("SELECT count(*) FROM provider_active_decisions", [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap(),
            0
        );
    }
    let mut conflict = spec();
    let mut scope: Value = serde_json::from_str(&conflict.scope).unwrap();
    scope["database"] = json!("shared");
    conflict.scope = serde_json::to_string(&scope).unwrap();
    assert!(
        db.begin_selection_generation(owner(), conflict, MAX_GENERATION_BYTES)
            .await
            .is_err()
    );
    assert_eq!(std::fs::read(&tasks[0].download_path).unwrap(), RAW);
}

#[tokio::test]
async fn v027_compact_native_receipts_remain_reachable_beyond_sixty_four_copies() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(
        SqliteStateDb::open_owned(&dir.path().join("state.db"), &owner())
            .await
            .unwrap(),
    );
    let mut expected = Vec::new();
    for copy in 0..66 {
        let root = db
            .begin_selection_generation(owner(), spec(), BUDGET)
            .await
            .unwrap();
        let component = {
            #[cfg(unix)]
            if copy >= 64 {
                use std::os::unix::ffi::OsStringExt;
                std::ffi::OsString::from_vec(vec![
                    b'c',
                    b'o',
                    b'p',
                    b'y',
                    b'-',
                    if copy == 64 { 0xfe } else { 0xff },
                ])
            } else {
                std::ffi::OsString::from(format!("copy-{copy}"))
            }
            #[cfg(not(unix))]
            std::ffi::OsString::from(format!("copy-{copy}"))
        };
        let (_, tasks, _) = admission(
            &db,
            &root.id,
            &dir.path().join("media").join(component),
            1,
            false,
            BUDGET,
        )
        .await;
        publish(&db, &tasks[0]).await;
        expected.push(SelectionPath::from_path(&tasks[0].download_path));
        db.seal_selection_generation(owner(), root.id, true, None)
            .await
            .unwrap();
    }
    // Make every completed copy depend on the compact owner, not retained roots.
    db.begin_selection_generation(owner(), spec(), BUDGET)
        .await
        .unwrap();
    let paths = db
        .verified_selection_paths(
            owner(),
            spec().scope,
            "pass".into(),
            "asset-compact-1".into(),
            "compact-1".into(),
            "original".into(),
            (CURRENT.into(), RAW.len() as u64),
        )
        .await
        .unwrap();
    assert_eq!(paths.len(), 66);
    let sha = data_encoding::HEXLOWER.encode(&Sha256::digest(RAW));
    for path in expected {
        assert!(paths.contains(&path));
        assert!(
            db.selection_path_matches_local(
                owner(),
                spec().scope,
                "pass".into(),
                "asset-compact-1".into(),
                path.clone(),
                (sha.clone(), RAW.len() as u64)
            )
            .await
            .unwrap()
        );
        assert_eq!(std::fs::read(path.to_path()).unwrap(), RAW);
    }
}

#[cfg(target_os = "linux")]
mod process_death {
    use super::{BUDGET, MAX_GENERATION_BYTES, RAW, admission, legacy_spec, owner, publish, spec};
    use crate::state::SqliteStateDb;
    use crate::state::db::provider_selection::SelectionPath;
    use sha2::{Digest, Sha256};
    use std::os::unix::process::ExitStatusExt;
    use std::process::{Child, Command, Stdio};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    const WORKER: &str = "download::orchestration::queue_projection::tests::retirement::process_death::v027_retirement_death_worker";
    struct Worker(Child);
    impl Drop for Worker {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[tokio::test]
    async fn v027_compaction_sigkill_never_loses_old_or_completed_owner() {
        for point in [
            "completion-receipts-saved",
            "completion-manifest-retired",
            "completion-handoff-committed",
            "completion-before-root-retirement",
            "completion-root-retired",
        ] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(
                dir.path().join("fixture-owner"),
                b"kei-synthetic-process-death",
            )
            .unwrap();
            let db = Arc::new(
                SqliteStateDb::open_owned(&dir.path().join("state.db"), &owner())
                    .await
                    .unwrap(),
            );
            let root = db
                .begin_selection_generation(owner(), spec(), BUDGET)
                .await
                .unwrap();
            let (_, tasks, _) =
                admission(&db, &root.id, &dir.path().join("media"), 1, false, BUDGET).await;
            publish(&db, &tasks[0]).await;
            let media = tasks[0].download_path.clone();
            let mtime = std::fs::metadata(&media).unwrap().modified().unwrap();
            if point.contains("root") {
                db.seal_selection_generation(owner(), root.id.clone(), true, None)
                    .await
                    .unwrap();
            }
            drop(db);
            let log = std::fs::File::create(dir.path().join("worker.log")).unwrap();
            let mut worker = Worker(
                Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", WORKER, "--ignored", "--nocapture"])
                    .env("KEI_TEST_PROCESS_DEATH_ROOT", dir.path())
                    .env("KEI_TEST_PROCESS_DEATH_POINT", point)
                    .env("KEI_COMPACTION_ROOT_ID", &root.id)
                    .stdout(Stdio::from(log.try_clone().unwrap()))
                    .stderr(Stdio::from(log))
                    .spawn()
                    .unwrap(),
            );
            let deadline = Instant::now() + Duration::from_secs(30);
            while !dir.path().join("ready").exists() {
                assert!(
                    worker.0.try_wait().unwrap().is_none(),
                    "worker exited at {point}: {}",
                    std::fs::read_to_string(dir.path().join("worker.log")).unwrap()
                );
                assert!(Instant::now() < deadline, "worker timeout at {point}");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            worker.0.kill().unwrap();
            assert_eq!(worker.0.wait().unwrap().signal(), Some(9));
            let reopened = Arc::new(
                SqliteStateDb::open_owned(&dir.path().join("state.db"), &owner())
                    .await
                    .unwrap(),
            );
            {
                let conn = reopened
                    .acquire_lock("old owner or compact receipt after SIGKILL")
                    .unwrap();
                assert_eq!(
                    conn.query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
                        .unwrap(),
                    "ok"
                );
                let owners:i64=conn.query_row("SELECT (SELECT count(*) FROM provider_active_destinations WHERE child='asset-compact-1' AND verified_media=1)+(SELECT count(*) FROM download_receipts WHERE child='asset-compact-1')",[],|r|r.get(0)).unwrap();
                assert!(owners > 0, "publication lost its owner at {point}");
            }
            let current = reopened
                .current_download_generation(owner(), spec().scope, spec().config_hash)
                .await
                .unwrap()
                .unwrap();
            reopened
                .seal_selection_generation(owner(), current.id, true, None)
                .await
                .unwrap();
            let successor = reopened
                .begin_selection_generation(owner(), spec(), BUDGET)
                .await
                .unwrap();
            reopened
                .seal_selection_generation(owner(), successor.id, true, None)
                .await
                .unwrap();
            let sha = data_encoding::HEXLOWER.encode(&Sha256::digest(RAW));
            assert!(
                reopened
                    .selection_path_matches_local(
                        owner(),
                        spec().scope,
                        "pass".into(),
                        "asset-compact-1".into(),
                        SelectionPath::from_path(&media),
                        (sha, RAW.len() as u64)
                    )
                    .await
                    .unwrap()
            );
            assert_eq!(std::fs::read(&media).unwrap(), RAW);
            assert_eq!(
                std::fs::metadata(&media).unwrap().modified().unwrap(),
                mtime
            );
        }
    }

    #[tokio::test]
    async fn v027_legacy_completion_sigkill_preserves_rank_owner_or_receipt() {
        for point in [
            "completion-receipts-saved",
            "completion-manifest-retired",
            "completion-before-root-retirement",
            "completion-root-retired",
        ] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(
                dir.path().join("fixture-owner"),
                b"kei-synthetic-process-death",
            )
            .unwrap();
            let db = Arc::new(
                SqliteStateDb::open_owned(&dir.path().join("state.db"), &owner())
                    .await
                    .unwrap(),
            );
            let root = db
                .begin_selection_generation(owner(), legacy_spec(), MAX_GENERATION_BYTES)
                .await
                .unwrap();
            let (_, tasks, _) = admission(
                &db,
                &root.id,
                &dir.path().join("media"),
                1,
                false,
                MAX_GENERATION_BYTES,
            )
            .await;
            publish(&db, &tasks[0]).await;
            db.seal_selection_generation(owner(), root.id.clone(), true, None)
                .await
                .unwrap();
            let media = tasks[0].download_path.clone();
            let mtime = std::fs::metadata(&media).unwrap().modified().unwrap();
            drop(db);
            let log = std::fs::File::create(dir.path().join("worker.log")).unwrap();
            let mut worker = Worker(
                Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", WORKER, "--ignored", "--nocapture"])
                    .env("KEI_TEST_PROCESS_DEATH_ROOT", dir.path())
                    .env("KEI_TEST_PROCESS_DEATH_POINT", point)
                    .env("KEI_LEGACY_RETIREMENT", "1")
                    .stdout(Stdio::from(log.try_clone().unwrap()))
                    .stderr(Stdio::from(log))
                    .spawn()
                    .unwrap(),
            );
            let deadline = Instant::now() + Duration::from_secs(30);
            while !dir.path().join("ready").exists() {
                assert!(
                    worker.0.try_wait().unwrap().is_none(),
                    "worker exited at {point}: {}",
                    std::fs::read_to_string(dir.path().join("worker.log")).unwrap()
                );
                assert!(Instant::now() < deadline, "worker timeout at {point}");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            worker.0.kill().unwrap();
            assert_eq!(worker.0.wait().unwrap().signal(), Some(9));
            let db = SqliteStateDb::open_owned(&dir.path().join("state.db"), &owner())
                .await
                .unwrap();
            {
                let conn = db.acquire_lock("legacy crash ownership").unwrap();
                let old: bool = conn
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM provider_active_sources WHERE generation=?1)",
                        [&root.id],
                        |row| row.get(0),
                    )
                    .unwrap();
                let complete:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM download_receipts WHERE child='asset-compact-1')",[],|row|row.get(0)).unwrap();
                assert!(
                    old || complete,
                    "neither source evidence nor completion survived"
                );
            }
            db.begin_selection_generation(owner(), spec(), MAX_GENERATION_BYTES)
                .await
                .unwrap();
            assert_eq!(
                db.acquire_lock("legacy retry finishes atomic retirement")
                    .unwrap()
                    .query_row(
                        "SELECT count(*) FROM provider_active_generations WHERE id=?1",
                        [&root.id],
                        |row| row.get::<_, i64>(0)
                    )
                    .unwrap(),
                0
            );
            assert_eq!(std::fs::read(&media).unwrap(), RAW);
            assert_eq!(
                std::fs::metadata(&media).unwrap().modified().unwrap(),
                mtime
            );
        }
    }

    #[tokio::test]
    #[ignore = "subprocess worker selected by the parent crash test"]
    async fn v027_retirement_death_worker() {
        let directory =
            std::path::PathBuf::from(std::env::var_os("KEI_TEST_PROCESS_DEATH_ROOT").unwrap());
        assert_eq!(
            std::fs::read(directory.join("fixture-owner")).unwrap(),
            b"kei-synthetic-process-death"
        );
        let db = SqliteStateDb::open_owned(&directory.join("state.db"), &owner())
            .await
            .unwrap();
        let point = std::env::var("KEI_TEST_PROCESS_DEATH_POINT").unwrap();
        if point.contains("root") || std::env::var_os("KEI_LEGACY_RETIREMENT").is_some() {
            db.begin_selection_generation(owner(), spec(), BUDGET)
                .await
                .unwrap();
        } else {
            db.seal_selection_generation(
                owner(),
                std::env::var("KEI_COMPACTION_ROOT_ID").unwrap(),
                true,
                None,
            )
            .await
            .unwrap();
        }
        panic!("requested compaction boundary not reached");
    }
}

#[tokio::test]
async fn v027_modified_completion_compacts_only_with_prepared_transfer_provenance() {
    use crate::state::db::{PreparedPublication, TempFileOwnershipStore};
    for proven in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(
            SqliteStateDb::open_owned(&dir.path().join("state.db"), &owner())
                .await
                .unwrap(),
        );
        let root = db
            .begin_selection_generation(owner(), spec(), BUDGET)
            .await
            .unwrap();
        let (_, tasks, _) =
            admission(&db, &root.id, &dir.path().join("media"), 1, false, BUDGET).await;
        let task = &tasks[0];
        let output = b"original source plus explicitly requested metadata";
        let source = data_encoding::HEXLOWER.encode(&Sha256::digest(RAW));
        let local = data_encoding::HEXLOWER.encode(&Sha256::digest(output));
        std::fs::create_dir_all(task.download_path.parent().unwrap()).unwrap();
        if proven {
            let part = task.download_path.with_extension("part");
            std::fs::write(&part, output).unwrap();
            db.claim_temp_file(&part).await.unwrap();
            db.prepare_temp_publication(
                &part,
                &PreparedPublication {
                    binding: None,
                    library: "PrimarySync".into(),
                    id: task.asset_id.to_string(),
                    version: task.version_size.as_str().into(),
                    provider_checksum: task.checksum.to_string(),
                    provider_size: RAW.len() as u64,
                    temporary: SelectionPath::from_path(&part),
                    destination: SelectionPath::from_path(&task.download_path),
                    local_checksum: local.clone(),
                    source_checksum: source.clone(),
                },
            )
            .await
            .unwrap();
            std::fs::rename(&part, &task.download_path).unwrap();
        } else {
            std::fs::write(&task.download_path, output).unwrap();
        }
        db.mark_verified_download(
            "PrimarySync",
            &task.asset_id,
            task.version_size.as_str(),
            &task.download_path,
            &local,
            Some(&source),
            false,
        )
        .await
        .unwrap();
        db.seal_selection_generation(owner(), root.id.clone(), true, None)
            .await
            .unwrap();
        db.begin_selection_generation(owner(), spec(), BUDGET)
            .await
            .unwrap();
        {
            let conn = db
                .acquire_lock("only trustworthy modified source is compacted")
                .unwrap();
            assert_eq!(
                conn.query_row("SELECT count(*) FROM download_receipts", [], |r| r
                    .get::<_, i64>(0))
                    .unwrap(),
                i64::from(proven)
            );
            let retained:i64=conn.query_row("SELECT count(*) FROM provider_active_decisions WHERE generation=?1 AND compacted_hash IS NULL",[&root.id],|r|r.get(0)).unwrap();
            assert_eq!(
                retained,
                i64::from(!proven),
                "unproven historical evidence must remain owned"
            );
        }
        if proven {
            assert!(
                db.selection_path_matches_local(
                    owner(),
                    spec().scope,
                    "pass".into(),
                    "asset-compact-1".into(),
                    SelectionPath::from_path(&task.download_path),
                    (local.clone(), output.len() as u64)
                )
                .await
                .unwrap()
            );
            assert!(
                !db.selection_path_matches_local(
                    owner(),
                    spec().scope,
                    "pass".into(),
                    "asset-compact-1".into(),
                    SelectionPath::from_path(&task.download_path),
                    ("0".repeat(64), output.len() as u64)
                )
                .await
                .unwrap()
            );
        }
        assert_eq!(std::fs::read(&task.download_path).unwrap(), output);
    }
}

#[tokio::test]
async fn v027_successor_cleanup_does_not_retire_another_library_active_root() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(
        SqliteStateDb::open_owned(&dir.path().join("state.db"), &owner())
            .await
            .unwrap(),
    );
    let primary = db
        .begin_selection_generation(owner(), spec(), BUDGET)
        .await
        .unwrap();
    let mut other = spec();
    other.zone["zoneName"] = json!("SharedSync-other");
    let mut scope: Value = serde_json::from_str(&other.scope).unwrap();
    scope["zone"] = other.zone.clone();
    other.scope = serde_json::to_string(&scope).unwrap();
    other.profile["passes"][0]["scope"]["zone"] = other.zone.clone();
    db.begin_selection_generation(owner(), other, MAX_GENERATION_BYTES)
        .await
        .unwrap();
    let retained = db
        .current_download_generation(owner(), spec().scope, spec().config_hash)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(retained.id, primary.id);
    admission(
        &db,
        &primary.id,
        &dir.path().join("media"),
        1,
        false,
        BUDGET,
    )
    .await;
}

#[tokio::test]
async fn v027_shared_physical_album_copy_keeps_both_fulfilled_passes_after_template_change() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};
    let server = wiremock::MockServer::start().await;
    let bytes = include_bytes!("../../../../../tests/data/media/pattern.jpg");
    Mock::given(method("GET"))
        .and(path("/one-copy"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.as_slice()))
        .expect(1)
        .mount(&server)
        .await;
    let mut f = Fixture::new().await;
    f.config.folder_structure_albums = "same".into();
    f.config.concurrent_downloads = 1;
    f.session.records.lock().unwrap()[0]["fields"]["resOriginalRes"]["value"] = json!({
        "downloadURL":format!("{}/one-copy",server.uri()),"size":bytes.len(),"fileChecksum":CURRENT
    });
    let passes = vec![destination_pass(&f, "A"), destination_pass(&f, "B")];
    let first = crate::download::download_photos_with_sync(
        &reqwest::Client::new(),
        &passes,
        Arc::new(f.config.clone()),
        controls(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(first.stats.downloaded, 1, "{first:?}");
    let original = f.config.directory.join("same/changed.JPG");
    assert_eq!(std::fs::read(&original).unwrap(), bytes);
    let mtime = std::fs::metadata(&original).unwrap().modified().unwrap();
    let keys: Vec<_> = passes
        .iter()
        .map(crate::download::orchestration::generation::pass_key)
        .collect();
    let aliases: String =
        f.db.acquire_lock("one physical receipt fulfills both passes")
            .unwrap()
            .query_row("SELECT origin_pass FROM download_receipts", [], |r| {
                r.get(0)
            })
            .unwrap();
    let aliases: Vec<String> = serde_json::from_str(&aliases).unwrap();
    assert!(keys.iter().all(|key| aliases.contains(key)));
    drop(passes);
    f.config.state_db = None;
    drop(f.capture);
    drop(f.pass);
    drop(f.db);
    f.db = Arc::new(
        SqliteStateDb::open_owned(&f.dir.path().join("state.db"), &owner())
            .await
            .unwrap(),
    );
    f.capture = ShadowCapture::new(f.db.clone(), owner(), "com");
    f.config.state_db = Some(f.db.clone() as Arc<dyn DownloadStore>);
    f.pass = destination_pass_with(&f.session, &f.capture, "A");
    f.config.folder_structure_albums = "changed/{album}".into();
    let passes = vec![destination_pass(&f, "A"), destination_pass(&f, "B")];
    let second = crate::download::download_photos_with_sync(
        &reqwest::Client::new(),
        &passes,
        Arc::new(f.config.clone()),
        controls(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        second.stats.downloaded, 0,
        "fulfilled shared path must stay present: {second:?}"
    );
    assert!(!f.config.directory.join("changed").exists());
    assert_eq!(std::fs::read(&original).unwrap(), bytes);
    assert_eq!(
        std::fs::metadata(&original).unwrap().modified().unwrap(),
        mtime
    );
    let before = f.session.rank_calls.load(Ordering::SeqCst);
    f.config.sync_mode = crate::download::SyncMode::Incremental {
        zone_sync_token: "quiet-after-template".into(),
    };
    for _ in 0..2 {
        let quiet = crate::download::download_photos_with_sync(
            &reqwest::Client::new(),
            &passes,
            Arc::new(f.config.clone()),
            controls(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(quiet.stats.downloaded, 0, "{quiet:?}");
        assert_eq!(f.session.rank_calls.load(Ordering::SeqCst), before);
    }
    assert_eq!(
        f.db.acquire_lock("deduplicated completed path")
            .unwrap()
            .query_row("SELECT count(*) FROM download_receipts", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        std::fs::metadata(&original).unwrap().modified().unwrap(),
        mtime
    );
    server.verify().await;
    f.preserved().await;
}

#[tokio::test]
async fn v027_full_legacy_budget_cannot_block_new_download_ledger_admission() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(
        SqliteStateDb::open_owned(&dir.path().join("state.db"), &owner())
            .await
            .unwrap(),
    );
    let mut legacy = spec();
    legacy.format = 1;
    legacy.profile["coverage"] = json!("observed_selection_window");
    legacy.basis = format!("{:x}", Sha256::digest(b""));
    let old = db
        .begin_selection_generation(owner(), legacy.clone(), MAX_GENERATION_BYTES)
        .await
        .unwrap();
    db.acquire_lock("represent a full retained legacy budget without allocating gigabytes")
        .unwrap()
        .execute(
            "UPDATE provider_active_generations SET charged_bytes=?2 WHERE id=?1",
            rusqlite::params![old.id, MAX_GENERATION_BYTES as i64],
        )
        .unwrap();
    assert!(
        db.begin_selection_generation(owner(), legacy, MAX_GENERATION_BYTES)
            .await
            .is_err(),
        "new format-one work still obeys its strict admission cap"
    );
    let root = db
        .begin_selection_generation(owner(), spec(), BUDGET)
        .await
        .unwrap();
    admission(&db, &root.id, &dir.path().join("media"), 1, false, BUDGET).await;
    let conn = db.acquire_lock("old owner retained exactly").unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT charged_bytes FROM provider_active_generations WHERE id=?1",
            [old.id],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        MAX_GENERATION_BYTES as i64
    );
}

#[tokio::test]
async fn v027_layout_inherited_active_hash_cannot_authorize_publication_reuse() {
    for trusted_compact in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(
            SqliteStateDb::open_owned(&dir.path().join("state.db"), &owner())
                .await
                .unwrap(),
        );
        let root = db
            .begin_selection_generation(owner(), spec(), BUDGET)
            .await
            .unwrap();
        let (manifest, tasks, _) =
            admission(&db, &root.id, &dir.path().join("media"), 1, false, BUDGET).await;
        publish(&db, &tasks[0]).await;
        if trusted_compact {
            // Independent completion evidence was captured before historical
            // layout attribution; that proof must continue to take precedence.
            db.seal_selection_generation(owner(), root.id.clone(), true, None)
                .await
                .unwrap();
        }
        db.acquire_lock("model historical layout source attribution").unwrap().execute(
            "INSERT INTO primary_layout_bindings(family,library,source_library,child,generation,decision,policy,binding) VALUES('historical','PrimarySync','PrimarySync',?1,1,'legacy','legacy',X'7B7D')",
            [&manifest.state_id],
        ).unwrap();
        let destination = &tasks[0].download_path;
        let before = std::fs::read(destination).unwrap();
        let mtime = std::fs::metadata(destination).unwrap().modified().unwrap();
        let sha = data_encoding::HEXLOWER.encode(&Sha256::digest(&before));
        let reused = db
            .reuse_selection_publication(
                owner(),
                root.id.clone(),
                "pass".into(),
                manifest.decision.child.clone(),
                SelectionPath::from_path(destination),
                (sha.clone(), before.len() as u64),
                authorization(
                    &root.spec.scope,
                    &root.id,
                    &manifest.decision.child,
                    0,
                    false,
                ),
            )
            .await
            .unwrap();
        assert_eq!(
            reused, trusted_compact,
            "a fresh local hash does not prove an inherited layout source; independent compact proof does"
        );
        assert_eq!(std::fs::read(destination).unwrap(), before);
        assert_eq!(
            std::fs::metadata(destination).unwrap().modified().unwrap(),
            mtime
        );
        if trusted_compact {
            let marker: String = db
                .acquire_lock("read native reuse marker")
                .unwrap()
                .query_row(
                    "SELECT preserved_path_key FROM asset_metadata_paths
                     WHERE library='PrimarySync' AND id=?1 AND version_size='original'
                       AND local_path=?2",
                    rusqlite::params![manifest.state_id, destination.to_str().unwrap()],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                marker,
                crate::state::db::primary_layout::path_key(destination).unwrap()
            );
            assert!(db.guard_primary_writer(destination).await.is_err());
            assert_eq!(
                db.source_metadata_recovery(destination).await.unwrap(),
                crate::state::db::SourceMetadataRecovery::ReadOnly
            );
            let mut sidecar = destination.as_os_str().to_os_string();
            sidecar.push(".xmp");
            let sidecar = std::path::PathBuf::from(sidecar);
            let protected = db.protected_legacy_paths().await.unwrap();
            assert!(protected.iter().any(|path| path == destination));
            assert!(protected.iter().any(|path| path == &sidecar));
            let reopened = SqliteStateDb::open_owned(&dir.path().join("state.db"), &owner())
                .await
                .unwrap();
            assert!(reopened.guard_primary_writer(destination).await.is_err());
            assert_eq!(
                reopened
                    .source_metadata_recovery(destination)
                    .await
                    .unwrap(),
                crate::state::db::SourceMetadataRecovery::ReadOnly
            );
            db.complete_selection_metadata(owner(), root.id.clone())
                .await
                .unwrap();
            let marker_after_compaction: String = db
                .acquire_lock("marker survives selection compaction")
                .unwrap()
                .query_row(
                    "SELECT preserved_path_key FROM asset_metadata_paths
                     WHERE library='PrimarySync' AND id=?1 AND version_size='original'
                       AND local_path=?2",
                    rusqlite::params![manifest.state_id, destination.to_str().unwrap()],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(marker_after_compaction, marker);
            db.acquire_lock("conflicting selection marker")
                .unwrap()
                .execute(
                    "UPDATE asset_metadata_paths SET preserved_path_key='conflict'
                     WHERE library='PrimarySync' AND id=?1 AND version_size='original'
                       AND local_path=?2",
                    rusqlite::params![manifest.state_id, destination.to_str().unwrap()],
                )
                .unwrap();
            assert!(
                db.reuse_selection_publication(
                    owner(),
                    root.id.clone(),
                    "pass".into(),
                    manifest.decision.child.clone(),
                    SelectionPath::from_path(destination),
                    (sha.clone(), before.len() as u64),
                    authorization(
                        &root.spec.scope,
                        &root.id,
                        &manifest.decision.child,
                        0,
                        false
                    ),
                )
                .await
                .is_err()
            );
            db.acquire_lock("remove selection compatibility receipt")
                .unwrap()
                .execute(
                    "DELETE FROM asset_metadata_paths
                     WHERE library='PrimarySync' AND id=?1 AND version_size='original'
                       AND local_path=?2",
                    rusqlite::params![manifest.state_id, destination.to_str().unwrap()],
                )
                .unwrap();
            assert!(
                !db.reuse_selection_publication(
                    owner(),
                    root.id.clone(),
                    "pass".into(),
                    manifest.decision.child.clone(),
                    SelectionPath::from_path(destination),
                    (sha, before.len() as u64),
                    authorization(
                        &root.spec.scope,
                        &root.id,
                        &manifest.decision.child,
                        0,
                        false
                    ),
                )
                .await
                .unwrap()
            );
        } else {
            let marker: Option<Option<String>> = db
                .acquire_lock("no native reuse marker")
                .unwrap()
                .query_row(
                    "SELECT preserved_path_key FROM asset_metadata_paths
                     WHERE library='PrimarySync' AND id=?1 AND version_size='original'
                       AND local_path=?2",
                    rusqlite::params![manifest.state_id, destination.to_str().unwrap()],
                    |row| row.get(0),
                )
                .optional()
                .unwrap();
            assert!(marker.flatten().is_none());
        }
    }
}

#[tokio::test]
async fn v027_layout_master_predecessor_cannot_launder_child_publication() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(
        SqliteStateDb::open_owned(&dir.path().join("state.db"), &owner())
            .await
            .unwrap(),
    );
    let old = db
        .begin_selection_generation(owner(), spec(), BUDGET)
        .await
        .unwrap();
    let (old_manifest, tasks, mut rows) = admission_with_legacy_state(
        &db,
        &old.id,
        &dir.path().join("media"),
        1,
        false,
        BUDGET,
        true,
    )
    .await;
    publish(&db, &tasks[0]).await;
    db.acquire_lock("historical binding owns master alias").unwrap().execute(
        "INSERT INTO primary_layout_bindings(family,library,source_library,child,generation,decision,policy,binding) VALUES('master-old','PrimarySync','PrimarySync',?1,1,'legacy','legacy',X'7B7D')",
        [&old_manifest.state_id],
    ).unwrap();
    let next = db
        .begin_selection_generation(owner(), spec(), BUDGET)
        .await
        .unwrap();
    let mut manifest = old_manifest;
    manifest.state_id = manifest.decision.child.clone();
    for row in &mut rows {
        row.id = manifest.state_id.clone().into();
    }
    db.project_selection_decision(owner(), next.id.clone(), manifest.clone(), rows, BUDGET)
        .await
        .unwrap();
    let path = &tasks[0].download_path;
    let sha = data_encoding::HEXLOWER.encode(&Sha256::digest(RAW));
    assert!(
        !db.reuse_selection_publication(
            owner(),
            next.id.clone(),
            "pass".into(),
            manifest.decision.child.clone(),
            SelectionPath::from_path(path),
            (sha, RAW.len() as u64),
            authorization(
                &next.spec.scope,
                &next.id,
                &manifest.decision.child,
                0,
                false
            ),
        )
        .await
        .unwrap()
    );
    assert_eq!(std::fs::read(path).unwrap(), RAW);
}

fn legacy_spec() -> GenerationSpec {
    let mut legacy = spec();
    legacy.format = 1;
    legacy.basis = format!("{:x}", Sha256::digest(b""));
    legacy.profile["coverage"] = json!("observed_selection_window");
    legacy.profile["passes"][0]["scope"]["list_type"] =
        json!("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted");
    legacy.profile["passes"][0]["scope"]["query_filter"] = Value::Null;
    legacy
}

fn legacy_rank_request() -> Value {
    json!({"zoneID":spec().zone,"resultsLimit":200,
    "query":{"recordType":"CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted","filterBy":[
        {"fieldName":"startRank","fieldValue":{"type":"INT64","value":0},"comparator":"EQUALS"},
        {"fieldName":"direction","fieldValue":{"type":"STRING","value":"ASCENDING"},"comparator":"EQUALS"}
    ]}})
}

#[tokio::test]
async fn v027_sealed_legacy_completion_retires_only_after_pending_rank_owners_finish() {
    for unaccounted in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(
            SqliteStateDb::open_owned(&dir.path().join("state.db"), &owner())
                .await
                .unwrap(),
        );
        let root = db
            .begin_selection_generation(owner(), legacy_spec(), MAX_GENERATION_BYTES)
            .await
            .unwrap();
        let (_, done, _) = admission(
            &db,
            &root.id,
            &dir.path().join("media"),
            1,
            false,
            MAX_GENERATION_BYTES,
        )
        .await;
        let (pending, tasks, _) = admission(
            &db,
            &root.id,
            &dir.path().join("media"),
            2,
            false,
            MAX_GENERATION_BYTES,
        )
        .await;
        publish(&db, &done[0]).await;
        if unaccounted {
            let body = serde_json::to_vec(
                &json!({"records":records("unadmitted",CURRENT,"unfinished admission")}),
            )
            .unwrap();
            db.capture_selection_rank_page(
                owner(),
                root.id.clone(),
                "pass".into(),
                legacy_rank_request(),
                body,
                MAX_GENERATION_BYTES,
            )
            .await
            .unwrap();
        }
        db.seal_selection_generation(owner(), root.id.clone(), true, None)
            .await
            .unwrap();
        let pages_before: i64 = db
            .acquire_lock("original rank owner count")
            .unwrap()
            .query_row(
                "SELECT count(*) FROM provider_selection_rank_pages WHERE generation=?1",
                [&root.id],
                |row| row.get(0),
            )
            .unwrap();
        db.begin_selection_generation(owner(), spec(), MAX_GENERATION_BYTES)
            .await
            .unwrap();
        {
            let conn = db
                .acquire_lock("mixed legacy owners retain all rank evidence")
                .unwrap();
            assert_eq!(
                conn.query_row("SELECT count(*) FROM download_receipts", [], |row| row
                    .get::<_, i64>(0))
                    .unwrap(),
                1
            );
            assert_eq!(
                conn.query_row(
                    "SELECT count(*) FROM provider_selection_rank_pages WHERE generation=?1",
                    [&root.id],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
                pages_before
            );
            assert_eq!(
                conn.query_row(
                    "SELECT count(*) FROM provider_active_sources WHERE generation=?1",
                    [&root.id],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
                pending.sources.len() as i64
            );
        }
        drop(db);
        let db = Arc::new(
            SqliteStateDb::open_owned(&dir.path().join("state.db"), &owner())
                .await
                .unwrap(),
        );
        assert!(
            db.has_pending_selection(owner(), root.id.clone())
                .await
                .unwrap()
        );
        publish(&db, &tasks[0]).await;
        db.begin_selection_generation(owner(), spec(), MAX_GENERATION_BYTES)
            .await
            .unwrap();
        let conn = db
            .acquire_lock("positive census controls final legacy retirement")
            .unwrap();
        assert_eq!(
            conn.query_row("SELECT count(*) FROM download_receipts", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            2
        );
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM provider_active_generations WHERE id=?1",
                [&root.id],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            i64::from(unaccounted)
        );
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM provider_selection_rank_pages WHERE generation=?1",
                [&root.id],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            if unaccounted { pages_before } else { 0 }
        );
        assert_eq!(std::fs::read(&done[0].download_path).unwrap(), RAW);
        assert_eq!(std::fs::read(&tasks[0].download_path).unwrap(), RAW);
    }
}

#[tokio::test]
async fn v027_unsealed_legacy_completion_keeps_original_observation_owner() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(
        SqliteStateDb::open_owned(&dir.path().join("state.db"), &owner())
            .await
            .unwrap(),
    );
    let root = db
        .begin_selection_generation(owner(), legacy_spec(), MAX_GENERATION_BYTES)
        .await
        .unwrap();
    let (_, tasks, _) = admission(
        &db,
        &root.id,
        &dir.path().join("media"),
        1,
        false,
        MAX_GENERATION_BYTES,
    )
    .await;
    publish(&db, &tasks[0]).await;
    db.begin_selection_generation(owner(), spec(), MAX_GENERATION_BYTES)
        .await
        .unwrap();
    let conn = db
        .acquire_lock("no seal invented for interrupted coverage")
        .unwrap();
    assert_eq!(
        conn.query_row("SELECT count(*) FROM download_receipts", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert!(
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM provider_active_sources WHERE generation=?1)",
            [&root.id],
            |row| row.get::<_, bool>(0)
        )
        .unwrap()
    );
}

#[tokio::test]
async fn v027_retained_mixed_legacy_roots_cannot_starve_newer_completed_retirement() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(
        SqliteStateDb::open_owned(&dir.path().join("state.db"), &owner())
            .await
            .unwrap(),
    );
    let mut mixed = Vec::new();
    for index in 0..65 {
        let root = db
            .begin_selection_generation(owner(), legacy_spec(), MAX_GENERATION_BYTES)
            .await
            .unwrap();
        let (_, done, _) = admission(
            &db,
            &root.id,
            &dir.path().join("media"),
            index * 2,
            false,
            MAX_GENERATION_BYTES,
        )
        .await;
        admission(
            &db,
            &root.id,
            &dir.path().join("media"),
            index * 2 + 1,
            false,
            MAX_GENERATION_BYTES,
        )
        .await;
        publish(&db, &done[0]).await;
        db.seal_selection_generation(owner(), root.id.clone(), true, None)
            .await
            .unwrap();
        mixed.push(root.id);
    }
    let complete = db
        .begin_selection_generation(owner(), legacy_spec(), MAX_GENERATION_BYTES)
        .await
        .unwrap();
    let (_, tasks, _) = admission(
        &db,
        &complete.id,
        &dir.path().join("media"),
        1000,
        false,
        MAX_GENERATION_BYTES,
    )
    .await;
    publish(&db, &tasks[0]).await;
    db.seal_selection_generation(owner(), complete.id.clone(), true, None)
        .await
        .unwrap();
    // Two bounded compaction batches must reach all 66 completed decisions.
    for _ in 0..2 {
        db.begin_selection_generation(owner(), spec(), MAX_GENERATION_BYTES)
            .await
            .unwrap();
    }
    let conn = db
        .acquire_lock("positive census eligibility precedes retirement batch limit")
        .unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT count(*) FROM provider_active_generations WHERE id=?1",
            [&complete.id],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    for id in mixed {
        assert!(
            conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM provider_active_generations WHERE id=?1)",
                [&id],
                |row| row.get::<_, bool>(0)
            )
            .unwrap()
        );
        assert!(
            conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM provider_selection_rank_pages WHERE generation=?1)",
                [&id],
                |row| row.get::<_, bool>(0)
            )
            .unwrap()
        );
    }
    assert_eq!(std::fs::read(&tasks[0].download_path).unwrap(), RAW);
}

#[tokio::test]
async fn v027_invalid_optional_legacy_seal_does_not_block_current_admission() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let db = Arc::new(SqliteStateDb::open_owned(&path, &owner()).await.unwrap());
    let old = db
        .begin_selection_generation(owner(), legacy_spec(), MAX_GENERATION_BYTES)
        .await
        .unwrap();
    let (_, tasks, _) = admission(
        &db,
        &old.id,
        &dir.path().join("media"),
        0,
        false,
        MAX_GENERATION_BYTES,
    )
    .await;
    publish(&db, &tasks[0]).await;
    db.seal_selection_generation(owner(), old.id.clone(), true, None)
        .await
        .unwrap();
    let original: Vec<u8> = {
        let conn = db
            .acquire_lock("corrupt optional legacy completion seal")
            .unwrap();
        conn.execute(
            "UPDATE provider_active_generations SET seal_hash='invalid' WHERE id=?1",
            [&old.id],
        )
        .unwrap();
        conn.query_row(
            "SELECT manifest FROM provider_active_decisions WHERE generation=?1",
            [&old.id],
            |row| row.get(0),
        )
        .unwrap()
    };
    for index in 1..=2 {
        let current = db
            .begin_selection_generation(owner(), spec(), BUDGET)
            .await
            .unwrap();
        let (_, fresh, _) = admission(
            &db,
            &current.id,
            &dir.path().join("media"),
            index,
            false,
            BUDGET,
        )
        .await;
        publish(&db, &fresh[0]).await;
        db.seal_selection_generation(owner(), current.id, true, None)
            .await
            .unwrap();
    }
    drop(db);
    let reopened = SqliteStateDb::open_owned(&path, &owner()).await.unwrap();
    let conn = reopened
        .acquire_lock("legacy evidence survives cleanup rollback and reopen")
        .unwrap();
    let retained: (Vec<u8>, Option<String>) = conn
        .query_row(
            "SELECT manifest,compacted_hash FROM provider_active_decisions WHERE generation=?1",
            [&old.id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(retained, (original, None));
    assert_eq!(std::fs::read(&tasks[0].download_path).unwrap(), RAW);
}

#[tokio::test]
async fn v027_current_pressure_never_revalidates_unrelated_legacy_completion() {
    use crate::state::db::provider_generations::take_legacy_compaction_validation_count;
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(
        SqliteStateDb::open_owned(&dir.path().join("state.db"), &owner())
            .await
            .unwrap(),
    );
    let old = db
        .begin_selection_generation(owner(), legacy_spec(), MAX_GENERATION_BYTES)
        .await
        .unwrap();
    for index in 0..65 {
        let (_, tasks, _) = admission(
            &db,
            &old.id,
            &dir.path().join("media"),
            index,
            false,
            MAX_GENERATION_BYTES,
        )
        .await;
        publish(&db, &tasks[0]).await;
    }
    db.seal_selection_generation(owner(), old.id.clone(), true, None)
        .await
        .unwrap();
    take_legacy_compaction_validation_count(&old.id);
    let current = db
        .begin_selection_generation(owner(), spec(), BUDGET)
        .await
        .unwrap();
    assert_eq!(
        take_legacy_compaction_validation_count(&old.id),
        1,
        "one optional batch validates the historical root once"
    );
    let (blocked, _, _) = admission(
        &db,
        &current.id,
        &dir.path().join("media"),
        100,
        true,
        BUDGET,
    )
    .await;
    let started = std::time::Instant::now();
    for index in 101..141 {
        let (_, tasks, _) = admission(
            &db,
            &current.id,
            &dir.path().join("media"),
            index,
            false,
            BUDGET,
        )
        .await;
        publish(&db, &tasks[0]).await;
    }
    assert_eq!(
        take_legacy_compaction_validation_count(&old.id),
        0,
        "current budget pressure must not traverse legacy evidence"
    );
    let retained = db
        .selection_decision(
            owner(),
            current.id.clone(),
            "pass".into(),
            blocked.decision.child.clone(),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        retained.decision.confirmation,
        blocked.decision.confirmation
    );
    let conn = db.acquire_lock("measure current compacted bodies").unwrap();
    let (compacted, bulky): (i64,i64) = conn.query_row(
        "SELECT sum(compacted_hash IS NOT NULL),sum(CASE WHEN compacted_hash IS NULL THEN length(manifest) ELSE 0 END) FROM provider_active_decisions WHERE generation=?1",
        [&current.id], |row| Ok((row.get(0)?,row.get(1)?))).unwrap();
    assert!(compacted > 0);
    assert!(bulky < BUDGET as i64);
    eprintln!(
        "measured: 40 current publications with blocked companion; compacted={compacted}, remaining_manifest_bytes={bulky}, legacy_pressure_validations=0, elapsed_ms={}",
        started.elapsed().as_millis()
    );
}

#[tokio::test]
async fn v027_sustained_successors_reclaim_bodies_with_blocked_predecessor_after_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let mut db = Arc::new(SqliteStateDb::open_owned(&path, &owner()).await.unwrap());
    let old = db
        .begin_selection_generation(owner(), spec(), BUDGET)
        .await
        .unwrap();
    let (blocked, tasks, _) =
        admission(&db, &old.id, &dir.path().join("media"), 0, true, BUDGET).await;
    publish(&db, &tasks[0]).await;
    let mtime = std::fs::metadata(&tasks[0].download_path)
        .unwrap()
        .modified()
        .unwrap();
    db.seal_selection_generation(owner(), old.id.clone(), true, None)
        .await
        .unwrap();
    let started = std::time::Instant::now();
    for cycle in 0..12 {
        drop(db);
        db = Arc::new(SqliteStateDb::open_owned(&path, &owner()).await.unwrap());
        let next = db
            .begin_selection_generation(owner(), spec(), BUDGET)
            .await
            .unwrap();
        for item in 1..=10 {
            let (_, fresh, _) = admission(
                &db,
                &next.id,
                &dir.path().join("media"),
                1 + cycle * 10 + item,
                false,
                BUDGET,
            )
            .await;
            publish(&db, &fresh[0]).await;
        }
        db.seal_selection_generation(owner(), next.id, true, None)
            .await
            .unwrap();
        let retained = db
            .selection_decision(
                owner(),
                old.id.clone(),
                "pass".into(),
                blocked.decision.child.clone(),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            retained.decision.confirmation,
            blocked.decision.confirmation
        );
        assert_eq!(std::fs::read(&tasks[0].download_path).unwrap(), RAW);
        assert_eq!(
            std::fs::metadata(&tasks[0].download_path)
                .unwrap()
                .modified()
                .unwrap(),
            mtime
        );
        assert!(!tasks[1].download_path.exists());
        let conn = db
            .acquire_lock("sustained compact ledger with blocked predecessor")
            .unwrap();
        let roots: i64 = conn
            .query_row(
                "SELECT count(*) FROM provider_active_generations",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            roots <= 2,
            "only blocked predecessor and current successor remain"
        );
        let bulky: i64 = conn.query_row("SELECT sum(length(manifest)) FROM provider_active_decisions WHERE compacted_hash IS NULL", [], |r| r.get(0)).unwrap();
        assert!(bulky < BUDGET as i64);
    }
    eprintln!(
        "measured: 12 reopened successor cycles, 120 publications, one blocked companion retained, maximum roots=2, remaining bulky evidence below 65536 bytes, elapsed_ms={}",
        started.elapsed().as_millis()
    );
}

#[tokio::test]
async fn v027_pending_reservation_reader_cost_with_unrelated_blocked_work() {
    use crate::state::db::ReconciliationStateStore;
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(
        SqliteStateDb::open_owned(&dir.path().join("state.db"), &owner())
            .await
            .unwrap(),
    );
    let root = db
        .begin_selection_generation(owner(), spec(), MAX_GENERATION_BYTES)
        .await
        .unwrap();
    let mut admitted = 0;
    for count in [0, 10, 100] {
        while admitted < count {
            admission(
                &db,
                &root.id,
                &dir.path().join("media"),
                admitted,
                false,
                MAX_GENERATION_BYTES,
            )
            .await;
            admitted += 1;
        }
        let started = std::time::Instant::now();
        for _ in 0..10 {
            let reservations = db.get_reconciliation_reservations().await.unwrap();
            assert_eq!(reservations.len(), count);
            assert!(
                reservations
                    .iter()
                    .all(|reservation| reservation.library.as_ref() == "PrimarySync")
            );
        }
        eprintln!(
            "measured reservation reader: pending={count}, calls=10, elapsed_us={}",
            started.elapsed().as_micros()
        );
        let started = std::time::Instant::now();
        for _ in 0..10 {
            let conn = db
                .acquire_lock("measure path-specific pending reservations")
                .unwrap();
            assert!(
                crate::state::db::provider_generations::pending_destination_reservations_for_path(
                    &conn,
                    &dir.path().join("unrelated.JPG")
                )
                .unwrap()
                .is_empty()
            );
        }
        eprintln!(
            "measured path-specific reservation reader: pending={count}, calls=10, elapsed_us={}",
            started.elapsed().as_micros()
        );
    }
}

#[tokio::test]
async fn v027_path_lookup_keeps_collision_aliases_and_rejects_uncertain_projections() {
    use crate::state::db::provider_generations::pending_destination_reservations_for_path;
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("state.db");
    let db = Arc::new(SqliteStateDb::open_owned(&db_path, &owner()).await.unwrap());
    let root = db
        .begin_selection_generation(owner(), spec(), BUDGET)
        .await
        .unwrap();
    let (manifest, tasks, _) = admission(
        &db,
        &root.id,
        &dir.path().join("media 1.30 PM"),
        0,
        true,
        BUDGET,
    )
    .await;
    let path = &tasks[0].download_path;
    let original = serde_json::to_vec(&manifest).unwrap();
    {
        let conn = db
            .acquire_lock("collision equivalence and damaged projection controls")
            .unwrap();
        for alias in [
            path.to_path_buf(),
            path.parent()
                .unwrap()
                .join(".")
                .join(path.file_name().unwrap()),
            std::path::PathBuf::from(path.to_str().unwrap().to_uppercase()),
            std::path::PathBuf::from(path.to_str().unwrap().replace(" PM", " PM")),
            std::path::PathBuf::from(path.to_str().unwrap().replace(" PM", "PM")),
        ] {
            assert!(
                !pending_destination_reservations_for_path(&conn, &alias)
                    .unwrap()
                    .is_empty()
            );
        }
        assert!(
            pending_destination_reservations_for_path(&conn, &dir.path().join("unrelated"))
                .unwrap()
                .is_empty()
        );
        // A missing/mutated summary cannot hide a saved manifest destination.
        conn.execute_batch("SAVEPOINT damaged_path").unwrap();
        conn.execute(
            "UPDATE provider_active_destinations SET path='{}' WHERE generation=?1",
            [&root.id],
        )
        .unwrap();
        assert!(pending_destination_reservations_for_path(&conn, path).is_err());
        assert!(
            pending_destination_reservations_for_path(&conn, &dir.path().join("unrelated"))
                .is_err()
        );
        conn.execute_batch("ROLLBACK TO damaged_path; RELEASE damaged_path")
            .unwrap();
        // Even an unrelated manifest must be hash-checked before excluding it.
        conn.execute_batch("SAVEPOINT damaged_manifest").unwrap();
        conn.execute(
            "UPDATE provider_active_decisions SET manifest_hash='invalid' WHERE generation=?1",
            [&root.id],
        )
        .unwrap();
        assert!(
            pending_destination_reservations_for_path(&conn, &dir.path().join("unrelated"))
                .is_err()
        );
        conn.execute_batch("ROLLBACK TO damaged_manifest; RELEASE damaged_manifest")
            .unwrap();
        // A matching candidate still requires the original complete validation.
        conn.execute_batch("SAVEPOINT damaged_body").unwrap();
        let mut damaged = manifest.clone();
        damaged.decision.confirmation = Some(b"{}".to_vec());
        let bytes = serde_json::to_vec(&damaged).unwrap();
        let hash = format!("{:x}", Sha256::digest(&bytes));
        conn.execute(
            "UPDATE provider_active_decisions SET manifest=?2,manifest_hash=?3 WHERE generation=?1",
            rusqlite::params![root.id, bytes, hash],
        )
        .unwrap();
        assert!(pending_destination_reservations_for_path(&conn, path).is_err());
        conn.execute_batch("ROLLBACK TO damaged_body; RELEASE damaged_body")
            .unwrap();
        let retained: Vec<u8> = conn
            .query_row(
                "SELECT manifest FROM provider_active_decisions WHERE generation=?1",
                [&root.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(retained, original);
    }
    let mut changed = spec();
    changed.config_hash = "c".repeat(64);
    db.begin_selection_generation(owner(), changed, BUDGET)
        .await
        .unwrap();
    drop(db);
    let reopened = SqliteStateDb::open_owned(&db_path, &owner()).await.unwrap();
    let conn = reopened
        .acquire_lock("pending ownership survives reopen")
        .unwrap();
    assert!(
        !pending_destination_reservations_for_path(&conn, path)
            .unwrap()
            .is_empty()
    );
    assert!(tasks.iter().all(|task| !task.download_path.exists()));
}

#[cfg(unix)]
#[tokio::test]
async fn v027_path_lookup_keeps_native_non_utf8_collision_owners() {
    use crate::state::db::provider_generations::pending_destination_reservations_for_path;
    use std::os::unix::ffi::OsStringExt;
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(
        SqliteStateDb::open_owned(&dir.path().join("state.db"), &owner())
            .await
            .unwrap(),
    );
    let root = db
        .begin_selection_generation(owner(), spec(), BUDGET)
        .await
        .unwrap();
    let media = dir
        .path()
        .join(std::ffi::OsString::from_vec(b"media-\xff".to_vec()));
    let (_, tasks, _) = admission(&db, &root.id, &media, 0, false, BUDGET).await;
    let conn = db.acquire_lock("native path lookup").unwrap();
    assert_eq!(
        pending_destination_reservations_for_path(&conn, &tasks[0].download_path)
            .unwrap()
            .len(),
        1
    );
    assert!(
        pending_destination_reservations_for_path(&conn, &dir.path().join("other"))
            .unwrap()
            .is_empty()
    );
}

mod lookup_measurement;

#[tokio::test]
async fn v027_path_cache_absence_invalidates_on_local_external_and_rolled_back_changes() {
    use crate::state::db::provider_generations::pending_destination_reservations_for_path;
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("state.db");
    let db = Arc::new(SqliteStateDb::open_owned(&db_path, &owner()).await.unwrap());
    let root = db
        .begin_selection_generation(owner(), spec(), BUDGET)
        .await
        .unwrap();
    let media = dir.path().join("media");
    let first = media.join("0/changed.JPG");
    let second = media.join("1/changed.JPG");
    {
        let conn = db.acquire_lock("prime empty candidate cache").unwrap();
        assert!(
            pending_destination_reservations_for_path(&conn, &first)
                .unwrap()
                .is_empty()
        );
    }
    let (_, tasks, _) = admission(&db, &root.id, &media, 0, false, BUDGET).await;
    assert_eq!(tasks[0].download_path, first);
    {
        let conn = db
            .acquire_lock("local insertion replaces cached absence")
            .unwrap();
        assert_eq!(
            pending_destination_reservations_for_path(&conn, &first)
                .unwrap()
                .len(),
            1
        );
        assert!(
            pending_destination_reservations_for_path(&conn, &second)
                .unwrap()
                .is_empty()
        );
        conn.execute_batch("SAVEPOINT malformed_pending").unwrap();
        conn.execute(
            "UPDATE provider_active_decisions SET manifest_hash='invalid' WHERE generation=?1",
            [&root.id],
        )
        .unwrap();
        assert!(pending_destination_reservations_for_path(&conn, &second).is_err());
        conn.execute_batch("ROLLBACK TO malformed_pending; RELEASE malformed_pending")
            .unwrap();
        assert_eq!(
            pending_destination_reservations_for_path(&conn, &first)
                .unwrap()
                .len(),
            1
        );
    }
    let other = Arc::new(SqliteStateDb::open_owned(&db_path, &owner()).await.unwrap());
    let (_, external, _) = admission(&other, &root.id, &media, 1, false, BUDGET).await;
    assert_eq!(external[0].download_path, second);
    {
        let conn = db
            .acquire_lock("external insertion invalidates cached absence")
            .unwrap();
        assert_eq!(
            pending_destination_reservations_for_path(&conn, &second)
                .unwrap()
                .len(),
            1
        );
        conn.execute_batch("CREATE TABLE measurement_schema_drift(value TEXT)")
            .unwrap();
        assert_eq!(
            pending_destination_reservations_for_path(&conn, &first)
                .unwrap()
                .len(),
            1
        );
        conn.execute_batch("PRAGMA query_only=ON").unwrap();
        assert_eq!(
            pending_destination_reservations_for_path(&conn, &second)
                .unwrap()
                .len(),
            1
        );
        conn.execute_batch("PRAGMA query_only=OFF").unwrap();
    }
    publish(&other, &external[0]).await;
    {
        let conn = db
            .acquire_lock("external completion retires only completed candidate")
            .unwrap();
        assert!(
            pending_destination_reservations_for_path(&conn, &second)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            pending_destination_reservations_for_path(&conn, &first)
                .unwrap()
                .len(),
            1
        );
        let durable_cache: i64 = conn.query_row(
            "SELECT count(*) FROM main.sqlite_master WHERE name LIKE 'pending_path_candidates%'",
            [],|r|r.get(0)).unwrap();
        assert_eq!(durable_cache, 0);
    }
    drop(other);
    drop(db);
    let reopened = SqliteStateDb::open_owned(&db_path, &owner()).await.unwrap();
    let conn = reopened
        .acquire_lock("reopen rebuilds only disposable candidates")
        .unwrap();
    assert_eq!(
        pending_destination_reservations_for_path(&conn, &first)
            .unwrap()
            .len(),
        1
    );
    assert!(
        pending_destination_reservations_for_path(&conn, &second)
            .unwrap()
            .is_empty()
    );
    assert!(!first.exists());
    assert_eq!(std::fs::read(second).unwrap(), RAW);
}

fn reference_version(
    version: &str,
) -> (
    String,
    crate::state::VersionSizeKey,
    crate::state::VersionSizeKey,
) {
    let key = crate::state::VersionSizeKey::from_str(version).unwrap();
    (version.to_owned(), key, key)
}
fn refreshed_reference(original: &ActiveDecision) -> ActiveDecision {
    let mut updated = original.clone();
    let master = "refreshed-current-master";
    let mut body: Value =
        serde_json::from_slice(updated.decision.confirmation.as_ref().unwrap()).unwrap();
    for record in body["records"].as_array_mut().unwrap() {
        if record["recordType"] == "CPLMaster" {
            record["recordName"] = json!(master);
        }
        if record["recordType"] == "CPLAsset" {
            record["fields"]["masterRef"]["value"]["recordName"] = json!(master);
        }
    }
    updated.decision.master = Some(master.into());
    updated.decision.confirmation = Some(serde_json::to_vec(&body).unwrap());
    updated
}
fn current_charge(db: &SqliteStateDb) -> u64 {
    let conn = db.acquire_lock("current reference charge").unwrap();
    let value:i64=conn.query_row("SELECT COALESCE((SELECT SUM(charged_bytes) FROM provider_active_generations),0)+COALESCE((SELECT SUM(charged_bytes) FROM provider_active_decisions WHERE compacted_hash IS NULL),0)",[],|r|r.get(0)).unwrap();
    value.try_into().unwrap()
}
#[tokio::test]
async fn changed_master_capacity_uses_committed_charge_and_rolls_back_overflow() {
    let mut fitting = None;
    for mode in 0..3 {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(
            SqliteStateDb::open_owned(&dir.path().join("state.db"), &owner())
                .await
                .unwrap(),
        );
        let root = db
            .begin_selection_generation(owner(), spec(), BUDGET)
            .await
            .unwrap();
        let (old, _, rows) =
            admission(&db, &root.id, &dir.path().join("media"), 0, false, BUDGET).await;
        db.seal_selection_generation(owner(), root.id.clone(), true, None)
            .await
            .unwrap();
        let updated = refreshed_reference(&old);
        let before = current_charge(&db);
        let limit = match mode {
            0 => BUDGET,
            1 => fitting.unwrap(),
            _ => fitting.unwrap() - 1,
        };
        let outcome = db
            .refresh_selection_reference(
                owner(),
                root.id.clone(),
                old.clone(),
                updated,
                (
                    rows,
                    old.decision
                        .destinations
                        .iter()
                        .map(|d| reference_version(&d.version_size))
                        .collect(),
                ),
                limit,
            )
            .await;
        if mode == 2 {
            assert!(matches!(
                outcome,
                Err(crate::state::error::StateError::ProviderSelectionFull)
            ));
            assert_eq!(current_charge(&db), before);
            assert_eq!(
                db.selection_decision(
                    owner(),
                    root.id.clone(),
                    "pass".into(),
                    old.decision.child.clone()
                )
                .await
                .unwrap(),
                Some(old)
            );
            assert_eq!(
                db.acquire_lock("no leaked successor")
                    .unwrap()
                    .query_row::<i64, _, _>(
                        "SELECT count(*) FROM provider_active_generations",
                        [],
                        |r| r.get(0)
                    )
                    .unwrap(),
                1
            );
        } else {
            outcome.unwrap();
            let after = current_charge(&db);
            if mode == 0 {
                fitting = Some(after);
            } else {
                assert_eq!(after, limit);
            }
        }
    }
}
#[tokio::test]
async fn changed_master_partial_preserves_merged_receipt_and_refuses_bad_proof() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(
        SqliteStateDb::open_owned(&dir.path().join("state.db"), &owner())
            .await
            .unwrap(),
    );
    let root = db
        .begin_selection_generation(owner(), spec(), BUDGET)
        .await
        .unwrap();
    let (old, tasks, rows) =
        admission(&db, &root.id, &dir.path().join("media"), 0, true, BUDGET).await;
    assert_eq!(tasks.len(), 2);
    publish(&db, &tasks[0]).await;
    db.seal_selection_generation(owner(), root.id.clone(), true, None)
        .await
        .unwrap();
    let saved = {
        let conn = db.acquire_lock("merge fulfilled receipt origins").unwrap();
        SqliteStateDb::merge_test_completed_origin(
            &conn,
            &root.id,
            "pass",
            &old.decision.child,
            "other-selected-pass",
        )
        .unwrap()
    };
    for mode in 0..6 {
        let mut updated = refreshed_reference(&old);
        let mut body: Value =
            serde_json::from_slice(updated.decision.confirmation.as_ref().unwrap()).unwrap();
        match mode {
            0 => {
                body["records"][1]["zoneID"] =
                    json!({"zoneName":"foreign","ownerRecordName":"other"})
            }
            1 => body["records"][1]["recordName"] = json!("redirected-child"),
            2 => body["records"][1]["deleted"] = json!(true),
            3 => {
                let duplicate = body["records"][1].clone();
                body["records"].as_array_mut().unwrap().push(duplicate);
            }
            _ => {}
        }
        if mode == 4 {
            assert_eq!(tasks[0].version_size.as_str(), "original");
            body["records"][0]["fields"]["resOriginalRes"]["value"]["size"] = json!(RAW.len() + 1);
        }
        updated.decision.confirmation = Some(serde_json::to_vec(&body).unwrap());
        assert!(
            db.refresh_selection_reference(
                owner(),
                root.id.clone(),
                old.clone(),
                updated,
                (
                    rows.clone(),
                    if mode == 5 {
                        vec![
                            reference_version("original"),
                            reference_version("live_original"),
                            reference_version("medium"),
                        ]
                    } else {
                        old.decision
                            .destinations
                            .iter()
                            .map(|d| reference_version(&d.version_size))
                            .collect()
                    }
                ),
                BUDGET
            )
            .await
            .is_err()
        );
        assert_eq!(
            db.selection_decision(
                owner(),
                root.id.clone(),
                "pass".into(),
                old.decision.child.clone()
            )
            .await
            .unwrap(),
            Some(old.clone())
        );
    }
    let successor = db
        .refresh_selection_reference(
            owner(),
            root.id.clone(),
            old.clone(),
            refreshed_reference(&old),
            (
                rows.clone(),
                old.decision
                    .destinations
                    .iter()
                    .map(|d| reference_version(&d.version_size))
                    .collect(),
            ),
            BUDGET,
        )
        .await
        .unwrap();
    let current = db
        .selection_decision(
            owner(),
            successor.id.clone(),
            "pass".into(),
            old.decision.child.clone(),
        )
        .await
        .unwrap()
        .unwrap();
    let mut next = current.clone();
    let mut body: Value =
        serde_json::from_slice(next.decision.confirmation.as_ref().unwrap()).unwrap();
    body["records"][0]["recordName"] = json!("second-current-master");
    body["records"][1]["fields"]["masterRef"]["value"]["recordName"] =
        json!("second-current-master");
    // Metadata-only evolution does not reopen completed media obligations.
    body["records"][1]["fields"]["captionEnc"] = json!({"type":"STRING","value":"new caption"});
    next.decision.master = Some("second-current-master".into());
    next.decision.confirmation = Some(serde_json::to_vec(&body).unwrap());
    let photo = crate::icloud::photos::current_asset(
        next.decision.confirmation.as_ref().unwrap(),
        &next.decision.child,
        next.decision.master.as_deref().unwrap(),
        &spec().zone,
    )
    .unwrap();
    let rows: Vec<_> = rows
        .into_iter()
        .map(|row| {
            let metadata = photo.metadata_arc(row.version_size);
            row.with_metadata_arc(metadata)
        })
        .collect();
    for destination in &mut next.decision.destinations {
        destination.metadata_hash = photo
            .metadata_arc(
                crate::state::VersionSizeKey::from_str(&destination.version_size).unwrap(),
            )
            .metadata_hash
            .clone()
            .unwrap();
    }
    let successor = db
        .refresh_selection_reference(
            owner(),
            successor.id.clone(),
            current,
            next,
            (
                rows,
                old.decision
                    .destinations
                    .iter()
                    .map(|d| reference_version(&d.version_size))
                    .collect(),
            ),
            BUDGET,
        )
        .await
        .unwrap();
    let conn = db
        .acquire_lock("completed receipt preserved exactly")
        .unwrap();
    let preserved = SqliteStateDb::merge_test_completed_origin(
        &conn,
        &root.id,
        "pass",
        &old.decision.child,
        "other-selected-pass",
    )
    .unwrap();
    assert_eq!(preserved, saved);
    assert_eq!(conn.query_row::<i64,_,_>("SELECT count(*) FROM provider_active_destinations WHERE generation=?1 AND verified_media=0",[successor.id],|r|r.get(0)).unwrap(),1);
    assert_eq!(conn.query_row::<i64,_,_>("SELECT count(*) FROM provider_active_destinations WHERE generation=?1 AND verified_media=1",[root.id],|r|r.get(0)).unwrap(),1);
}

#[tokio::test]
async fn changed_master_raw_mapping_preserves_completed_resource() {
    REFERENCE_RAW.scope(true, async {
        let dir=tempfile::tempdir().unwrap();
        let db=Arc::new(SqliteStateDb::open_owned(&dir.path().join("state.db"),&owner()).await.unwrap());
        let root=db.begin_selection_generation(owner(),spec(),BUDGET).await.unwrap();
        let (old,tasks,rows)=admission(&db,&root.id,&dir.path().join("media"),0,true,BUDGET).await;
        let completed=tasks.iter().find(|t|t.version_size.as_str()=="original").unwrap();
        assert_eq!(completed.checksum.as_ref(),super::OLD, "logical Original must select provider Alternative RAW");
        publish(&db,completed).await;
        db.seal_selection_generation(owner(),root.id.clone(),true,None).await.unwrap();
        let receipt={let conn=db.acquire_lock("RAW completion proof").unwrap();SqliteStateDb::merge_test_completed_origin(&conn,&root.id,"pass",&old.decision.child,"pass").unwrap()};
        let selected=old.decision.destinations.iter().map(|d| {
            let key=if d.version_size=="original" {crate::state::VersionSizeKey::Alternative} else {crate::state::VersionSizeKey::from_str(&d.version_size).unwrap()};
            (d.version_size.clone(),key,key)
        }).collect::<Vec<_>>();
        let mut changed=refreshed_reference(&old);
        let mut body:Value=serde_json::from_slice(changed.decision.confirmation.as_ref().unwrap()).unwrap();
        body["records"][0]["fields"]["resOriginalAltRes"]["value"]["size"]=json!(RAW.len()+1);
        changed.decision.confirmation=Some(serde_json::to_vec(&body).unwrap());
        assert!(db.refresh_selection_reference(owner(),root.id.clone(),old.clone(),changed,(rows.clone(),selected.clone()),BUDGET).await.is_err());
        assert_eq!(db.selection_decision(owner(),root.id.clone(),"pass".into(),old.decision.child.clone()).await.unwrap(),Some(old.clone()));
        let successor=db.refresh_selection_reference(owner(),root.id.clone(),old.clone(),refreshed_reference(&old),(rows,selected),BUDGET).await.unwrap();
        let conn=db.acquire_lock("RAW exact receipt survives").unwrap();
        assert_eq!(SqliteStateDb::merge_test_completed_origin(&conn,&root.id,"pass",&old.decision.child,"pass").unwrap(),receipt);
        assert!(conn.query_row::<i64,_,_>("SELECT count(*) FROM provider_active_destinations WHERE generation=?1 AND verified_media=0",[successor.id],|r|r.get(0)).unwrap()>0);
        assert_eq!(std::fs::read(&completed.download_path).unwrap(),RAW);
    }).await;
}

#[tokio::test]
async fn recorded_work_successor_owns_pending_destinations_without_empty_predecessor_debt() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let db = Arc::new(SqliteStateDb::open_owned(&path, &owner()).await.unwrap());
    let mut specification = spec();
    specification.profile["current_download_metadata"] = json!(1);
    let root = db
        .begin_selection_generation(owner(), specification, BUDGET)
        .await
        .unwrap();
    let (old, _, rows) = admission(&db, &root.id, &dir.path().join("media"), 0, true, BUDGET).await;
    let count = old.decision.destinations.len() as u64;
    assert_eq!(count, 2);
    db.seal_selection_generation(owner(), root.id.clone(), true, None)
        .await
        .unwrap();
    db.refresh_selection_reference(
        owner(),
        root.id.clone(),
        old.clone(),
        refreshed_reference(&old),
        (
            rows,
            old.decision
                .destinations
                .iter()
                .map(|d| reference_version(&d.version_size))
                .collect(),
        ),
        BUDGET,
    )
    .await
    .unwrap();
    {
        let conn = db.acquire_lock("empty predecessor proof").unwrap();
        assert_eq!(conn.query_row::<i64,_,_>("SELECT count(*) FROM provider_active_decisions WHERE generation=?1 AND compacted_hash IS NOT NULL", [&root.id], |r|r.get(0)).unwrap(), 1);
    }
    drop(db);
    let db = SqliteStateDb::open_owned(&path, &owner()).await.unwrap();
    let work = db.recorded_selection_work().await.unwrap();
    assert_eq!(work.invalid_roots, 0);
    assert_eq!(work.profiles.len(), 1);
    assert_eq!(work.profiles[0].pending_media, count);
    assert_eq!(work.profiles[0].deferred_decisions, 0);
    assert_eq!(work.profiles[0].verified_destinations, 0);
}

#[tokio::test]
async fn relative_publication_binds_exact_receipt_and_keeps_sibling_pending_through_rollback() {
    let dir = tempfile::tempdir_in(".").unwrap();
    let state = std::path::absolute(dir.path().join("state.db")).unwrap();
    let db = Arc::new(SqliteStateDb::open_owned(&state, &owner()).await.unwrap());
    let first = db
        .begin_selection_generation(owner(), spec(), BUDGET)
        .await
        .unwrap();
    let (_, mut first_tasks, _) =
        admission(&db, &first.id, &dir.path().join("first"), 0, false, BUDGET).await;
    let second = db
        .begin_selection_generation(owner(), spec(), BUDGET)
        .await
        .unwrap();
    let (_, second_tasks, _) = admission(
        &db,
        &second.id,
        &std::path::absolute(dir.path().join("second")).unwrap(),
        0,
        false,
        BUDGET,
    )
    .await;
    // Admission normalizes task destinations; exercise the public state
    // boundary with the equivalent natural relative spelling of that same path.
    let first_native = first_tasks[0].download_path.clone();
    first_tasks[0].download_path = first_native
        .strip_prefix(std::env::current_dir().unwrap())
        .unwrap()
        .to_path_buf();
    assert_eq!(
        std::path::absolute(&first_tasks[0].download_path).unwrap(),
        first_native
    );
    let a = &first_tasks[0];
    let b = &second_tasks[0];
    assert!(a.download_path.is_relative());
    assert!(b.download_path.is_absolute());
    for task in [a, b] {
        std::fs::create_dir_all(task.download_path.parent().unwrap()).unwrap();
        std::fs::write(&task.download_path, RAW).unwrap();
    }
    let timestamps: Vec<_> = [a, b]
        .iter()
        .map(|t| {
            std::fs::metadata(&t.download_path)
                .unwrap()
                .modified()
                .unwrap()
        })
        .collect();
    let sha = data_encoding::HEXLOWER.encode(&Sha256::digest(RAW));
    let mark = |db: Arc<SqliteStateDb>, task: crate::download::filter::DownloadTask| {
        let sha = sha.clone();
        async move {
            db.mark_verified_download(
                "PrimarySync",
                &task.asset_id,
                task.version_size.as_str(),
                &task.download_path,
                &sha,
                Some(&sha),
                false,
            )
            .await
        }
    };
    // An equivalent absolute receipt spelling alone cannot authorize the
    // relative caller. Model inconsistent evidence without changing any bytes.
    {
        let conn = db.acquire_lock("receipt spelling refusal").unwrap();
        conn.execute(
            "CREATE TEMP TABLE alternate_receipt_path(path TEXT NOT NULL)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO alternate_receipt_path VALUES(?1)",
            [std::path::absolute(&a.download_path)
                .unwrap()
                .to_str()
                .unwrap()],
        )
        .unwrap();
        conn.execute_batch("CREATE TEMP TRIGGER alter_receipt_spelling AFTER UPDATE OF source_checksum ON main.asset_metadata_paths WHEN NEW.source_checksum IS NOT NULL BEGIN UPDATE asset_metadata_paths SET local_path=(SELECT path FROM alternate_receipt_path) WHERE library=NEW.library AND id=NEW.id AND version_size=NEW.version_size AND local_path=NEW.local_path; END;").unwrap();
    }
    mark(db.clone(), a.clone()).await.unwrap();
    {
        let conn = db.acquire_lock("check refused alternate spelling").unwrap();
        assert_eq!(
            conn.query_row::<i64, _, _>(
                "SELECT sum(verified_media) FROM provider_active_destinations",
                [],
                |r| r.get(0)
            )
            .unwrap(),
            0
        );
        conn.execute_batch(
            "DROP TRIGGER alter_receipt_spelling; DROP TABLE alternate_receipt_path;",
        )
        .unwrap();
    }
    mark(db.clone(), a.clone()).await.unwrap();
    {
        let conn = db.acquire_lock("exact first destination only").unwrap();
        assert_eq!(
            conn.query_row::<i64, _, _>(
                "SELECT verified_media FROM provider_active_destinations WHERE generation=?1",
                [&first.id],
                |r| r.get(0)
            )
            .unwrap(),
            1
        );
        assert_eq!(
            conn.query_row::<i64, _, _>(
                "SELECT verified_media FROM provider_active_destinations WHERE generation=?1",
                [&second.id],
                |r| r.get(0)
            )
            .unwrap(),
            0
        );
        conn.execute_batch("CREATE TRIGGER fail_settlement BEFORE UPDATE OF verified_media ON provider_active_destinations WHEN OLD.verified_media=0 AND NEW.verified_media=1 BEGIN SELECT RAISE(ABORT,'synthetic settlement interruption'); END;").unwrap();
    }
    assert!(mark(db.clone(), b.clone()).await.is_err());
    {
        let conn = db.acquire_lock("receipt transaction rollback").unwrap();
        assert_eq!(
            conn.query_row::<i64, _, _>(
                "SELECT count(*) FROM asset_metadata_paths WHERE local_path=?1",
                [b.download_path.to_str().unwrap()],
                |r| r.get(0)
            )
            .unwrap(),
            0
        );
        assert_eq!(
            conn.query_row::<i64, _, _>(
                "SELECT sum(verified_media) FROM provider_active_destinations",
                [],
                |r| r.get(0)
            )
            .unwrap(),
            1
        );
    }
    drop(db);
    let db = Arc::new(SqliteStateDb::open_owned(&state, &owner()).await.unwrap());
    db.acquire_lock("resume settlement")
        .unwrap()
        .execute_batch("DROP TRIGGER fail_settlement")
        .unwrap();
    mark(db.clone(), b.clone()).await.unwrap();
    assert_eq!(
        db.acquire_lock("both independent destinations settled")
            .unwrap()
            .query_row::<i64, _, _>(
                "SELECT sum(verified_media) FROM provider_active_destinations",
                [],
                |r| r.get(0)
            )
            .unwrap(),
        2
    );
    drop(db);
    let reopened = SqliteStateDb::open_owned(&state, &owner()).await.unwrap();
    assert_eq!(
        reopened
            .acquire_lock("settlement survives reopen")
            .unwrap()
            .query_row::<i64, _, _>(
                "SELECT sum(verified_media) FROM provider_active_destinations",
                [],
                |r| r.get(0)
            )
            .unwrap(),
        2
    );
    for (i, task) in [a, b].iter().enumerate() {
        assert_eq!(std::fs::read(&task.download_path).unwrap(), RAW);
        assert_eq!(
            std::fs::metadata(&task.download_path)
                .unwrap()
                .modified()
                .unwrap(),
            timestamps[i]
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn native_publication_with_equal_lossy_text_settles_only_exact_destination() {
    use std::os::unix::ffi::OsStringExt;
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(
        SqliteStateDb::open_owned(&dir.path().join("state.db"), &owner())
            .await
            .unwrap(),
    );
    let mut roots = Vec::new();
    let mut tasks = Vec::new();
    for byte in [0xfe, 0xff] {
        let root = db
            .begin_selection_generation(owner(), spec(), BUDGET)
            .await
            .unwrap();
        let media = dir
            .path()
            .join(std::ffi::OsString::from_vec(vec![b'p', byte]));
        let (_, selected, _) = admission(&db, &root.id, &media, 0, false, BUDGET).await;
        roots.push(root.id);
        tasks.push(selected[0].clone());
    }
    assert_ne!(tasks[0].download_path, tasks[1].download_path);
    assert_eq!(
        tasks[0].download_path.to_string_lossy(),
        tasks[1].download_path.to_string_lossy()
    );
    publish(&db, &tasks[0]).await;
    {
        let conn = db.acquire_lock("native sibling still pending").unwrap();
        assert_eq!(
            conn.query_row::<i64, _, _>(
                "SELECT verified_media FROM provider_active_destinations WHERE generation=?1",
                [&roots[0]],
                |r| r.get(0)
            )
            .unwrap(),
            1
        );
        assert_eq!(
            conn.query_row::<i64, _, _>(
                "SELECT verified_media FROM provider_active_destinations WHERE generation=?1",
                [&roots[1]],
                |r| r.get(0)
            )
            .unwrap(),
            0
        );
    }
    publish(&db, &tasks[1]).await;
    assert_eq!(
        db.acquire_lock("both native paths separately settled")
            .unwrap()
            .query_row::<i64, _, _>(
                "SELECT sum(verified_media) FROM provider_active_destinations",
                [],
                |r| r.get(0)
            )
            .unwrap(),
        2
    );
    for task in tasks {
        assert_eq!(std::fs::read(task.download_path).unwrap(), RAW);
    }
}

#[tokio::test]
async fn explicit_refresh_reuse_is_writable_and_completion_is_bound_to_recorded_identity_and_options()
 {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(
        SqliteStateDb::open_owned(&dir.path().join("state.db"), &owner())
            .await
            .unwrap(),
    );
    let mut specification = spec();
    specification.metadata_enabled = true;
    specification.metadata_flags = 1;
    specification.profile["explicit_metadata_refresh"] = json!(1);
    let root = db
        .begin_selection_generation(owner(), specification, BUDGET)
        .await
        .unwrap();
    let (manifest, tasks, _) =
        admission(&db, &root.id, &dir.path().join("media"), 1, false, BUDGET).await;
    publish(&db, &tasks[0]).await;
    let task = &tasks[0];
    let path = task.download_path.clone();
    let sha = data_encoding::HEXLOWER.encode(&Sha256::digest(RAW));
    let size = RAW.len() as u64;

    assert!(
        db.reuse_selection_publication(
            owner(),
            root.id.clone(),
            "different-pass".into(),
            manifest.decision.child.clone(),
            SelectionPath::from_path(&path),
            (sha.clone(), size),
            authorization(
                &root.spec.scope,
                &root.id,
                &manifest.decision.child,
                1,
                true
            ),
        )
        .await
        .is_err(),
        "the recorded pass/key must bind the exception"
    );
    assert!(
        db.reuse_selection_publication(
            owner(),
            root.id.clone(),
            "pass".into(),
            manifest.decision.child.clone(),
            SelectionPath::from_path(&path.with_file_name("different.JPG")),
            (sha.clone(), size),
            authorization(
                &root.spec.scope,
                &root.id,
                &manifest.decision.child,
                1,
                true
            ),
        )
        .await
        .is_err(),
        "a different path cannot borrow the authorization"
    );

    let different_resource = data_encoding::HEXLOWER.encode(&Sha256::digest(b"different resource"));
    assert!(
        !db.reuse_selection_publication(
            owner(),
            root.id.clone(),
            "pass".into(),
            manifest.decision.child.clone(),
            SelectionPath::from_path(&path),
            (different_resource, size),
            authorization(
                &root.spec.scope,
                &root.id,
                &manifest.decision.child,
                1,
                true
            ),
        )
        .await
        .unwrap(),
        "a different local resource cannot borrow the authorization"
    );
    assert!(
        db.reuse_selection_publication(
            owner(),
            root.id.clone(),
            "pass".into(),
            manifest.decision.child.clone(),
            SelectionPath::from_path(&path),
            (sha.clone(), size),
            authorization(
                &root.spec.scope,
                &root.id,
                &manifest.decision.child,
                1,
                true
            ),
        )
        .await
        .unwrap()
    );
    assert!(db.guard_primary_writer(&path).await.is_ok());
    // A later retry whose generation still records the explicit refresh remains
    // writable even when the caller would otherwise install the marker.
    assert!(
        db.reuse_selection_publication(
            owner(),
            root.id.clone(),
            "pass".into(),
            manifest.decision.child.clone(),
            SelectionPath::from_path(&path),
            (sha.clone(), size),
            authorization(
                &root.spec.scope,
                &root.id,
                &manifest.decision.child,
                1,
                false
            ),
        )
        .await
        .unwrap()
    );
    assert!(db.guard_primary_writer(&path).await.is_ok());

    let mut pending = db
        .pending_selection_metadata(owner(), root.id.clone(), 10)
        .await
        .unwrap()
        .into_iter()
        .find(|pending| pending.asset.local_path.as_deref() == Some(path.as_path()))
        .expect("the authorized selected metadata work remains pending");
    let old_local = pending.asset.local_checksum.clone().unwrap();
    let output = data_encoding::HEXLOWER.encode(&Sha256::digest(b"rewritten metadata"));
    let output_size = b"rewritten metadata".len() as u64;
    pending.selection_receipt = db
        .record_selection_metadata_prepared(&pending, &output, output_size)
        .await
        .unwrap();
    assert!(pending.selection_receipt.is_some());

    let mut mismatched = pending.clone();
    mismatched
        .selection_receipt
        .as_mut()
        .unwrap()
        .metadata_flags = 2;
    assert!(matches!(
        db.finish_metadata_rewrite(
            &mismatched,
            MetadataRewriteQueue::Ordinary,
            Some(&output),
            Some(&old_local),
            MetadataRewriteCompletion::Ordinary,
        )
        .await,
        Err(crate::state::error::StateError::ProviderWorkConflict)
    ));
    assert!(
        db.finish_metadata_rewrite(
            &pending,
            MetadataRewriteQueue::Ordinary,
            Some(&output),
            Some(&old_local),
            MetadataRewriteCompletion::Ordinary,
        )
        .await
        .unwrap()
    );
    assert_eq!(
        db.acquire_lock("explicit refresh completed").unwrap().query_row::<i64,_,_>(
            "SELECT verified_metadata FROM provider_active_destinations WHERE generation=?1 AND pass_key='pass' AND child=?2",
            rusqlite::params![root.id, manifest.decision.child],
            |r| r.get(0),
        ).unwrap(),
        1
    );

    // A same-path generation with different metadata options cannot borrow
    // the first generation's refresh authorization.
    let cross = db
        .begin_selection_generation(owner(), spec(), BUDGET)
        .await
        .unwrap();
    let (cross_manifest, cross_tasks, _) =
        admission(&db, &cross.id, &dir.path().join("media"), 1, false, BUDGET).await;
    let cross_path = cross_tasks[0].download_path.clone();
    publish(&db, &cross_tasks[0]).await;
    let cross_sha = data_encoding::HEXLOWER.encode(&Sha256::digest(RAW));
    assert!(db.guard_primary_writer(&cross_path).await.is_ok());
    assert!(
        db.reuse_selection_publication(
            owner(),
            cross.id.clone(),
            "pass".into(),
            cross_manifest.decision.child.clone(),
            SelectionPath::from_path(&cross_path),
            (cross_sha, RAW.len() as u64),
            authorization(
                &cross.spec.scope,
                &cross.id,
                &cross_manifest.decision.child,
                1,
                true,
            ),
        )
        .await
        .unwrap()
    );
    assert!(
        db.guard_primary_writer(&cross_path).await.is_err(),
        "cross-generation option mismatch must install protection"
    );

    // A different generation with different metadata options cannot borrow
    // the refresh authorization; reuse installs protection.
    let other = db
        .begin_selection_generation(owner(), spec(), BUDGET)
        .await
        .unwrap();
    let (other_manifest, other_tasks, _) =
        admission(&db, &other.id, &dir.path().join("media"), 2, false, BUDGET).await;
    publish(&db, &other_tasks[0]).await;
    let other_path = other_tasks[0].download_path.clone();
    let other_sha = data_encoding::HEXLOWER.encode(&Sha256::digest(RAW));
    assert!(
        db.reuse_selection_publication(
            owner(),
            other.id.clone(),
            "pass".into(),
            other_manifest.decision.child.clone(),
            SelectionPath::from_path(&other_path),
            (other_sha.clone(), RAW.len() as u64),
            authorization(
                &other.spec.scope,
                &other.id,
                &other_manifest.decision.child,
                1,
                true,
            ),
        )
        .await
        .unwrap()
    );
    assert!(db.guard_primary_writer(&other_path).await.is_err());
    // An explicit refresh request does not remove or bypass an existing marker.
    let _ = db
        .reuse_selection_publication(
            owner(),
            other.id.clone(),
            "pass".into(),
            other_manifest.decision.child.clone(),
            SelectionPath::from_path(&other_path),
            (other_sha, RAW.len() as u64),
            authorization(
                &other.spec.scope,
                &other.id,
                &other_manifest.decision.child,
                0,
                true,
            ),
        )
        .await;
    assert!(db.guard_primary_writer(&other_path).await.is_err());
}

#[cfg(all(unix, target_os = "linux"))]
#[tokio::test]
async fn native_and_utf8_lossy_collision_does_not_settle_or_protect_the_sibling() {
    use std::os::unix::ffi::OsStringExt;

    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(
        SqliteStateDb::open_owned(&dir.path().join("state.db"), &owner())
            .await
            .unwrap(),
    );
    let native_root = dir
        .path()
        .join(std::ffi::OsString::from_vec(b"native-root-\xff".to_vec()));
    std::fs::create_dir(&native_root).unwrap();
    let root = db
        .begin_selection_generation(owner(), spec(), BUDGET)
        .await
        .unwrap();
    let (manifest, tasks, _) = admission(&db, &root.id, &native_root, 1, false, BUDGET).await;
    publish(&db, &tasks[0]).await;
    let native = tasks[0].download_path.clone();
    let lossy = std::path::PathBuf::from(native.to_string_lossy().as_ref());
    assert_ne!(native, lossy);
    std::fs::create_dir_all(lossy.parent().unwrap()).unwrap();
    std::fs::write(&lossy, RAW).unwrap();
    let sha = data_encoding::HEXLOWER.encode(&Sha256::digest(RAW));
    let auth = authorization(
        &root.spec.scope,
        &root.id,
        &manifest.decision.child,
        0,
        false,
    );

    assert!(
        !db.reuse_selection_publication(
            owner(),
            root.id.clone(),
            "pass".into(),
            manifest.decision.child.clone(),
            SelectionPath::from_path(&native),
            (sha.clone(), RAW.len() as u64),
            auth.clone(),
        )
        .await
        .unwrap(),
        "a colliding UTF-8 sibling must prevent native settlement"
    );
    assert!(db.guard_primary_writer(&native).await.is_ok());
    assert_eq!(std::fs::read(&native).unwrap(), RAW);
    assert_eq!(std::fs::read(&lossy).unwrap(), RAW);
    let marker: Option<String> = db
        .acquire_lock("native collision has no marker")
        .unwrap()
        .query_row(
            "SELECT preserved_path_key FROM asset_metadata_paths WHERE id=?1",
            [&manifest.state_id],
            |row| row.get(0),
        )
        .unwrap();
    assert!(marker.is_none());

    drop(db);
    let reopened = Arc::new(
        SqliteStateDb::open_owned(&dir.path().join("state.db"), &owner())
            .await
            .unwrap(),
    );
    assert!(
        !reopened
            .reuse_selection_publication(
                owner(),
                root.id.clone(),
                "pass".into(),
                manifest.decision.child.clone(),
                SelectionPath::from_path(&native),
                (sha, RAW.len() as u64),
                auth,
            )
            .await
            .unwrap(),
        "reopen must retain the local refusal"
    );
    assert!(reopened.guard_primary_writer(&native).await.is_ok());
    assert_eq!(std::fs::read(&native).unwrap(), RAW);

    // A healthy UTF-8 destination in the same generation still settles.
    let healthy_root = db_healthy_root(&reopened).await;
    let (healthy_manifest, healthy_tasks, _) =
        admission(&reopened, &healthy_root.id, dir.path(), 2, false, BUDGET).await;
    publish(&reopened, &healthy_tasks[0]).await;
    let healthy_path = healthy_tasks[0].download_path.clone();
    let healthy_sha = data_encoding::HEXLOWER.encode(&Sha256::digest(RAW));
    assert!(
        reopened
            .reuse_selection_publication(
                owner(),
                healthy_root.id.clone(),
                "pass".into(),
                healthy_manifest.decision.child.clone(),
                SelectionPath::from_path(&healthy_path),
                (healthy_sha, RAW.len() as u64),
                authorization(
                    &healthy_root.spec.scope,
                    &healthy_root.id,
                    &healthy_manifest.decision.child,
                    0,
                    false,
                ),
            )
            .await
            .unwrap()
    );
    assert!(reopened.guard_primary_writer(&healthy_path).await.is_err());
}

#[cfg(target_os = "linux")]
async fn db_healthy_root(db: &Arc<SqliteStateDb>) -> ActiveGeneration {
    db.begin_selection_generation(owner(), spec(), BUDGET)
        .await
        .unwrap()
}
