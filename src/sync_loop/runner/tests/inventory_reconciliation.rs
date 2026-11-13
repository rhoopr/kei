//! Count-era upgrade requests through the ordinary shared-scope cycle owner.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use base64::Engine;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::commands::{AlbumPass, PassKind};
use crate::icloud::photos::PhotoAlbum;
use crate::icloud::photos::PhotoAlbumConfig;
use crate::icloud::photos::inbox::ShadowCapture;
use crate::icloud::photos::session::PhotosSession;
use crate::state::SqliteStateDb;
use crate::state::db::account::AccountOwner;
use crate::sync_cycle::{CycleResult, ENUM_CONFIG_HASH_KEY, run_cycle};
use crate::sync_loop::test_support::{
    album_count_response, full_album_page_with_download, make_run_cycle_config,
    make_run_cycle_download_config_builder, make_run_cycle_library_state_with_passes,
    make_shared_session_for_run_cycle,
};
use crate::{config, download, retry};

const ZONE: &str = "SharedSync-INVENTORY";
const CURSOR: &str = "sync_token:SharedSync-INVENTORY";
const REQUEST: &str = "inventory_reconciliation_required:SharedSync-INVENTORY";
const RECEIPT: &str = "recent_selection_recovery:SharedSync-INVENTORY";
const BYTES: &[u8] = b"\xff\xd8\xff\xe0independent shared inventory media";

#[derive(Clone)]
struct InventorySession {
    provider_owner: &'static str,
    records: Arc<HashMap<&'static str, Vec<Value>>>,
    displaced: Arc<AtomicBool>,
    unstable: Arc<AtomicBool>,
    unknown: bool,
    initial_changes: bool,
    unresolved_initial: bool,
    expired: Option<&'static str>,
    cancel_replay: Option<CancellationToken>,
    replay_metadata: Option<(Arc<SqliteStateDb>, &'static str, &'static str)>,
    rank_calls: Arc<AtomicUsize>,
    trace: Arc<Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl PhotosSession for InventorySession {
    async fn post(
        &self,
        url: &str,
        body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        let body: Value = serde_json::from_str(&body)?;
        if url.contains("/changes/zone?") {
            let cursor = body["zones"][0]["syncToken"]
                .as_str()
                .expect("retained cursor");
            self.trace.lock().unwrap().push(format!("delta:{cursor}"));
            if self.expired == Some(cursor) {
                return Ok(
                    json!({"zones":[{"zoneID":{"zoneName":ZONE,"ownerRecordName":self.provider_owner},
                    "serverErrorCode":"BAD_REQUEST","reason":"synthetic expired boundary",
                    "syncToken":"","moreComing":false}]}),
                );
            }
            let replay = cursor == "S";
            let raw = if cursor == "P" && self.unresolved_initial {
                vec![json!({"recordName":"unresolved-child","recordType":"CPLAsset","fields":{}})]
            } else if cursor == "P" && self.initial_changes {
                ["A", "B", "C", "D"]
                    .into_iter()
                    .flat_map(|id| self.records[id].clone())
                    .collect()
            } else if replay && self.unstable.load(Ordering::SeqCst) {
                if self.unknown {
                    vec![
                        json!({"recordName":"unknown-current", "recordType":"FutureRankMutation", "fields":{}}),
                    ]
                } else {
                    vec![json!({"recordName":"asset-A", "deleted":true})]
                }
            } else {
                Vec::new()
            };
            if replay && let Some(cancel) = &self.cancel_replay {
                cancel.cancel();
            }
            if replay && let Some((db, key, value)) = &self.replay_metadata {
                db.set_metadata(key, value).await?;
            }
            return Ok(
                json!({"zones":[{"zoneID":{"zoneName":ZONE,"ownerRecordName":self.provider_owner},
                "syncToken": if cursor == "P" { "S" } else { "E" },
                "moreComing":false,"records":raw}]}),
            );
        }
        if url.contains("/internal/records/query/batch") {
            return Ok(album_count_response(4));
        }
        if url.contains("/records/lookup?") {
            let requested: Vec<_> = body["records"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|record| record["recordName"].as_str())
                .collect();
            let records: Vec<_> = self
                .records
                .values()
                .flatten()
                .filter(|record| requested.contains(&record["recordName"].as_str().unwrap()))
                .cloned()
                .collect();
            return Ok(json!({"records":records}));
        }
        if url.contains("/records/query?") {
            self.rank_calls.fetch_add(1, Ordering::SeqCst);
            let offset = body["query"]["filterBy"]
                .as_array()
                .unwrap()
                .iter()
                .find(|filter| filter["fieldName"] == "startRank")
                .and_then(|filter| filter["fieldValue"]["value"].as_u64())
                .unwrap();
            self.trace.lock().unwrap().push(format!("rank:{offset}"));
            let count = body["resultsLimit"].as_u64().unwrap() / 2;
            if offset == 2 && self.unstable.load(Ordering::SeqCst) && !self.unknown {
                self.displaced.store(true, Ordering::SeqCst);
            }
            let ids: &[&str] = if self.displaced.load(Ordering::SeqCst) {
                &["B", "C", "D"]
            } else {
                &["A", "B", "C", "D"]
            };
            let mut records = Vec::new();
            for id in ids
                .iter()
                .skip(usize::try_from(offset).unwrap())
                .take(usize::try_from(count).unwrap())
            {
                records.extend(self.records[*id].clone());
            }
            return Ok(json!({"records":records,"syncToken":"opaque-query-token"}));
        }
        anyhow::bail!("unexpected synthetic inventory endpoint")
    }

    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

struct Fixture {
    root: tempfile::TempDir,
    path: PathBuf,
    media: PathBuf,
    owner: AccountOwner,
    config: config::Config,
    session: InventorySession,
    server: MockServer,
    receipt: Vec<u8>,
}

impl Fixture {
    async fn new(receipt: &[u8], hash: Option<&str>, unstable: bool, unknown: bool) -> Self {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("owned.db");
        let media = root.path().join("media");
        std::fs::create_dir(&media).unwrap();
        std::fs::write(media.join("retained.jpg"), b"unrelated retained media").unwrap();
        let owner = AccountOwner::authenticated(
            "inventory@example.invalid",
            "com",
            &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-inventory"}})).unwrap(),
        )
        .unwrap();
        let db = SqliteStateDb::open_owned(&path, &owner).await.unwrap();
        db.set_metadata(CURSOR, "P").await.unwrap();
        let mut config = make_run_cycle_config();
        config.filters.selection.libraries =
            crate::selection::parse_library_selector(&[ZONE.into()]).unwrap();
        if let Some(hash) = hash {
            let value = if hash == "matching" {
                download::compute_config_hash(&config)
            } else {
                hash.to_owned()
            };
            db.set_metadata(ENUM_CONFIG_HASH_KEY, &value).await.unwrap();
        }
        {
            let conn = db.acquire_lock("seed count-era upgrade").unwrap();
            conn.execute(
                "INSERT INTO metadata(key,value) VALUES(?1,?2)",
                rusqlite::params![RECEIPT, receipt],
            )
            .unwrap();
            conn.pragma_update(None, "user_version", 36).unwrap();
        }
        drop(db);
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(BYTES))
            .mount(&server)
            .await;
        let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(BYTES));
        let mut records = HashMap::new();
        for id in ["A", "B", "C", "D"] {
            let mut page = full_album_page_with_download(
                ZONE,
                id,
                "query",
                &format!("{}/{id}.jpg", server.uri()),
                BYTES.len() as u64,
                &checksum,
            );
            page["records"][0]["fields"]["filenameEnc"]["value"] = json!(format!("{id}.jpg"));
            records.insert(id, page["records"].as_array().unwrap().clone());
        }
        Self {
            root,
            path,
            media,
            owner,
            config,
            server,
            receipt: receipt.to_vec(),
            session: InventorySession {
                provider_owner: "shared-owner",
                records: Arc::new(records),
                displaced: Arc::new(AtomicBool::new(false)),
                unstable: Arc::new(AtomicBool::new(unstable)),
                unknown,
                initial_changes: false,
                unresolved_initial: false,
                expired: None,
                cancel_replay: None,
                replay_metadata: None,
                rank_calls: Arc::new(AtomicUsize::new(0)),
                trace: Arc::new(Mutex::new(Vec::new())),
            },
        }
    }

    async fn open(&self) -> Arc<SqliteStateDb> {
        Arc::new(
            SqliteStateDb::open_owned(&self.path, &self.owner)
                .await
                .unwrap(),
        )
    }

    async fn cycle(&self, named: bool, cancel: CancellationToken) -> CycleResult {
        let db = self.open().await;
        let mut album = PhotoAlbum::new(
            PhotoAlbumConfig {
                params: Arc::new(HashMap::new()),
                service_endpoint: Arc::from("https://fixture.invalid/shared"),
                name: Arc::from(if named { "Trip" } else { "" }),
                list_type: Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
                obj_type: Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
                query_filter: None,
                page_size: 2,
                zone_id: Arc::new(
                    json!({"zoneName":ZONE,"ownerRecordName":self.session.provider_owner}),
                ),
                retry_config: retry::RetryConfig {
                    max_retries: 0,
                    ..retry::RetryConfig::default()
                },
                container_id: named.then(|| Arc::from("trip")),
                cross_zone_sources: Vec::new(),
            },
            Box::new(self.session.clone()),
        );
        album.set_shadow_capture(
            ShadowCapture::new(db.clone(), self.owner.clone(), "com"),
            Arc::from(if self.session.provider_owner == "_defaultOwner" {
                "private"
            } else {
                "shared"
            }),
        );
        let lib = make_run_cycle_library_state_with_passes(
            ZONE,
            CURSOR,
            vec![AlbumPass {
                kind: if named {
                    PassKind::Album
                } else {
                    PassKind::Unfiled
                },
                album,
                exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
            }],
        );
        let base = make_run_cycle_download_config_builder(&self.media, db.clone());
        let build = |mode, excludes, groups, zone| {
            let mut config = base(mode, excludes, groups, zone);
            let config_mut = Arc::make_mut(&mut config);
            config_mut.retry.max_retries = 0;
            config_mut.max_download_attempts = 1;
            config
        };
        let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
        let result = Box::pin(run_cycle(
            &[&lib],
            &self.config,
            Some(db.as_ref()),
            false,
            &build,
            download::DownloadControls::download_hidden(),
            &shared,
            &cancel,
        ))
        .await
        .unwrap();
        drop(base);
        drop(lib);
        drop(db);
        result
    }

    async fn preserved(&self, pending: bool) {
        let db = self.open().await;
        assert_eq!(db.get_metadata(REQUEST).await.unwrap().is_some(), pending);
        let bytes: Vec<u8> = db
            .acquire_lock("retained receipt bytes")
            .unwrap()
            .query_row(
                "SELECT CAST(value AS BLOB) FROM metadata WHERE key=?1",
                [RECEIPT],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(bytes, self.receipt);
        assert_eq!(
            std::fs::read(self.media.join("retained.jpg")).unwrap(),
            b"unrelated retained media"
        );
        assert!(self.root.path().is_dir());
    }

    async fn gets(&self, id: &str) -> usize {
        self.server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| request.url.path() == format!("/{id}.jpg"))
            .count()
    }

    async fn quiet_twice(&self, named: bool) {
        let media = self.media_snapshot();
        let ranks = self.session.rank_calls.load(Ordering::SeqCst);
        let requests = self.server.received_requests().await.unwrap().len();
        for _ in 0..2 {
            let result = self.cycle(named, CancellationToken::new()).await;
            assert_eq!(result.failed_count, 0, "{result:?}");
            self.preserved(false).await;
            assert_eq!(
                self.media_snapshot(),
                media,
                "quiet reopen changed files, paths or mtimes"
            );
        }
        assert_eq!(self.session.rank_calls.load(Ordering::SeqCst), ranks);
        assert_eq!(
            self.server.received_requests().await.unwrap().len(),
            requests
        );
    }

    fn media_snapshot(
        &self,
    ) -> std::collections::BTreeMap<PathBuf, (Vec<u8>, std::time::SystemTime)> {
        let mut files = std::collections::BTreeMap::new();
        let mut directories = vec![self.media.clone()];
        while let Some(directory) = directories.pop() {
            for entry in std::fs::read_dir(directory).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    directories.push(path);
                } else {
                    files.insert(
                        path.clone(),
                        (
                            std::fs::read(&path).unwrap(),
                            std::fs::metadata(&path).unwrap().modified().unwrap(),
                        ),
                    );
                }
            }
        }
        files
    }
}

#[tokio::test]
async fn inventory_expired_initial_and_replay_boundaries_finish_current_work_then_hold_across_reopens()
 {
    for provider_owner in ["shared-owner", "_defaultOwner"] {
        for boundary in ["P", "S"] {
            let mut fixture =
                Fixture::new(b"expired retained history", Some("matching"), false, false).await;
            fixture.session.provider_owner = provider_owner;
            fixture.session.expired = Some(boundary);
            let result = fixture.cycle(false, CancellationToken::new()).await;
            assert!(
                !result.db_sync_token_advance_safe,
                "{provider_owner}/{boundary}: {result:?}"
            );
            assert_eq!(
                result.stats.sync_token_blocked_reason,
                Some("retained_checkpoint_expired")
            );
            fixture.preserved(true).await;
            let db = fixture.open().await;
            assert_eq!(db.get_metadata(CURSOR).await.unwrap().as_deref(), Some("P"));
            assert_eq!(db.get_downloaded_page(0, 10).await.unwrap().len(), 4);
            let hold_key = format!("retained_checkpoint_hold:{ZONE}");
            let hold = db
                .get_metadata(&hold_key)
                .await
                .unwrap()
                .expect("completed fallback must reach the existing durable hold");
            assert_eq!(
                serde_json::from_str::<Value>(&hold).unwrap()["attempt_count"],
                1
            );
            drop(db);
            for id in ["A", "B", "C", "D"] {
                assert_eq!(fixture.gets(id).await, 1);
            }
            let files = fixture.media_snapshot();
            let trace = fixture.session.trace.lock().unwrap().clone();
            assert_eq!(
                trace
                    .iter()
                    .filter(|entry| entry.as_str() == "rank:0")
                    .count(),
                1,
                "fallback must not repeat already completed inventory: {trace:?}"
            );
            assert!(
                trace
                    .iter()
                    .any(|entry| entry == &format!("delta:{boundary}"))
            );
            for _ in 0..2 {
                let held = fixture.cycle(false, CancellationToken::new()).await;
                assert!(!held.db_sync_token_advance_safe);
                fixture.preserved(true).await;
                assert_eq!(
                    *fixture.session.trace.lock().unwrap(),
                    trace,
                    "held reopen must not repeat provider work"
                );
                let db = fixture.open().await;
                assert_eq!(db.get_metadata(CURSOR).await.unwrap().as_deref(), Some("P"));
                assert_eq!(
                    db.get_metadata(&hold_key).await.unwrap().as_deref(),
                    Some(hold.as_str())
                );
                assert_eq!(fixture.media_snapshot(), files);
                for id in ["A", "B", "C", "D"] {
                    assert_eq!(fixture.gets(id).await, 1);
                }
            }
            // Repair the original provider boundary, not its saved cursor or
            // request. Only a normal due retry can qualify and retire the guard.
            fixture.session.expired = None;
            let db = fixture.open().await;
            let mut due: Value = serde_json::from_str(&hold).unwrap();
            due["next_attempt_at"] = json!(0);
            db.set_metadata(&hold_key, &due.to_string()).await.unwrap();
            drop(db);
            let recovered = fixture.cycle(false, CancellationToken::new()).await;
            assert_eq!(recovered.failed_count, 0, "{recovered:?}");
            fixture.preserved(false).await;
            assert!(
                fixture
                    .open()
                    .await
                    .get_metadata(&hold_key)
                    .await
                    .unwrap()
                    .is_none()
            );
            for id in ["A", "B", "C", "D"] {
                assert_eq!(fixture.gets(id).await, 1);
            }
            fixture.quiet_twice(false).await;
        }
    }
}

#[tokio::test]
async fn inventory_upgrade_preserves_any_old_receipt_and_recovers_quiet_shared_items_once() {
    for (receipt, hash) in [
        (b"{malformed\0\xff".as_slice(),Some("matching")),
        (b"{\"version\":99,\"complete\":true}".as_slice(),None),
        (b"{\"version\":1,\"bounded\":false,\"complete\":true,\"fingerprint\":\"wrong\",\"cursor_hash\":\"wrong\"}".as_slice(),Some("mismatching")),
    ] {
        let fixture = Fixture::new(receipt,hash,false,false).await;
        fixture.preserved(true).await;
        let result = fixture.cycle(false,CancellationToken::new()).await;
        assert_eq!(result.failed_count,0,"{result:?}");
        fixture.preserved(false).await;
        for id in ["A","B","C","D"] { assert_eq!(fixture.gets(id).await,1); }
        {
            let trace = fixture.session.trace.lock().unwrap();
            assert_eq!(trace.first().map(String::as_str),Some("delta:P"));
            assert!(trace.iter().position(|entry|entry == "rank:0").unwrap()
                < trace.iter().position(|entry|entry == "delta:S").unwrap());
        }
        fixture.quiet_twice(false).await;
    }
}

#[tokio::test]
async fn inventory_rank_displacement_retains_request_and_membership_then_finds_c_once() {
    let fixture = Fixture::new(b"unparsed", Some("matching"), true, false).await;
    let db = fixture.open().await;
    db.upsert_album_container(ZONE, "trip", "Trip", "icloud")
        .await
        .unwrap();
    let generation = db
        .start_album_membership_snapshot(ZONE, "trip", None)
        .await
        .unwrap();
    db.add_album_membership_to_snapshot(ZONE, "trip", generation, "asset-C", Some("C"), "icloud")
        .await
        .unwrap();
    db.complete_album_membership_snapshot(ZONE, "trip", generation)
        .await
        .unwrap();
    drop(db);
    let result = fixture.cycle(true, CancellationToken::new()).await;
    assert!(result.stats.sync_token_blocked, "{result:?}");
    fixture.preserved(true).await;
    assert_eq!(fixture.gets("C").await, 0);
    let db = fixture.open().await;
    let deleted: bool = db.acquire_lock("C membership").unwrap().query_row(
        "SELECT is_deleted FROM asset_album_memberships WHERE library=?1 AND container_id='trip' AND asset_record_name='asset-C'",
        [ZONE],|row|row.get(0)).unwrap();
    assert!(!deleted);
    assert_eq!(db.get_metadata(CURSOR).await.unwrap().as_deref(), Some("P"));
    let deleted_a: i64 = db.acquire_lock("explicit deleted current item").unwrap().query_row(
        "SELECT COUNT(*) FROM assets WHERE library=?1 AND id IN ('A','asset-A') AND is_deleted=1",[ZONE],|row|row.get(0)).unwrap();
    assert!(deleted_a > 0, "explicit source deletion was not applied");
    let files = fixture.media_snapshot();
    assert_eq!(
        files
            .iter()
            .find(|(path, _)| path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("A.jpg")))
            .expect("the explicitly deleted A file must remain")
            .1
            .0,
        BYTES
    );
    drop(db);
    fixture.session.unstable.store(false, Ordering::SeqCst);
    let recovered = fixture.cycle(true, CancellationToken::new()).await;
    assert_eq!(recovered.failed_count, 0, "{recovered:?}");
    fixture.preserved(false).await;
    for id in ["A", "B", "C", "D"] {
        assert_eq!(fixture.gets(id).await, 1);
    }
    fixture.quiet_twice(true).await;
}

#[tokio::test]
async fn inventory_unknown_raw_record_is_not_quiet_even_without_an_emitted_event() {
    for provider_owner in ["shared-owner", "_defaultOwner"] {
        let mut fixture = Fixture::new(b"future", Some("matching"), true, true).await;
        fixture.session.provider_owner = provider_owner;
        fixture.cycle(false, CancellationToken::new()).await;
        fixture.preserved(true).await;
        assert_eq!(
            fixture
                .open()
                .await
                .get_metadata(CURSOR)
                .await
                .unwrap()
                .as_deref(),
            Some("P")
        );
        fixture.session.unstable.store(false, Ordering::SeqCst);
        fixture.cycle(false, CancellationToken::new()).await;
        fixture.preserved(false).await;
        for id in ["A", "B", "C", "D"] {
            assert_eq!(fixture.gets(id).await, 1);
        }
        fixture.quiet_twice(false).await;
    }
}

#[tokio::test]
async fn inventory_clear_transaction_failure_and_cancellation_survive_restart() {
    for cancelled in [false, true] {
        let mut fixture = Fixture::new(b"old exact receipt", Some("matching"), false, false).await;
        let db = fixture.open().await;
        db.upsert_album_container(ZONE, "trip", "Trip", "icloud")
            .await
            .unwrap();
        let generation = db
            .start_album_membership_snapshot(ZONE, "trip", None)
            .await
            .unwrap();
        db.add_album_membership_to_snapshot(
            ZONE,
            "trip",
            generation,
            "absent-from-inventory",
            Some("absent"),
            "icloud",
        )
        .await
        .unwrap();
        db.complete_album_membership_snapshot(ZONE, "trip", generation)
            .await
            .unwrap();
        drop(db);
        let cancel = CancellationToken::new();
        if cancelled {
            fixture.session.cancel_replay = Some(cancel.clone());
        } else {
            fixture.open().await.acquire_lock("clear fault").unwrap().execute_batch(
                "CREATE TRIGGER fail_inventory_clear BEFORE DELETE ON metadata WHEN OLD.key='inventory_reconciliation_required:SharedSync-INVENTORY' BEGIN SELECT RAISE(ABORT,'clear fault'); END;"
            ).unwrap();
        }
        let result = fixture.cycle(true, cancel).await;
        if !cancelled {
            assert!(result.failed_count > 0);
        }
        fixture.preserved(true).await;
        assert_eq!(fixture.open().await.acquire_lock("failed snapshot retirement").unwrap().query_row::<i64,_,_>(
            "SELECT is_deleted FROM asset_album_memberships WHERE library=?1 AND asset_record_name='absent-from-inventory'",[ZONE],|row|row.get(0)).unwrap(),0);
        fixture.session.cancel_replay = None;
        if !cancelled {
            fixture
                .open()
                .await
                .acquire_lock("remove clear fault")
                .unwrap()
                .execute_batch("DROP TRIGGER fail_inventory_clear")
                .unwrap();
        }
        let recovered = fixture.cycle(true, CancellationToken::new()).await;
        assert_eq!(recovered.failed_count, 0, "{recovered:?}");
        fixture.preserved(false).await;
        assert_eq!(fixture.open().await.acquire_lock("qualified snapshot retirement").unwrap().query_row::<i64,_,_>(
            "SELECT is_deleted FROM asset_album_memberships WHERE library=?1 AND asset_record_name='absent-from-inventory'",[ZONE],|row|row.get(0)).unwrap(),1);
        for id in ["A", "B", "C", "D"] {
            assert_eq!(fixture.gets(id).await, 1);
        }
        fixture.quiet_twice(true).await;
    }
}

#[tokio::test]
async fn inventory_private_initial_identity_veto_keeps_healthy_jobs_and_retry_evidence() {
    let mut fixture =
        Fixture::new(b"unresolved current source", Some("matching"), false, false).await;
    fixture.session.provider_owner = "_defaultOwner";
    fixture.session.unresolved_initial = true;
    for _ in 0..2 {
        let held = fixture.cycle(false, CancellationToken::new()).await;
        assert!(
            held.failed_count > 0 && held.stats.identity_incomplete,
            "{held:?}"
        );
        fixture.preserved(true).await;
        let db = fixture.open().await;
        assert_eq!(db.get_metadata(CURSOR).await.unwrap().as_deref(), Some("P"));
        assert!(
            db.get_metadata(&crate::state::unresolved_identity_key(ZONE))
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(db.get_downloaded_page(0, 10).await.unwrap().len(), 4);
        for id in ["A", "B", "C", "D"] {
            assert_eq!(fixture.gets(id).await, 1);
        }
    }
}

#[tokio::test]
async fn inventory_initial_delta_failure_cannot_advance_before_inventory_qualifies() {
    let mut fixture = Fixture::new(b"delta failure", Some("matching"), false, false).await;
    fixture.session.initial_changes = true;
    Mock::given(method("GET"))
        .and(path("/C.jpg"))
        .respond_with(ResponseTemplate::new(500))
        .with_priority(1)
        .mount(&fixture.server)
        .await;
    let failed = fixture.cycle(false, CancellationToken::new()).await;
    assert!(failed.failed_count > 0, "{failed:?}");
    // Ordinary failed-URL refresh may query ranks, but the guard cannot proceed
    // to its qualifying second replay after the initial delta's failed work.
    assert!(
        !fixture
            .session
            .trace
            .lock()
            .unwrap()
            .iter()
            .any(|entry| entry == "delta:S")
    );
    fixture.preserved(true).await;
    assert_eq!(
        fixture
            .open()
            .await
            .get_metadata(CURSOR)
            .await
            .unwrap()
            .as_deref(),
        Some("P")
    );
    for id in ["A", "B", "D"] {
        assert_eq!(fixture.gets(id).await, 1);
    }
    fixture.server.reset().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(BYTES))
        .mount(&fixture.server)
        .await;
    let recovered = fixture.cycle(false, CancellationToken::new()).await;
    assert_eq!(recovered.failed_count, 0, "{recovered:?}");
    fixture.preserved(false).await;
    assert_eq!(fixture.gets("C").await, 1);
    for id in ["A", "B", "D"] {
        assert_eq!(fixture.gets(id).await, 0);
    }
    fixture.quiet_twice(false).await;
}

#[tokio::test]
async fn inventory_current_config_and_request_fences_hold_a_stale_plan() {
    for key in [REQUEST, crate::sync_cycle::PENDING_ENUM_CONFIG_HASH_KEY] {
        let mut fixture = Fixture::new(b"fenced history", Some("matching"), false, false).await;
        fixture.session.replay_metadata = Some((fixture.open().await, key, "concurrent change"));
        let stale = fixture.cycle(false, CancellationToken::new()).await;
        assert!(stale.failed_count > 0, "{key}: {stale:?}");
        fixture.preserved(true).await;
        fixture.session.replay_metadata = None;
        if key != REQUEST {
            fixture
                .open()
                .await
                .delete_metadata_by_prefix(key)
                .await
                .unwrap();
        }
        let recovered = fixture.cycle(false, CancellationToken::new()).await;
        assert_eq!(recovered.failed_count, 0, "{recovered:?}");
        fixture.preserved(false).await;
        for id in ["A", "B", "C", "D"] {
            assert_eq!(fixture.gets(id).await, 1);
        }
        fixture.quiet_twice(false).await;
    }
}

#[tokio::test]
async fn inventory_current_transfer_failure_keeps_successful_peers_across_restart() {
    let fixture = Fixture::new(b"old receipt", Some("matching"), false, false).await;
    Mock::given(method("GET"))
        .and(path("/C.jpg"))
        .respond_with(ResponseTemplate::new(500))
        .with_priority(1)
        .mount(&fixture.server)
        .await;
    let failed = fixture.cycle(false, CancellationToken::new()).await;
    assert!(failed.failed_count > 0, "{failed:?}");
    fixture.preserved(true).await;
    for id in ["A", "B", "D"] {
        assert_eq!(fixture.gets(id).await, 1);
    }
    fixture.server.reset().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(BYTES))
        .mount(&fixture.server)
        .await;
    let recovered = fixture.cycle(false, CancellationToken::new()).await;
    assert_eq!(recovered.failed_count, 0, "{recovered:?}");
    fixture.preserved(false).await;
    assert_eq!(fixture.gets("C").await, 1);
    for id in ["A", "B", "D"] {
        assert_eq!(fixture.gets(id).await, 0);
    }
    fixture.quiet_twice(false).await;
}

#[tokio::test]
async fn inventory_private_generation_uses_one_guarded_inventory_not_implicit_delta_scans() {
    let mut fixture = Fixture::new(b"private old receipt", Some("matching"), false, false).await;
    fixture.session.provider_owner = "_defaultOwner";
    let result = fixture.cycle(false, CancellationToken::new()).await;
    assert_eq!(result.failed_count, 0, "{result:?}");
    fixture.preserved(false).await;
    let roots: i64 = fixture
        .open()
        .await
        .acquire_lock("private inventory roots")
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM provider_active_generations",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(roots, 1);
    assert_eq!(
        fixture
            .session
            .trace
            .lock()
            .unwrap()
            .iter()
            .filter(|entry| entry.as_str() == "rank:0")
            .count(),
        1
    );
    for id in ["A", "B", "C", "D"] {
        assert_eq!(fixture.gets(id).await, 1);
    }
    fixture.quiet_twice(false).await;
}

#[tokio::test]
async fn inventory_qualified_fact_retires_without_historical_checkpoint_certification() {
    let fixture = Fixture::new(b"historical receipt", Some("matching"), false, false).await;
    fixture.open().await.acquire_lock("retained unrelated historical evidence").unwrap().execute(
        "INSERT INTO unattributed_legacy(library,asset_id,evidence_version,original_evidence,files,prepared_at) \
         VALUES(?1,'unrelated-history',1,'unparsed original history','[]',1)", [ZONE],
    ).unwrap();
    let result = fixture.cycle(false, CancellationToken::new()).await;
    assert!(result.db_sync_token_advance_safe, "{result:?}");
    fixture.preserved(false).await;
    assert_eq!(
        fixture
            .open()
            .await
            .get_metadata(CURSOR)
            .await
            .unwrap()
            .as_deref(),
        Some("E")
    );
    let guarded_replays = fixture
        .session
        .trace
        .lock()
        .unwrap()
        .iter()
        .filter(|entry| entry.as_str() == "delta:S")
        .count();
    for _ in 0..2 {
        fixture.cycle(false, CancellationToken::new()).await;
        fixture.preserved(false).await;
    }
    // Current identity and retained-cursor holds remain independent of this
    // request. Retirement cannot repeat its guarded replay or redownload
    // successfully finalized current media.
    assert_eq!(
        fixture
            .session
            .trace
            .lock()
            .unwrap()
            .iter()
            .filter(|entry| entry.as_str() == "delta:S")
            .count(),
        guarded_replays
    );
    for id in ["A", "B", "C", "D"] {
        assert_eq!(fixture.gets(id).await, 1);
    }
}

#[tokio::test]
async fn inventory_state_write_failure_retains_request_and_successful_media() {
    let fixture = Fixture::new(b"state-fault receipt", Some("matching"), false, false).await;
    fixture
        .open()
        .await
        .acquire_lock("current state write fault")
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER fail_current_finalization BEFORE UPDATE ON assets \
         WHEN NEW.id IN ('C','asset-C') AND NEW.status='downloaded' \
         BEGIN SELECT RAISE(ABORT,'current finalization fault'); END;",
        )
        .unwrap();
    let failed = fixture.cycle(false, CancellationToken::new()).await;
    assert!(
        failed.failed_count > 0 && failed.stats.state_write_failures > 0,
        "{failed:?}"
    );
    fixture.preserved(true).await;
    for id in ["A", "B", "D"] {
        assert_eq!(fixture.gets(id).await, 1);
    }
    fixture
        .open()
        .await
        .acquire_lock("remove current finalization fault")
        .unwrap()
        .execute_batch("DROP TRIGGER fail_current_finalization")
        .unwrap();
    let recovered = fixture.cycle(false, CancellationToken::new()).await;
    assert_eq!(recovered.failed_count, 0, "{recovered:?}");
    fixture.preserved(false).await;
    for id in ["A", "B", "D"] {
        assert_eq!(fixture.gets(id).await, 1);
    }
    fixture.quiet_twice(false).await;
}

#[tokio::test]
async fn inventory_missing_cursor_keeps_request_until_existing_first_run_activates() {
    let fixture = Fixture::new(b"missing cursor", Some("matching"), false, false).await;
    fixture
        .open()
        .await
        .delete_metadata_by_prefix(CURSOR)
        .await
        .unwrap();
    let first = fixture.cycle(false, CancellationToken::new()).await;
    assert_eq!(first.failed_count, 0, "{first:?}");
    fixture.preserved(true).await;
    assert!(
        !fixture
            .session
            .trace
            .lock()
            .unwrap()
            .iter()
            .any(|entry| entry.starts_with("delta:"))
    );
    let recovered = fixture.cycle(false, CancellationToken::new()).await;
    assert_eq!(recovered.failed_count, 0, "{recovered:?}");
    fixture.preserved(false).await;
    for id in ["A", "B", "C", "D"] {
        assert_eq!(fixture.gets(id).await, 1);
    }
    fixture.quiet_twice(false).await;
}

#[tokio::test]
async fn inventory_fixed_date_coverage_retires_without_claiming_unbounded_checkpoint() {
    let mut fixture = Fixture::new(b"bounded current policy", None, false, false).await;
    fixture.config.filters.skip_created_before = Some(config::CreatedDateFilter::CaptureDate(
        chrono::NaiveDate::from_ymd_opt(2025, 1, 1).unwrap(),
    ));
    // All provider items are older than the selected fixed lower date. Full
    // natural EOF still qualifies the empty selected inventory, not its cursor.
    let db = fixture.open().await;
    db.set_metadata(
        ENUM_CONFIG_HASH_KEY,
        &download::compute_config_hash(&fixture.config),
    )
    .await
    .unwrap();
    drop(db);
    // Keep the builder's fixed boundary identical to the actual app policy.
    let db = fixture.open().await;
    let base = make_run_cycle_download_config_builder(&fixture.media, db.clone());
    let build = |mode, excludes, groups, zone| {
        let mut config = base(mode, excludes, groups, zone);
        Arc::make_mut(&mut config).skip_created_before = fixture.config.filters.skip_created_before;
        config
    };
    let album = PhotoAlbum::new(
        PhotoAlbumConfig {
            params: Arc::new(HashMap::new()),
            service_endpoint: Arc::from("https://fixture.invalid/shared"),
            name: Arc::from(""),
            list_type: Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
            obj_type: Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
            query_filter: None,
            page_size: 2,
            zone_id: Arc::new(json!({"zoneName":ZONE,"ownerRecordName":"shared-owner"})),
            retry_config: retry::RetryConfig::default(),
            container_id: None,
            cross_zone_sources: Vec::new(),
        },
        Box::new(fixture.session.clone()),
    );
    let lib = make_run_cycle_library_state_with_passes(
        ZONE,
        CURSOR,
        vec![AlbumPass {
            kind: PassKind::Unfiled,
            album,
            exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
        }],
    );
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let result = run_cycle(
        &[&lib],
        &fixture.config,
        Some(db.as_ref()),
        false,
        &build,
        download::DownloadControls::download_hidden(),
        &shared,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(result.failed_count, 0, "{result:?}");
    fixture.preserved(false).await;
    assert_eq!(fixture.server.received_requests().await.unwrap().len(), 0);
    assert_eq!(db.get_metadata(CURSOR).await.unwrap().as_deref(), Some("P"));
}
