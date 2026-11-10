//! A current source observation is distinct from the same child's saved mapping.

use super::{MEDIA, PRIMARY, ResetCycleSession, baseline_owner, hex, pair, provider_checksum};
use crate::commands::{CollectionContext, PassScope, resolve_passes_for_scope};
use crate::download;
use crate::icloud::photos::{PhotoLibrary, PhotosSession};
use crate::selection::Selection;
use crate::state::SqliteStateDb;
use crate::sync_cycle::{run_cycle, sync_token_key};
use crate::sync_loop::test_support::{
    make_run_cycle_config, make_run_cycle_download_config_builder,
    make_run_cycle_library_state_with_passes, make_shared_session_for_run_cycle,
};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

const CHILD: &str = "asset-baseline-0";
const MASTER: &str = "baseline-0";

fn primary_zone() -> Value {
    json!({"zoneName": PRIMARY, "ownerRecordName": "_defaultOwner"})
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Baseline,
    Unresolved,
    Quiet,
    Resolved,
}

#[derive(Clone)]
struct CurrentSession {
    base: ResetCycleSession,
    phase: Phase,
    sparse: Value,
    peer: Arc<Vec<Value>>,
    resolved: Arc<Vec<Value>>,
    cursor: String,
    requests: Arc<Mutex<Vec<(String, Value)>>>,
}

#[async_trait::async_trait]
impl PhotosSession for CurrentSession {
    async fn post(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        let request: Value = serde_json::from_str(&body)?;
        assert!(url.contains("/private/"), "{url}");
        if url.contains("/records/query?") || url.contains("/records/lookup?") {
            assert_eq!(request["zoneID"], primary_zone());
        }
        if url.contains("/changes/zone?") {
            assert_eq!(request["zones"][0]["zoneID"], primary_zone());
        }
        self.requests
            .lock()
            .unwrap()
            .push((url.to_owned(), request.clone()));
        if self.phase != Phase::Baseline && url.contains("/changes/zone?") {
            assert_eq!(request["zones"][0]["syncToken"], self.cursor);
            let records = match self.phase {
                // The held cursor replays the same source and peer before the deadline.
                Phase::Unresolved | Phase::Quiet => std::iter::once(self.sparse.clone())
                    .chain(self.peer.iter().cloned())
                    .collect::<Vec<_>>(),
                Phase::Resolved => self
                    .resolved
                    .iter()
                    .cloned()
                    .chain(self.peer.iter().cloned())
                    .collect(),
                Phase::Baseline => Vec::new(),
            };
            return Ok(json!({"zones":[{"zoneID":primary_zone(),"records":records,
                "syncToken":"current-delta-after","moreComing":false}]}));
        }
        if self.phase != Phase::Baseline && url.contains("/records/lookup?") {
            let names = request["records"].as_array().unwrap();
            if self.phase != Phase::Resolved {
                assert!(
                    !names.iter().any(|name| name["recordName"] == MASTER),
                    "a saved historical mapping must not become the current lookup master"
                );
            }
            let mut available = self.base.inventory.as_ref().clone();
            if self.phase != Phase::Resolved {
                available.retain(|record| {
                    record["recordName"] != CHILD && record["recordName"] != MASTER
                });
                available.push(self.sparse.clone());
            } else {
                available.retain(|record| {
                    record["recordName"] != CHILD && record["recordName"] != MASTER
                });
                available.extend(self.resolved.iter().cloned());
            }
            available.extend(self.peer.iter().cloned());
            let records: Vec<_> = names
                .iter()
                .filter_map(|name| {
                    available
                        .iter()
                        .find(|record| record["recordName"] == name["recordName"])
                        .cloned()
                })
                .collect();
            return Ok(json!({"records":records}));
        }
        self.base.post(url, body, headers).await
    }

    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

async fn cycle(
    db: Arc<SqliteStateDb>,
    root: &std::path::Path,
    session: CurrentSession,
    shared: &crate::auth::SharedSession,
) -> crate::sync_cycle::CycleResult {
    let mut library = PhotoLibrary::new(
        "https://synthetic.example/database/1/photos/production/private".into(),
        Arc::default(),
        Box::new(session),
        Arc::new(primary_zone()),
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
    let plan = resolve_passes_for_scope(
        &library,
        &Selection::default(),
        PassScope {
            include_albums: false,
            include_smart_folders: false,
            include_unfiled: true,
        },
        &CollectionContext {
            collection_album_names: BTreeSet::new(),
            selected_smart_folder_names: Vec::new(),
        },
        &[],
    )
    .await
    .unwrap();
    let mut state =
        make_run_cycle_library_state_with_passes(PRIMARY, &sync_token_key(PRIMARY), plan.passes);
    state.library = library;
    let base = make_run_cycle_download_config_builder(root, db.clone());
    let builder = |mode, exclude, groups, zone| {
        let mut config = (*base(mode, exclude, groups, zone)).clone();
        config.folder_structure = "none".into();
        Arc::new(config)
    };
    run_cycle(
        &[&state],
        &make_run_cycle_config(),
        Some(db.as_ref()),
        false,
        &builder,
        download::DownloadControls::download_hidden(),
        shared,
        &CancellationToken::new(),
    )
    .await
    .unwrap()
}

fn historical_rows(conn: &rusqlite::Connection) -> Vec<Vec<rusqlite::types::Value>> {
    let mut rows = Vec::new();
    for sql in [
        "SELECT library,id,version_size,status,local_path,checksum,size_bytes,local_checksum,download_checksum,downloaded_at FROM assets WHERE library=?1 AND id LIKE 'asset-baseline-%' ORDER BY id,version_size",
        "SELECT library,id,version_size,local_path,provider_checksum,local_checksum,download_checksum,source_checksum FROM asset_metadata_paths WHERE library=?1 AND id LIKE 'asset-baseline-%' ORDER BY id,version_size,local_path",
        "SELECT library,asset_record_name,master_record_name FROM asset_master_mappings WHERE library=?1 AND asset_record_name LIKE 'asset-baseline-%' ORDER BY asset_record_name",
        "SELECT account_key,provider_key,scope,library,child,master,state_id,origin_pass,version_size,provider_checksum,provider_size,native_path,source_checksum,local_checksum,proof_hash FROM download_receipts WHERE library=?1 AND child LIKE 'asset-baseline-%' ORDER BY child,native_path",
    ] {
        let mut stmt = conn.prepare(sql).unwrap();
        let count = stmt.column_count();
        rows.extend(
            stmt.query_map([PRIMARY], |row| {
                (0..count)
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

#[derive(Debug, PartialEq, Eq)]
struct SparseRow {
    library: String,
    child: String,
    original: String,
    observed: String,
    lookup: String,
    generation: i64,
    origin: String,
    attempted_at: i64,
    next_retry_at: i64,
    outcome: String,
    attempts: i64,
}

fn sparse_row(conn: &rusqlite::Connection) -> SparseRow {
    let count: i64 = conn
        .query_row(
            "SELECT count(*) FROM unresolved_sparse_identities WHERE epoch=1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1, "exactly one new current identity");
    conn.query_row("SELECT library,source_record_name,original_evidence,observed_evidence,lookup_evidence,generation,observation_origin,last_attempt_at,next_retry_at,last_outcome,attempts FROM unresolved_sparse_identities WHERE epoch=1 AND library=?1 AND source_record_name=?2", rusqlite::params![PRIMARY, CHILD], |r| Ok(SparseRow {
        library: r.get(0)?, child: r.get(1)?, original: r.get(2)?, observed: r.get(3)?, lookup: r.get(4)?, generation: r.get(5)?, origin: r.get(6)?, attempted_at: r.get(7)?, next_retry_at: r.get(8)?, outcome: r.get(9)?, attempts: r.get(10)?,
    })).unwrap()
}

fn marker(conn: &rusqlite::Connection) -> Option<String> {
    conn.query_row(
        "SELECT value FROM metadata WHERE key=?1",
        [crate::state::unresolved_identity_key(PRIMARY)],
        |r| r.get(0),
    )
    .ok()
}

fn peer_receipt(conn: &rusqlite::Connection) -> String {
    conn.query_row("SELECT json_array(account_key,provider_key,scope,library,child,master,state_id,origin_pass,version_size,provider_checksum,provider_size,native_path,source_checksum,local_checksum,proof_hash) FROM download_receipts WHERE library=?1 AND child='asset-peer-current'", [PRIMARY], |r| r.get(0)).unwrap()
}

#[tokio::test]
async fn current_same_child_without_master_retains_scoped_retry_after_reopen() {
    let server = crate::start_wiremock_or_skip!();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    std::fs::create_dir(&root).unwrap();
    let db_path = dir.path().join("state.db");
    let mut records = Vec::new();
    for index in 0..6 {
        let name = format!("baseline-{index}");
        let mock = Mock::given(method("GET"))
            .and(path(format!("/{name}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA));
        mock.expect(1).mount(&server).await;
        let mut record = pair(
            &name,
            &format!("{name}.JPG"),
            &format!("{}/{name}", server.uri()),
            41 + index as u8,
        );
        record[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
            json!(provider_checksum(MEDIA));
        record[1]["fields"]["masterRef"]["value"]["zoneID"] = primary_zone();
        records.extend(record);
    }
    Mock::given(method("GET"))
        .and(path("/peer"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
        .expect(1)
        .mount(&server)
        .await;
    let mut peer = pair(
        "peer-current",
        "peer.JPG",
        &format!("{}/peer", server.uri()),
        60,
    );
    peer[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(provider_checksum(MEDIA));
    peer[1]["fields"]["masterRef"]["value"]["zoneID"] = primary_zone();
    let mut sparse = crate::test_helpers::sparse_shared_asset_record();
    // This is a Primary source record with an unselected link hint. No Shared
    // provider is selected or queried by this fixture.
    sparse["recordName"] = json!(CHILD);
    assert!(sparse["fields"].get("masterRef").is_none());
    let inventory = Arc::new(records);
    let mut resolved = inventory[..2].to_vec();
    for record in &mut resolved {
        record["recordChangeTag"] = json!("fresh-same-source-pair");
    }
    let resolved = Arc::new(resolved);
    let requests = Arc::new(Mutex::new(Vec::new()));
    let full = Arc::new(AtomicUsize::new(0));
    let make_session = |phase, cursor: String| CurrentSession {
        base: ResetCycleSession {
            zone: PRIMARY,
            inventory: Arc::clone(&inventory),
            full_queries: Arc::clone(&full),
            changes: Arc::new(AtomicUsize::new(0)),
            fail_full: false,
            auth_expired: false,
            boundary: "current-baseline-before",
        },
        phase,
        sparse: sparse.clone(),
        peer: Arc::new(peer.clone()),
        resolved: Arc::clone(&resolved),
        cursor,
        requests: Arc::clone(&requests),
    };
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    assert_eq!(
        db.get_metadata(&sync_token_key(PRIMARY)).await.unwrap(),
        None
    );
    {
        let conn = db.acquire_lock("inspect empty baseline state").unwrap();
        let receipts: i64 = conn
            .query_row("SELECT count(*) FROM download_receipts", [], |r| r.get(0))
            .unwrap();
        let sparse: i64 = conn
            .query_row(
                "SELECT count(*) FROM unresolved_sparse_identities",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!((receipts, sparse), (0, 0));
        assert_eq!(marker(&conn), None);
    }
    let baseline = cycle(
        Arc::clone(&db),
        &root,
        make_session(Phase::Baseline, String::new()),
        &shared,
    )
    .await;
    assert_eq!(baseline.failed_count, 0, "{baseline:?}");
    assert_eq!(baseline.stats.downloaded, 6, "{baseline:?}");
    let token = db
        .get_metadata(&sync_token_key(PRIMARY))
        .await
        .unwrap()
        .unwrap();
    let snapshots: Vec<_> = (0..6)
        .map(|i| {
            let path = root.join(format!("baseline-{i}.JPG"));
            assert_eq!(std::fs::read(&path).unwrap(), MEDIA);
            let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
            (path, mtime)
        })
        .collect();
    let (old_rows, epoch_before) = {
        let conn = db.acquire_lock("inspect initial current baseline").unwrap();
        let epoch: String = conn
            .query_row(
                "SELECT value FROM metadata WHERE key=?1",
                [crate::state::db::sync_epoch::library_key(PRIMARY)],
                |r| r.get(0),
            )
            .unwrap();
        let epoch_value: Value = serde_json::from_str(&epoch).unwrap();
        assert_eq!(epoch_value["state"], "complete");
        assert_eq!(epoch_value["library"], PRIMARY);
        let mapping: String = conn.query_row("SELECT master_record_name FROM asset_master_mappings WHERE library=?1 AND asset_record_name=?2", rusqlite::params![PRIMARY, CHILD], |r| r.get(0)).unwrap();
        assert_eq!(mapping, MASTER);
        let rows = historical_rows(&conn);
        for table in [
            "assets",
            "asset_metadata_paths",
            "asset_master_mappings",
            "download_receipts",
        ] {
            let (column, filter) = match table {
                "asset_master_mappings" => ("asset_record_name", "asset-baseline-%"),
                "download_receipts" => ("child", "asset-baseline-%"),
                _ => ("id", "asset-baseline-%"),
            };
            let count: i64 = conn
                .query_row(
                    &format!("SELECT count(*) FROM {table} WHERE library=?1 AND {column} LIKE ?2"),
                    rusqlite::params![PRIMARY, filter],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(count, 6, "baseline {table}");
        }
        (rows, epoch)
    };
    assert_eq!(Arc::strong_count(&db), 1);
    drop(db);
    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    let inventory_before = full.load(Ordering::SeqCst);
    let delta = cycle(
        Arc::clone(&db),
        &root,
        make_session(Phase::Unresolved, token.clone()),
        &shared,
    )
    .await;
    assert_eq!(delta.failed_count, 0, "{delta:?}");
    assert_eq!(delta.stats.downloaded, 1, "{delta:?}");
    assert!(delta.stats.identity_incomplete, "{delta:?}");
    assert!(!delta.db_sync_token_advance_safe, "{delta:?}");
    assert_eq!(std::fs::read(root.join("peer.JPG")).unwrap(), MEDIA);
    assert_eq!(full.load(Ordering::SeqCst), inventory_before);
    let current_token = db.get_metadata(&sync_token_key(PRIMARY)).await.unwrap();
    assert_eq!(current_token.as_deref(), Some(token.as_str()));
    let expected_scope = serde_json::to_string(&json!({"format":1,"realm":"com","container":"com.apple.photos.cloud","environment":"production","database":"private","zone":primary_zone()})).unwrap();
    let (sparse_before, peer_proof) = {
        let conn = db.acquire_lock("inspect current sparse delta").unwrap();
        assert_eq!(historical_rows(&conn), old_rows);
        let row = sparse_row(&conn);
        let expected_evidence = json!([
            1,
            "private-linked-child",
            "SharedSync-private-removed",
            "private-owner"
        ])
        .to_string();
        assert_eq!(row.library, PRIMARY);
        assert_eq!(row.child, CHILD);
        assert_eq!(row.original, expected_evidence);
        assert_eq!(row.observed, row.original);
        assert_eq!(row.lookup, row.original);
        assert_eq!(
            row.origin,
            json!(["current-v1", expected_scope]).to_string()
        );
        assert!(row.generation > 0);
        assert_eq!(row.outcome, "unresolved");
        assert_eq!(row.attempts, 1);
        assert_eq!(row.next_retry_at - row.attempted_at, 3600);
        assert!(row.next_retry_at > chrono::Utc::now().timestamp());
        assert_eq!(marker(&conn).as_deref(), Some("1"));
        let peer_native =
            crate::state::db::provider_selection::SelectionPath::from_path(&root.join("peer.JPG"))
                .key()
                .unwrap();
        let owner = baseline_owner();
        let (account, provider) = owner.authenticated_keys().unwrap();
        let saved_owner: (String, String) = conn
            .query_row(
                "SELECT account_key,provider_key FROM account_owner WHERE singleton=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            (saved_owner.0.as_str(), saved_owner.1.as_str()),
            (account, provider)
        );
        let count: i64 = conn.query_row("SELECT count(*) FROM download_receipts WHERE account_key=?1 AND provider_key=?2 AND scope=?3 AND library=?4 AND child='asset-peer-current' AND master='peer-current' AND state_id='asset-peer-current' AND version_size='original' AND native_path=?5 AND provider_checksum=?6 AND provider_size=?7 AND source_checksum=?8 AND local_checksum=?8", rusqlite::params![account,provider,expected_scope,PRIMARY,peer_native,provider_checksum(MEDIA),MEDIA.len() as i64,hex(MEDIA)], |r| r.get(0)).unwrap();
        assert_eq!(count, 1, "valid peer has its own scoped media receipt");
        (row, peer_receipt(&conn))
    };
    assert!(
        requests
            .lock()
            .unwrap()
            .iter()
            .any(|(url, request)| url.contains("/records/lookup?")
                && request["records"]
                    .as_array()
                    .is_some_and(|items| items.iter().any(|item| item["recordName"] == CHILD))),
        "real source lookup must execute"
    );
    for (path, mtime) in &snapshots {
        assert_eq!(std::fs::read(path).unwrap(), MEDIA);
        assert_eq!(std::fs::metadata(path).unwrap().modified().unwrap(), *mtime);
    }
    let media_requests = server.received_requests().await.unwrap();
    for path in [
        "/baseline-0",
        "/baseline-1",
        "/baseline-2",
        "/baseline-3",
        "/baseline-4",
        "/baseline-5",
        "/peer",
    ] {
        assert_eq!(
            media_requests
                .iter()
                .filter(|request| request.url.path() == path)
                .count(),
            1,
            "GET count for {path} before retry"
        );
    }
    let lookup_count = requests
        .lock()
        .unwrap()
        .iter()
        .filter(|(url, body)| {
            url.contains("/records/lookup?")
                && body["records"]
                    .as_array()
                    .is_some_and(|items| items.iter().any(|item| item["recordName"] == CHILD))
        })
        .count();
    let peer_mtime = std::fs::metadata(root.join("peer.JPG"))
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(Arc::strong_count(&db), 1);
    drop(db);
    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    let quiet_cursor = db
        .get_metadata(&sync_token_key(PRIMARY))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(quiet_cursor, token);
    assert!(
        chrono::Utc::now().timestamp() < sparse_before.next_retry_at,
        "quiet cycle must precede the real retry deadline"
    );
    let quiet = cycle(
        Arc::clone(&db),
        &root,
        make_session(Phase::Quiet, quiet_cursor),
        &shared,
    )
    .await;
    assert_eq!(quiet.failed_count, 0, "{quiet:?}");
    assert_eq!(quiet.stats.downloaded, 0, "{quiet:?}");
    assert!(quiet.stats.identity_incomplete, "{quiet:?}");
    assert!(!quiet.db_sync_token_advance_safe, "{quiet:?}");
    assert_eq!(
        db.get_metadata(&sync_token_key(PRIMARY))
            .await
            .unwrap()
            .as_deref(),
        Some(token.as_str())
    );
    assert_eq!(full.load(Ordering::SeqCst), inventory_before);
    assert_eq!(
        requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(url, body)| url.contains("/records/lookup?")
                && body["records"]
                    .as_array()
                    .is_some_and(|items| items.iter().any(|item| item["recordName"] == CHILD)))
            .count(),
        lookup_count,
        "future deadline suppresses an immediate repeated source lookup"
    );
    {
        let conn = db.acquire_lock("inspect quiet current sparse row").unwrap();
        assert_eq!(sparse_row(&conn), sparse_before);
        assert_eq!(marker(&conn).as_deref(), Some("1"));
        let epoch: String = conn
            .query_row(
                "SELECT value FROM metadata WHERE key=?1",
                [crate::state::db::sync_epoch::library_key(PRIMARY)],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(epoch, epoch_before);
        assert_eq!(historical_rows(&conn), old_rows);
        assert_eq!(peer_receipt(&conn), peer_proof);
    }
    for (path, mtime) in &snapshots {
        assert_eq!(std::fs::read(path).unwrap(), MEDIA);
        assert_eq!(std::fs::metadata(path).unwrap().modified().unwrap(), *mtime);
    }
    assert_eq!(std::fs::read(root.join("peer.JPG")).unwrap(), MEDIA);
    assert_eq!(
        std::fs::metadata(root.join("peer.JPG"))
            .unwrap()
            .modified()
            .unwrap(),
        peer_mtime
    );
    assert_eq!(Arc::strong_count(&db), 1);
    drop(db);
    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    let cursor = db
        .get_metadata(&sync_token_key(PRIMARY))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cursor, token);
    assert!(chrono::Utc::now().timestamp() < sparse_before.next_retry_at);
    let before_resolution_requests = requests.lock().unwrap().len();
    let resolved_cycle = cycle(
        Arc::clone(&db),
        &root,
        make_session(Phase::Resolved, cursor),
        &shared,
    )
    .await;
    assert_eq!(resolved_cycle.failed_count, 0, "{resolved_cycle:?}");
    assert_eq!(resolved_cycle.stats.downloaded, 0, "{resolved_cycle:?}");
    assert!(
        resolved_cycle.stats.identity_incomplete,
        "{resolved_cycle:?}"
    );
    assert!(
        !resolved_cycle.db_sync_token_advance_safe,
        "{resolved_cycle:?}"
    );
    assert_eq!(
        db.get_metadata(&sync_token_key(PRIMARY))
            .await
            .unwrap()
            .as_deref(),
        Some(token.as_str())
    );
    assert_eq!(full.load(Ordering::SeqCst), inventory_before);
    assert!(
        requests.lock().unwrap()[before_resolution_requests..]
            .iter()
            .any(|(url, request)| url.contains("/records/lookup?")
                && request["records"].as_array().is_some_and(|records| records
                    .iter()
                    .any(|record| record["recordName"] == CHILD)
                    && records.iter().any(|record| record["recordName"] == MASTER))),
        "fresh same-source pair must reach current confirmation"
    );
    {
        let conn = db
            .acquire_lock("inspect same-source paired resolution")
            .unwrap();
        assert_eq!(sparse_row(&conn), sparse_before);
        assert_eq!(marker(&conn).as_deref(), Some("1"));
        let epoch: String = conn
            .query_row(
                "SELECT value FROM metadata WHERE key=?1",
                [crate::state::db::sync_epoch::library_key(PRIMARY)],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(epoch, epoch_before);
        assert_eq!(historical_rows(&conn), old_rows);
        assert_eq!(peer_receipt(&conn), peer_proof);
    }
    for (path, mtime) in &snapshots {
        assert_eq!(std::fs::read(path).unwrap(), MEDIA);
        assert_eq!(std::fs::metadata(path).unwrap().modified().unwrap(), *mtime);
    }
    assert_eq!(Arc::strong_count(&db), 1);
    drop(db);
    let db = SqliteStateDb::open_owned(&db_path, &baseline_owner())
        .await
        .unwrap();
    assert_eq!(
        db.get_metadata(&sync_token_key(PRIMARY))
            .await
            .unwrap()
            .as_deref(),
        Some(token.as_str())
    );
    {
        let conn = db.acquire_lock("inspect reopened paired hold").unwrap();
        assert_eq!(sparse_row(&conn), sparse_before);
        assert_eq!(marker(&conn).as_deref(), Some("1"));
        assert_eq!(historical_rows(&conn), old_rows);
        assert_eq!(peer_receipt(&conn), peer_proof);
    }
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        7,
        "only the initial six media and one peer were requested"
    );
    server.verify().await;
}
