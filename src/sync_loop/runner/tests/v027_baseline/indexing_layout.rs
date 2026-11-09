//! Indexing qualification must not resume obsolete archive layout convergence.
use super::{
    BaselineSession, MEDIA, OLD_TOKEN, ZONE, baseline_owner, pair, seed_existing_upgrade_db, zone,
};
use crate::commands::{AlbumPass, PassKind};
use crate::download;
use crate::icloud::photos::{PhotoLibrary, PhotosSession};
use crate::state::SqliteStateDb;
use crate::state::db::primary_layout::{LayoutBinding, LayoutFile, LayoutFingerprint};
use crate::state::db::provider_selection::SelectionPath;
use crate::sync_cycle::{
    indexing_materialization_key, indexing_reconciliation_key, indexing_replay_key, run_cycle,
    sync_token_key,
};
use crate::sync_loop::test_support::{
    make_run_cycle_config, make_run_cycle_download_config_builder,
    make_run_cycle_library_state_with_passes, make_shared_session_for_run_cycle,
};
use base64::Engine as _;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

#[derive(Clone)]
struct IndexingView(BaselineSession);

#[async_trait::async_trait]
impl PhotosSession for IndexingView {
    async fn post(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        let request: Value = serde_json::from_str(&body)?;
        if request["query"]["recordType"] == "CheckIndexingState" {
            return Ok(
                json!({"records":[{"recordName":"index-state","recordType":"CheckIndexingState",
                "fields":{"state":{"value":if self.0.cycle==0 {"RUNNING"} else {"FINISHED"}}}}]}),
            );
        }
        if url.contains("/changes/zone?") {
            self.0.changes.fetch_add(1, Ordering::SeqCst);
            self.0
                .requests
                .lock()
                .unwrap()
                .push((url.into(), request.clone()));
            return Ok(json!({"zones":[{"zoneID":zone(),"records":[],
                "syncToken":format!("indexing-current-{}",self.0.cycle),"moreComing":false}]}));
        }
        self.0.post(url, body, headers).await
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

#[tokio::test]
async fn v027_indexing_running_finished_quiet_preserves_recorded_layout() {
    let server = crate::start_wiremock_or_skip!();
    for endpoint in ["/first", "/second"] {
        Mock::given(method("GET"))
            .and(path(endpoint))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
            .expect(1)
            .mount(&server)
            .await;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    std::fs::create_dir(&root).unwrap();
    let old = root.join("historical-edited.JPG");
    std::fs::write(&old, b"preserved historical metadata-edited bytes").unwrap();
    let bytes = std::fs::read(&old).unwrap();
    let mtime = std::fs::metadata(&old).unwrap().modified().unwrap();
    let binding = LayoutBinding {
        family: "historical-family".into(),
        library: ZONE.into(),
        source_library: ZONE.into(),
        child: "historical-child".into(),
        generation: 1,
        decision: "recorded-decision".into(),
        policy: "recorded-policy".into(),
        qualifier: "recorded-qualifier".into(),
        files: vec![LayoutFile {
            path: SelectionPath::from_path(&old),
            version: "original".into(),
            provider_checksum: "historical-provider-generation".into(),
            fingerprint: LayoutFingerprint {
                size: bytes.len() as u64,
                sha256: Sha256::digest(&bytes).into(),
                identity: crate::fs_util::file_identity(&std::fs::File::open(&old).unwrap())
                    .unwrap(),
            },
            source_checksum: None,
            sidecar: None,
            archive_original: false,
            metadata_decision: None,
        }],
    };
    let binding_bytes = serde_json::to_vec(&binding).unwrap();
    let db_path = dir.path().join("state.db");
    let mut inventory = pair("first", "first.JPG", &format!("{}/first", server.uri()), 11);
    inventory.extend(pair(
        "second",
        "second.JPG",
        &format!("{}/second", server.uri()),
        12,
    ));
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let key = sync_token_key(ZONE);
    for cycle in 0..4 {
        let db = Arc::new(
            SqliteStateDb::open_owned(&db_path, &baseline_owner())
                .await
                .unwrap(),
        );
        if cycle == 0 {
            db.acquire_lock("seed historical layout")
                .unwrap()
                .execute(
                    "INSERT INTO primary_layout_bindings VALUES(?1,?2,?2,?3,1,?4,?5,?6)",
                    rusqlite::params![
                        binding.family,
                        ZONE,
                        binding.child,
                        binding.decision,
                        binding.policy,
                        binding_bytes
                    ],
                )
                .unwrap();
        }
        let session = BaselineSession {
            database: "private",
            cycle,
            inventory: Arc::new(inventory.clone()),
            concurrent: Arc::new(Vec::new()),
            incremental_addition: Arc::new(Vec::new()),
            requests: Arc::new(Mutex::new(Vec::new())),
            inventory_queries: Arc::new(AtomicUsize::new(0)),
            inventory_starts: Arc::new(AtomicUsize::new(0)),
            boundaries: Arc::new(AtomicUsize::new(0)),
            changes: Arc::new(AtomicUsize::new(0)),
        };
        let mut library = PhotoLibrary::new(
            "https://synthetic.example/database/1/photos/production/private".into(),
            Arc::new(std::collections::HashMap::new()),
            Box::new(IndexingView(session.clone())),
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
        let base = make_run_cycle_download_config_builder(&root, db.clone());
        let builder = |mode, exclude, groups, zone| {
            let mut config = (*base(mode, exclude, groups, zone)).clone();
            config.folder_structure = "none".into();
            config.edited_naming = crate::types::EditedNaming::Primary;
            Arc::new(config)
        };
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
        assert_eq!(result.failed_count, 0, "cycle {cycle}: {result:?}");
        assert_eq!(
            result.stats.downloaded,
            if cycle == 0 { 2 } else { 0 },
            "cycle {cycle}: {result:?}"
        );
        assert_eq!(std::fs::read(&old).unwrap(), bytes);
        assert_eq!(std::fs::metadata(&old).unwrap().modified().unwrap(), mtime);
        {
            let conn = db.acquire_lock("unchanged historical layout").unwrap();
            let saved: Vec<u8> = conn
                .query_row(
                    "SELECT binding FROM primary_layout_bindings WHERE family=?1",
                    [&binding.family],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(saved, binding_bytes);
            let operations: i64 = conn
                .query_row("SELECT count(*) FROM primary_layout_operations", [], |r| {
                    r.get(0)
                })
                .unwrap();
            assert_eq!(operations, 0, "ordinary indexing must not plan convergence");
        }
        assert_eq!(
            db.get_metadata(&indexing_materialization_key(ZONE))
                .await
                .unwrap(),
            None
        );
        if cycle < 2 {
            assert!(
                !result.db_sync_token_advance_safe,
                "unqualified inventory cannot save token"
            );
            assert_eq!(db.get_metadata(&key).await.unwrap(), None);
            assert_eq!(session.changes.load(Ordering::SeqCst), 0);
        } else {
            assert!(
                result.db_sync_token_advance_safe,
                "cycle {cycle}: {result:?}"
            );
            assert_eq!(
                db.get_metadata(&key).await.unwrap(),
                Some(format!("indexing-current-{cycle}"))
            );
            assert_eq!(
                db.get_metadata(&indexing_reconciliation_key(ZONE))
                    .await
                    .unwrap(),
                None
            );
        }
        if cycle == 3 {
            assert_eq!(
                session.inventory_queries.load(Ordering::SeqCst),
                0,
                "quiet cycle must be incremental"
            );
            assert_eq!(session.boundaries.load(Ordering::SeqCst), 0);
        }
    }
}

#[derive(Clone)]
struct PendingUpgradeIndexingView {
    session: BaselineSession,
    indexing_running: bool,
}

#[async_trait::async_trait]
impl PhotosSession for PendingUpgradeIndexingView {
    async fn post(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        let request: Value = serde_json::from_str(&body)?;
        if request["query"]["recordType"] == "CheckIndexingState" {
            return Ok(
                json!({"records":[{"recordName":"index-state","recordType":"CheckIndexingState",
                "fields":{"state":{"value":if self.indexing_running {"RUNNING"} else {"FINISHED"}}}}]}),
            );
        }
        self.session.post(url, body, headers).await
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

/// `cycle` selects the provider phase script in `BaselineSession`, not the
/// process wake count: phase 0 is pre-baseline (fresh boundary), phase 1 is the
/// post-baseline token chain.
fn pending_upgrade_session(cycle: usize, inventory: Arc<Vec<Value>>) -> BaselineSession {
    BaselineSession {
        database: "private",
        cycle,
        inventory,
        concurrent: Arc::new(Vec::new()),
        incremental_addition: Arc::new(Vec::new()),
        requests: Arc::new(Mutex::new(Vec::new())),
        inventory_queries: Arc::new(AtomicUsize::new(0)),
        inventory_starts: Arc::new(AtomicUsize::new(0)),
        boundaries: Arc::new(AtomicUsize::new(0)),
        changes: Arc::new(AtomicUsize::new(0)),
    }
}

async fn run_pending_upgrade_indexing_cycle(
    db: Arc<SqliteStateDb>,
    session: BaselineSession,
    indexing_running: bool,
    root: &std::path::Path,
    shared: &crate::auth::SharedSession,
) -> crate::sync_cycle::CycleResult {
    try_run_pending_upgrade_indexing_cycle(db, session, indexing_running, root, shared)
        .await
        .unwrap()
}

async fn try_run_pending_upgrade_indexing_cycle(
    db: Arc<SqliteStateDb>,
    session: BaselineSession,
    indexing_running: bool,
    root: &std::path::Path,
    shared: &crate::auth::SharedSession,
) -> anyhow::Result<crate::sync_cycle::CycleResult> {
    let mut library = PhotoLibrary::new(
        "https://synthetic.example/database/1/photos/production/private".into(),
        Arc::new(std::collections::HashMap::new()),
        Box::new(PendingUpgradeIndexingView {
            session,
            indexing_running,
        }),
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
    let base = make_run_cycle_download_config_builder(root, db.clone());
    let builder = |mode, exclude, groups, zone| {
        let mut config = (*base(mode, exclude, groups, zone)).clone();
        config.folder_structure = "none".into();
        config.edited_naming = crate::types::EditedNaming::Primary;
        Arc::new(config)
    };
    run_cycle(
        &[&selected],
        &make_run_cycle_config(),
        Some(db.as_ref()),
        false,
        &builder,
        download::DownloadControls::download_hidden(),
        shared,
        &CancellationToken::new(),
    )
    .await
}

fn assert_released_catalog_preserved(
    db: &SqliteStateDb,
    path: &str,
    provider_checksum: &str,
    size: i64,
) {
    let conn = db
        .acquire_lock("inspect preserved released catalog row")
        .unwrap();
    let (status, local_path, checksum, stored_size, metadata_paths): (
        String,
        Option<String>,
        String,
        i64,
        i64,
    ) = conn
        .query_row(
            "SELECT a.status,a.local_path,a.checksum,a.size_bytes, \
             (SELECT count(*) FROM asset_metadata_paths p \
              WHERE p.library=a.library AND p.id=a.id AND p.version_size=a.version_size \
              AND p.local_path=a.local_path) \
             FROM assets a WHERE a.library=?1 AND a.id='historical-asset' \
             AND a.version_size='original'",
            [ZONE],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(status, "downloaded");
    assert_eq!(local_path.as_deref(), Some(path));
    assert_eq!(checksum, provider_checksum);
    assert_eq!(stored_size, size);
    assert_eq!(metadata_paths, 1);
}

/// Faithful stock schema-36 upgrade: the owned open migrates as
/// `existing_upgrade`, the first wake observes RUNNING and persists the
/// indexing hold, the reopened second wake observes FINISHED. Current
/// production defers before a full inventory and leaves the hold in place, so
/// the library can never reach its ordinary epoch baseline.
#[tokio::test]
async fn v027_pending_existing_upgrade_running_then_finished_qualifies_then_baselines() {
    let server = crate::start_wiremock_or_skip!();
    Mock::given(method("GET"))
        .and(path("/current"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    std::fs::create_dir(&root).unwrap();
    let old = root.join("historical-edited.JPG");
    std::fs::write(&old, b"preserved pending-upgrade historical bytes").unwrap();
    let old_bytes = std::fs::read(&old).unwrap();
    let old_mtime = std::fs::metadata(&old).unwrap().modified().unwrap();
    let old_path = old.to_str().unwrap().to_owned();
    let provider_checksum =
        base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&old_bytes));
    let local_checksum = format!("{:x}", Sha256::digest(&old_bytes));
    let db_path = dir.path().join("state.db");
    seed_existing_upgrade_db(&db_path);
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        // Released schema 26/36 stores the active cursor under the legacy
        // `sync_token:<zone>` key. It is the cursor the upgrade must clear and
        // must never send as a post-upgrade provider token.
        conn.execute(
            "INSERT OR REPLACE INTO metadata(key,value) VALUES(?1,?2)",
            rusqlite::params![format!("sync_token:{ZONE}"), OLD_TOKEN],
        )
        .unwrap();
        // A populated released catalog row binds the preserved bytes by its
        // actual path, downloaded status, provider checksum and size.
        conn.execute(
            "INSERT INTO assets(library,id,version_size,checksum,filename,created_at,size_bytes,media_type,status,local_path,last_seen_at,local_checksum,download_checksum) \
             VALUES(?1,'historical-asset','original',?2,'historical-edited.JPG',1700000000,?3,'photo','downloaded',?4,1700000000,?5,?5)",
            rusqlite::params![
                ZONE,
                &provider_checksum,
                old_bytes.len() as i64,
                &old_path,
                &local_checksum
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO asset_metadata_paths(library,id,version_size,local_path,provider_checksum,local_checksum,download_checksum) \
             VALUES(?1,'historical-asset','original',?2,?3,?4,?4)",
            rusqlite::params![ZONE, &old_path, &provider_checksum, &local_checksum],
        )
        .unwrap();
    }
    let inventory = Arc::new(pair(
        "current",
        "current.JPG",
        &format!("{}/current", server.uri()),
        21,
    ));
    let key = sync_token_key(ZONE);
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;

    // First wake: the provider is RUNNING. Current production persists the
    // hold, must not enumerate, and must not stage or activate a cursor.
    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    {
        let conn = db.acquire_lock("inspect pending upgrade origin").unwrap();
        let epoch = crate::state::db::sync_epoch::validate(&conn).unwrap();
        assert_eq!(epoch.origin, "existing_upgrade");
    }
    let session_running = pending_upgrade_session(0, Arc::clone(&inventory));
    let running_counters = session_running.clone();
    let running =
        run_pending_upgrade_indexing_cycle(Arc::clone(&db), session_running, true, &root, &shared)
            .await;
    let record_running = {
        let conn = db.acquire_lock("inspect RUNNING epoch record").unwrap();
        crate::state::db::sync_epoch::read_library(&conn, ZONE)
            .unwrap()
            .unwrap()
    };
    assert_eq!(running.stats.downloaded, 0, "{running:?}");
    assert_eq!(running_counters.inventory_queries.load(Ordering::SeqCst), 0);
    assert_eq!(running_counters.boundaries.load(Ordering::SeqCst), 0);
    assert_eq!(record_running.state, "pending");
    assert_eq!(record_running.reason, "existing_upgrade");
    assert!(record_running.binding.is_none());
    assert!(
        db.get_metadata(&indexing_reconciliation_key(ZONE))
            .await
            .unwrap()
            .is_some(),
        "RUNNING must persist the indexing hold"
    );
    assert_eq!(db.get_metadata(&key).await.unwrap(), None);
    assert_eq!(
        db.get_metadata(&format!("sync_token:{ZONE}"))
            .await
            .unwrap(),
        None,
        "the released legacy cursor key must be cleared by enrollment"
    );
    assert_released_catalog_preserved(&db, &old_path, &provider_checksum, old_bytes.len() as i64);
    assert_eq!(std::fs::read(&old).unwrap(), old_bytes);
    drop(db);

    // Reopened second wake: FINISHED with the persisted hold. The qualified
    // provisional inventory must run, download current same-source media, and
    // atomically replace the hold with replay debt without adopting a cursor
    // or rotating the epoch attempt.
    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    let session_finished = pending_upgrade_session(1, Arc::clone(&inventory));
    let finished_counters = session_finished.clone();
    let finished = run_pending_upgrade_indexing_cycle(
        Arc::clone(&db),
        session_finished,
        false,
        &root,
        &shared,
    )
    .await;
    let record_finished = {
        let conn = db.acquire_lock("inspect FINISHED epoch record").unwrap();
        crate::state::db::sync_epoch::read_library(&conn, ZONE)
            .unwrap()
            .unwrap()
    };
    let hold = db
        .get_metadata(&indexing_reconciliation_key(ZONE))
        .await
        .unwrap();
    let replay = db.get_metadata(&indexing_replay_key(ZONE)).await.unwrap();
    let token = db.get_metadata(&key).await.unwrap();
    let inventory_queries = finished_counters.inventory_queries.load(Ordering::SeqCst);
    eprintln!(
        "FINISHED pending upgrade: inventory_queries={inventory_queries} boundaries={} hold={hold:?} replay={replay:?} token={token:?} record={record_finished:?}",
        finished_counters.boundaries.load(Ordering::SeqCst),
    );
    assert!(
        inventory_queries > 0,
        "a FINISHED pending ExistingUpgrade with a persisted indexing hold must run the \
         qualification inventory; current production deferred before inventory \
         (hold={hold:?}, record={record_finished:?})"
    );
    assert_eq!(finished.stats.downloaded, 1, "{finished:?}");
    assert_eq!(
        hold, None,
        "qualification must remove only the indexing hold"
    );
    assert_eq!(replay, Some("1".to_owned()));
    assert_eq!(token, None, "qualification must not adopt a cursor");
    assert_eq!(record_finished.state, "pending");
    assert!(record_finished.binding.is_none());
    assert_eq!(record_finished.attempt, record_running.attempt);
    assert_eq!(record_finished.attempts, record_running.attempts);
    let now = chrono::Utc::now().timestamp();
    assert!(
        record_finished.next_attempt_at <= now,
        "successful qualification must restore the previous already-due deadline: {record_finished:?}"
    );
    assert_released_catalog_preserved(&db, &old_path, &provider_checksum, old_bytes.len() as i64);
    assert_eq!(std::fs::read(&old).unwrap(), old_bytes);
    assert_eq!(
        std::fs::metadata(&old).unwrap().modified().unwrap(),
        old_mtime
    );
    drop(db);

    // Third wake: ordinary epoch baseline from a fresh boundary, then a quiet
    // incremental wake. The released legacy cursor is never requested and the
    // preserved catalog row survives, while the media GET happens exactly once.
    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    let session_baseline = pending_upgrade_session(0, Arc::clone(&inventory));
    let baseline_counters = session_baseline.clone();
    let baseline = run_pending_upgrade_indexing_cycle(
        Arc::clone(&db),
        session_baseline,
        false,
        &root,
        &shared,
    )
    .await;
    let (record_baseline, active_token) = {
        let conn = db.acquire_lock("inspect completed epoch").unwrap();
        let record = crate::state::db::sync_epoch::read_library(&conn, ZONE)
            .unwrap()
            .unwrap();
        let token: Option<String> = conn
            .query_row("SELECT value FROM metadata WHERE key=?1", [&key], |row| {
                row.get(0)
            })
            .ok();
        (record, token)
    };
    eprintln!(
        "ORDINARY baseline after qualification: inventory_queries={} boundaries={} result={baseline:?} record={record_baseline:?}",
        baseline_counters.inventory_queries.load(Ordering::SeqCst),
        baseline_counters.boundaries.load(Ordering::SeqCst),
    );
    assert_eq!(baseline_counters.boundaries.load(Ordering::SeqCst), 1);
    assert!(baseline_counters.inventory_queries.load(Ordering::SeqCst) > 0);
    assert_eq!(record_baseline.state, "complete");
    assert!(record_baseline.binding.is_none());
    assert!(active_token.is_some());
    assert_ne!(active_token.as_deref(), Some(OLD_TOKEN));
    assert_released_catalog_preserved(&db, &old_path, &provider_checksum, old_bytes.len() as i64);
    assert_eq!(
        db.get_metadata(&indexing_reconciliation_key(ZONE))
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        db.get_metadata(&indexing_replay_key(ZONE)).await.unwrap(),
        None
    );
    assert_eq!(std::fs::read(&old).unwrap(), old_bytes);
    drop(db);

    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    let session_quiet = pending_upgrade_session(1, Arc::clone(&inventory));
    let quiet_counters = session_quiet.clone();
    let quiet =
        run_pending_upgrade_indexing_cycle(Arc::clone(&db), session_quiet, false, &root, &shared)
            .await;
    assert_eq!(quiet.stats.downloaded, 0, "{quiet:?}");
    assert_eq!(quiet_counters.inventory_queries.load(Ordering::SeqCst), 0);
    assert_eq!(quiet_counters.boundaries.load(Ordering::SeqCst), 0);
    assert_released_catalog_preserved(&db, &old_path, &provider_checksum, old_bytes.len() as i64);
    assert_eq!(std::fs::read(&old).unwrap(), old_bytes);
}

/// Current strict reader/gate control for the exact proposed qualification
/// outcome. This is separate from the faithful stock upgrade regression and
/// uses an in-memory enrolled record only. It proves the new reader defers
/// before the deadline and blocks ordinary claim at the deadline.
#[test]
fn pending_upgrade_tagged_outcome_roundtrips_through_strict_reader_and_gate() {
    use crate::state::SyncEpochGate;
    use crate::state::db::sync_epoch::{
        EpochOrigin, backoff, enroll_library, establish_epoch, gate, has_pending_work, library_key,
        read_library,
    };
    use rusqlite::{Connection, params};

    #[derive(serde::Serialize)]
    struct QualificationOutcome<'a> {
        version: u8,
        nonce: &'a str,
        prior_next_attempt_at: i64,
    }

    fn qualification_outcome(nonce: &str, prior_next_attempt_at: i64) -> String {
        let payload = QualificationOutcome {
            version: 1,
            nonce,
            prior_next_attempt_at,
        };
        format!(
            "qualification_in_progress:v1:{}",
            serde_json::to_string(&payload).unwrap()
        )
    }

    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE metadata(key TEXT PRIMARY KEY, value TEXT NOT NULL);
         CREATE TABLE assets(library TEXT NOT NULL, id TEXT NOT NULL, PRIMARY KEY(library,id));",
    )
    .unwrap();
    conn.execute_batch(crate::state::db::source_revalidation::DDL)
        .unwrap();
    establish_epoch(&conn, EpochOrigin::ExistingUpgrade).unwrap();
    conn.execute(
        "INSERT INTO assets(library,id) VALUES(?1,'historical')",
        [ZONE],
    )
    .unwrap();
    enroll_library(&conn, ZONE, "scope").unwrap();

    let before = read_library(&conn, ZONE).unwrap().unwrap();
    assert_eq!(before.state, "pending");
    assert_eq!(before.reason, "existing_upgrade");
    assert!(before.binding.is_none());

    let nonce = "5154f8f1-7e3b-4c5a-9f2d-6a1b0c9d8e7f";
    let prior_deadline = 1_700_000_000;
    let reserved_deadline = 1_800_000_000;
    let outcome = qualification_outcome(nonce, prior_deadline);
    let payload = outcome
        .strip_prefix("qualification_in_progress:v1:")
        .unwrap();
    let parsed: Value = serde_json::from_str(payload).unwrap();
    assert_eq!(parsed.as_object().unwrap().len(), 3);
    assert!(!outcome.contains("prior_last_outcome"));
    for n in 0..64 {
        let next_nonce = format!("00000000-0000-4000-8000-{n:012}");
        let next = qualification_outcome(&next_nonce, prior_deadline);
        assert!(
            next.len() <= 160,
            "repeated reservations must keep the durable outcome bounded"
        );
        let next_payload = next.strip_prefix("qualification_in_progress:v1:").unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(next_payload)
                .unwrap()
                .as_object()
                .unwrap()
                .len(),
            3
        );
    }
    for prior in [i64::MIN, -1, 0, 1, i64::MAX] {
        let next = qualification_outcome(nonce, prior);
        assert!(
            next.len() <= 160,
            "serialized outcome must stay bounded across i64 widths"
        );
        let next_payload = next.strip_prefix("qualification_in_progress:v1:").unwrap();
        let parsed: Value = serde_json::from_str(next_payload).unwrap();
        assert_eq!(parsed.as_object().unwrap().len(), 3);
        assert_eq!(parsed["prior_next_attempt_at"], json!(prior));
    }

    let mut reserved = before.clone();
    reserved.next_attempt_at = reserved_deadline;
    reserved.last_outcome = Some(outcome.clone());
    conn.execute(
        "UPDATE metadata SET value=?1 WHERE key=?2",
        params![serde_json::to_string(&reserved).unwrap(), library_key(ZONE)],
    )
    .unwrap();
    conn.execute(
        "INSERT OR REPLACE INTO metadata(key,value) VALUES(?1,'1')",
        [indexing_reconciliation_key(ZONE)],
    )
    .unwrap();

    let decoded = read_library(&conn, ZONE).unwrap().unwrap();
    assert_eq!(decoded.version, reserved.version);
    assert_eq!(decoded.epoch, reserved.epoch);
    assert_eq!(decoded.library, ZONE);
    assert_eq!(decoded.state, "pending");
    assert_eq!(decoded.reason, reserved.reason);
    assert_eq!(decoded.attempt, before.attempt);
    assert_eq!(decoded.attempts, before.attempts);
    assert_eq!(decoded.next_attempt_at, reserved_deadline);
    assert_eq!(decoded.last_outcome.as_deref(), Some(outcome.as_str()));
    assert!(decoded.binding.is_none());

    let stored_before: String = conn
        .query_row(
            "SELECT value FROM metadata WHERE key=?1",
            [library_key(ZONE)],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        gate(&conn, ZONE, "scope", prior_deadline, None).unwrap(),
        SyncEpochGate::Defer {
            reason: "epoch_backoff"
        }
    );
    let stored_after: String = conn
        .query_row(
            "SELECT value FROM metadata WHERE key=?1",
            [library_key(ZONE)],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stored_after, stored_before);
    assert!(backoff(&conn, ZONE, prior_deadline).unwrap());
    assert!(has_pending_work(&conn, ZONE).unwrap());

    let stored_at_deadline: String = conn
        .query_row(
            "SELECT value FROM metadata WHERE key=?1",
            [library_key(ZONE)],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        gate(&conn, ZONE, "scope", reserved_deadline, None).unwrap(),
        SyncEpochGate::Blocked {
            reason: "qualification_reservation_pending"
        }
    );
    let stored_after_deadline: String = conn
        .query_row(
            "SELECT value FROM metadata WHERE key=?1",
            [library_key(ZONE)],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stored_after_deadline, stored_at_deadline);
    let hold: Option<String> = conn
        .query_row(
            "SELECT value FROM metadata WHERE key=?1",
            [indexing_reconciliation_key(ZONE)],
            |row| row.get(0),
        )
        .ok();
    let replay: Option<String> = conn
        .query_row(
            "SELECT value FROM metadata WHERE key=?1",
            [indexing_replay_key(ZONE)],
            |row| row.get(0),
        )
        .ok();
    assert_eq!(hold.as_deref(), Some("1"));
    assert_eq!(replay, None);
}

/// Baseline v2 decoder/gate control retained separately from production.
///
/// This test does not call the new strict reader. It copies the released v2
/// record shape and the baseline gate behavior so the old-reader compatibility
/// property remains executable after the production reader becomes strict. It
/// is not upgrade setup.
#[test]
fn pending_upgrade_tagged_outcome_roundtrips_through_baseline_v2_decoder_and_gate() {
    #[derive(Debug, serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct BaselineV2Record {
        version: u8,
        epoch: String,
        library: String,
        state: String,
        reason: String,
        attempt: String,
        attempts: u32,
        next_attempt_at: i64,
        last_outcome: Option<String>,
        baseline_scope: String,
        enrolled_at: i64,
        completed_at: Option<i64>,
        #[serde(default)]
        binding: Option<Value>,
    }

    #[derive(serde::Serialize)]
    struct QualificationOutcome<'a> {
        version: u8,
        nonce: &'a str,
        prior_next_attempt_at: i64,
    }

    fn qualification_outcome(nonce: &str, prior_next_attempt_at: i64) -> String {
        let payload = QualificationOutcome {
            version: 1,
            nonce,
            prior_next_attempt_at,
        };
        format!(
            "qualification_in_progress:v1:{}",
            serde_json::to_string(&payload).unwrap()
        )
    }

    fn baseline_gate(record: &BaselineV2Record, now: i64) -> &'static str {
        if record.state != "pending" || record.binding.is_some() {
            return "not_pending";
        }
        if now < record.next_attempt_at {
            "defer"
        } else {
            "baseline_claim"
        }
    }

    let nonce = "5154f8f1-7e3b-4c5a-9f2d-6a1b0c9d8e7f";
    let prior_deadline = 1_700_000_000;
    let reserved_deadline = 1_800_000_000;
    let outcome = qualification_outcome(nonce, prior_deadline);
    let record = json!({
        "version": 2,
        "epoch": "epoch-id",
        "library": ZONE,
        "state": "pending",
        "reason": "existing_upgrade",
        "attempt": "attempt-id",
        "attempts": 0,
        "next_attempt_at": reserved_deadline,
        "last_outcome": outcome,
        "baseline_scope": "scope",
        "enrolled_at": prior_deadline,
        "completed_at": null
    });
    let decoded: BaselineV2Record = serde_json::from_value(record).unwrap();
    assert_eq!(decoded.version, 2);
    assert_eq!(decoded.epoch, "epoch-id");
    assert_eq!(decoded.library, ZONE);
    assert_eq!(decoded.state, "pending");
    assert_eq!(decoded.reason, "existing_upgrade");
    assert_eq!(decoded.attempt, "attempt-id");
    assert_eq!(decoded.attempts, 0);
    assert_eq!(decoded.next_attempt_at, reserved_deadline);
    assert_eq!(decoded.last_outcome.as_deref(), Some(outcome.as_str()));
    assert_eq!(decoded.baseline_scope, "scope");
    assert_eq!(decoded.enrolled_at, prior_deadline);
    assert!(decoded.completed_at.is_none());
    assert!(decoded.binding.is_none());

    assert_eq!(baseline_gate(&decoded, prior_deadline), "defer");
    assert_eq!(baseline_gate(&decoded, reserved_deadline), "baseline_claim");
    assert_eq!(decoded.last_outcome.as_deref(), Some(outcome.as_str()));
    assert!(decoded.binding.is_none());

    for prior in [i64::MIN, -1, 0, 1, i64::MAX] {
        let next = qualification_outcome(nonce, prior);
        assert!(next.len() <= 160);
        let payload = next.strip_prefix("qualification_in_progress:v1:").unwrap();
        let parsed: Value = serde_json::from_str(payload).unwrap();
        assert_eq!(parsed.as_object().unwrap().len(), 3);
        assert_eq!(parsed["prior_next_attempt_at"], json!(prior));
    }
}

async fn seed_running_pending_upgrade(
    media_url: &str,
) -> (
    tempfile::TempDir,
    std::path::PathBuf,
    std::path::PathBuf,
    Arc<Vec<Value>>,
    crate::auth::session::SharedSession,
) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    std::fs::create_dir(&root).unwrap();
    let db_path = dir.path().join("state.db");
    seed_existing_upgrade_db(&db_path);
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO metadata(key,value) VALUES(?1,?2)",
            rusqlite::params![format!("sync_token:{ZONE}"), OLD_TOKEN],
        )
        .unwrap();
    }
    let inventory = Arc::new(pair("current", "current.JPG", media_url, 21));
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    let session = pending_upgrade_session(0, Arc::clone(&inventory));
    let running =
        run_pending_upgrade_indexing_cycle(Arc::clone(&db), session, true, &root, &shared).await;
    assert_eq!(running.stats.downloaded, 0, "{running:?}");
    assert!(
        db.get_metadata(&indexing_reconciliation_key(ZONE))
            .await
            .unwrap()
            .is_some()
    );
    drop(db);
    (dir, root, db_path, inventory, shared)
}

#[tokio::test]
async fn pending_upgrade_qualification_write_fault_surfaces_and_keeps_guard() {
    let server = crate::start_wiremock_or_skip!();
    Mock::given(method("GET"))
        .and(path("/current"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
        .expect(1)
        .mount(&server)
        .await;
    let (_dir, root, db_path, inventory, shared) =
        seed_running_pending_upgrade(&format!("{}/current", server.uri())).await;
    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    {
        let conn = db
            .acquire_lock("inject qualification commit fault")
            .unwrap();
        conn.execute_batch(&format!(
            "CREATE TRIGGER fail_qualification_commit BEFORE UPDATE ON metadata \
             WHEN NEW.key='{}' AND json_valid(NEW.value) \
             AND json_extract(NEW.value,'$.last_outcome')='indexing_inventory_qualified' \
             BEGIN SELECT RAISE(ABORT,'injected qualification commit fault'); END;",
            crate::state::db::sync_epoch::library_key(ZONE)
        ))
        .unwrap();
    }
    let running_record = {
        let conn = db.acquire_lock("read running record").unwrap();
        crate::state::db::sync_epoch::read_library(&conn, ZONE)
            .unwrap()
            .unwrap()
    };
    let session = pending_upgrade_session(1, inventory);
    let counters = session.clone();
    let failed =
        run_pending_upgrade_indexing_cycle(Arc::clone(&db), session, false, &root, &shared).await;
    assert!(counters.inventory_queries.load(Ordering::SeqCst) > 0);
    assert!(failed.failed_count > 0, "{failed:?}");
    assert!(failed.stats.state_write_failures > 0, "{failed:?}");
    assert!(
        db.get_metadata(&indexing_reconciliation_key(ZONE))
            .await
            .unwrap()
            .is_some(),
        "failed qualification commit keeps its durable hold"
    );
    assert_eq!(
        db.get_metadata(&indexing_replay_key(ZONE)).await.unwrap(),
        None
    );
    assert_eq!(db.get_metadata(&sync_token_key(ZONE)).await.unwrap(), None);
    let after = {
        let conn = db
            .acquire_lock("read retained qualification guard")
            .unwrap();
        let raw: String = conn
            .query_row(
                "SELECT value FROM metadata WHERE key=?1",
                [crate::state::db::sync_epoch::library_key(ZONE)],
                |row| row.get(0),
            )
            .unwrap();
        (
            raw,
            crate::state::db::sync_epoch::read_library(&conn, ZONE)
                .unwrap()
                .unwrap(),
        )
    };
    assert!(
        after.0.contains("qualification_in_progress:v1:"),
        "write fault must leave the committed reservation guard: {}",
        after.0
    );
    assert_eq!(after.1.state, "pending");
    assert_eq!(after.1.attempt, running_record.attempt);
    assert_eq!(after.1.attempts, running_record.attempts);
    assert!(after.1.binding.is_none());
}

#[tokio::test]
async fn pending_upgrade_qualification_not_due_keeps_existing_deadline_and_no_work() {
    let (_dir, root, db_path, inventory, shared) =
        seed_running_pending_upgrade("http://synthetic.invalid/current").await;
    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    let future = chrono::Utc::now().timestamp() + 3_600;
    {
        let conn = db
            .acquire_lock("seed future qualification deadline")
            .unwrap();
        conn.execute(
            "UPDATE metadata SET value=json_set(value,'$.next_attempt_at',?1) WHERE key=?2",
            rusqlite::params![future, crate::state::db::sync_epoch::library_key(ZONE)],
        )
        .unwrap();
    }
    let session = pending_upgrade_session(1, inventory);
    let counters = session.clone();
    let held =
        run_pending_upgrade_indexing_cycle(Arc::clone(&db), session, false, &root, &shared).await;
    assert_eq!(counters.inventory_queries.load(Ordering::SeqCst), 0);
    assert!(
        held.holds
            .iter()
            .any(|hold| hold.reason == "indexing_qualification_not_due"),
        "{held:?}"
    );
    let record = {
        let conn = db.acquire_lock("read retained future deadline").unwrap();
        crate::state::db::sync_epoch::read_library(&conn, ZONE)
            .unwrap()
            .unwrap()
    };
    assert_eq!(record.next_attempt_at, future);
    assert_eq!(record.state, "pending");
    assert!(record.binding.is_none());
    assert!(
        db.get_metadata(&indexing_reconciliation_key(ZONE))
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        db.get_metadata(&indexing_replay_key(ZONE)).await.unwrap(),
        None
    );
}

#[tokio::test]
async fn pending_upgrade_qualification_unknown_namespace_refuses_without_fallback() {
    let (_dir, root, db_path, inventory, shared) =
        seed_running_pending_upgrade("http://synthetic.invalid/current").await;
    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    {
        let conn = db
            .acquire_lock("seed unknown qualification namespace")
            .unwrap();
        conn.execute(
            "UPDATE metadata SET value=json_set(value,'$.last_outcome','qualification_in_progress:v1:{not-json}','$.next_attempt_at',0) WHERE key=?1",
            [crate::state::db::sync_epoch::library_key(ZONE)],
        )
        .unwrap();
    }
    let session = pending_upgrade_session(1, inventory);
    let counters = session.clone();
    let error =
        try_run_pending_upgrade_indexing_cycle(Arc::clone(&db), session, false, &root, &shared)
            .await
            .expect_err("unknown qualification namespace must refuse the cycle");
    assert_eq!(counters.inventory_queries.load(Ordering::SeqCst), 0);
    assert_eq!(counters.boundaries.load(Ordering::SeqCst), 0);
    assert!(
        error.to_string().contains("malformed") || error.to_string().contains("refusing"),
        "{error}"
    );
    drop(db);
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let raw: String = conn
        .query_row(
            "SELECT value FROM metadata WHERE key=?1",
            [crate::state::db::sync_epoch::library_key(ZONE)],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        raw.contains("qualification_in_progress:v1:{not-json}"),
        "{raw}"
    );
    let replay: Option<String> = conn
        .query_row(
            "SELECT value FROM metadata WHERE key=?1",
            [indexing_replay_key(ZONE)],
            |row| row.get(0),
        )
        .ok();
    let token: Option<String> = conn
        .query_row(
            "SELECT value FROM metadata WHERE key=?1",
            [sync_token_key(ZONE)],
            |row| row.get(0),
        )
        .ok();
    assert_eq!(replay, None);
    assert_eq!(token, None);
    let hold: Option<String> = conn
        .query_row(
            "SELECT value FROM metadata WHERE key=?1",
            [indexing_reconciliation_key(ZONE)],
            |row| row.get(0),
        )
        .ok();
    assert!(hold.is_some());
}

#[tokio::test]
async fn pending_upgrade_qualification_failed_transfer_keeps_attempt_and_floor() {
    let server = crate::start_wiremock_or_skip!();
    Mock::given(method("GET"))
        .and(path("/broken"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    let (_dir, root, db_path, inventory, shared) =
        seed_running_pending_upgrade(&format!("{}/broken", server.uri())).await;
    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    let running_record = {
        let conn = db.acquire_lock("read running record").unwrap();
        crate::state::db::sync_epoch::read_library(&conn, ZONE)
            .unwrap()
            .unwrap()
    };
    let session = pending_upgrade_session(1, inventory);
    let counters = session.clone();
    let refused =
        run_pending_upgrade_indexing_cycle(Arc::clone(&db), session, false, &root, &shared).await;
    assert!(counters.inventory_queries.load(Ordering::SeqCst) > 0);
    assert!(refused.stats.failed > 0, "{refused:?}");
    let record = {
        let conn = db
            .acquire_lock("read transfer-refused qualification record")
            .unwrap();
        crate::state::db::sync_epoch::read_library(&conn, ZONE)
            .unwrap()
            .unwrap()
    };
    assert_eq!(
        record.last_outcome.as_deref(),
        Some("qualification_incomplete:v1:failed_transfers"),
        "a local transfer failure must not become a provider attempt"
    );
    assert_eq!(record.attempts, running_record.attempts);
    assert_eq!(record.attempt, running_record.attempt);
    assert_eq!(record.state, "pending");
    assert!(record.binding.is_none());
    assert_eq!(
        db.get_metadata(&indexing_replay_key(ZONE)).await.unwrap(),
        None
    );
    assert_eq!(db.get_metadata(&sync_token_key(ZONE)).await.unwrap(), None);
    assert!(
        db.get_metadata(&indexing_reconciliation_key(ZONE))
            .await
            .unwrap()
            .is_some()
    );
}
