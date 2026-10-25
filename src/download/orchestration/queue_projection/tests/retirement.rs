//! Completion ownership survives body compaction and predecessor retirement.
use super::{CURRENT, Fixture, controls, destination_pass, destination_pass_with, owner, records};
use crate::download::orchestration::models::DownloadStore;
use crate::download::planner::{TaskPlanner, pending_record_for_task};
use crate::icloud::photos::inbox::ShadowCapture;
use crate::state::DownloadStateStore;
use crate::state::SqliteStateDb;
use crate::state::db::provider_generations::{
    ActiveDecision, GenerationSpec, MAX_GENERATION_BYTES,
};
use crate::state::db::provider_selection::{SelectionDecision, SelectionDestination};
use crate::state::db::provider_selection::{SelectionOutcome, SelectionPath};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio_util::sync::CancellationToken;

const RAW: &[u8] = b"raw fixture bytes";
const BUDGET: u64 = 64 * 1024;

fn spec() -> GenerationSpec {
    let zone = json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"});
    GenerationSpec {
        format:2,
        scope:serde_json::to_string(&json!({"format":1,"realm":"com","container":"com.apple.photos.cloud","environment":"production","database":"private","zone":zone})).unwrap(),
        zone:zone.clone(),config_hash:"a".repeat(64),basis:"b".repeat(64),
        profile:json!({"coverage":"selected_downloads","passes":[{"key":"pass","scope":{"database":"private","zone":zone,"kind":"unfiled"}}]}),
        metadata_enabled:false,metadata_flags:0,
    }
}
async fn admission(
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
    let master = format!("compact-{index}");
    let child = format!("asset-{master}");
    let mut values = records(&master, CURRENT, "compact fixture");
    values[0]["fields"]["resOriginalRes"]["value"]["size"] = json!(RAW.len());
    if companion {
        values[0]["fields"]["resOriginalVidComplRes"] = json!({"value":{"downloadURL":"https://p01.icloud-content.com/motion.mov","size":RAW.len(),"fileChecksum":CURRENT}});
        values[0]["fields"]["resOriginalVidComplFileType"] =
            json!({"value":"com.apple.quicktime-movie"});
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
    config.state_db = Some(db.clone() as Arc<dyn DownloadStore>);
    let mut planner = TaskPlanner::for_download(config.state_db.as_deref())
        .await
        .unwrap();
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
        let mut component = std::ffi::OsString::from(format!("copy-{copy}"));
        #[cfg(unix)]
        if copy >= 64 {
            use std::os::unix::ffi::OsStringExt;
            component = std::ffi::OsString::from_vec(vec![
                b'c',
                b'o',
                b'p',
                b'y',
                b'-',
                if copy == 64 { 0xfe } else { 0xff },
            ]);
        }
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
async fn v027_successor_cleanup_does_not_retire_another_librarys_active_root() {
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
                root.id,
                "pass".into(),
                manifest.decision.child,
                SelectionPath::from_path(destination),
                (sha, before.len() as u64),
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
            next.id,
            "pass".into(),
            manifest.decision.child,
            SelectionPath::from_path(path),
            (sha, RAW.len() as u64)
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
