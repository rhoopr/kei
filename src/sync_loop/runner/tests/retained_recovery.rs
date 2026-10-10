//! Retained-token expiry is a durable, bounded hold, never inventory permission.
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::icloud::photos::PhotosSession;
use crate::state::SparseIdentityStore;
use crate::sync_cycle::{ENUM_CONFIG_HASH_KEY, PENDING_ENUM_CONFIG_HASH_KEY, run_cycle};
use crate::sync_loop::test_support::{
    RUN_CYCLE_ASSET_DATE_MS, RunCycleDownloadConfigOptions, album_count_response,
    full_album_page_with_download, make_full_album_with_boxed_session, make_run_cycle_config,
    make_run_cycle_download_config_builder_with_options, make_run_cycle_library_state_with_album,
    make_shared_session_for_run_cycle, media_without_photo_downloads,
};
use crate::{download, state};

const ZONE: &str = "PrimarySync";
const CURSOR_KEY: &str = "sync_token:PrimarySync";
const HOLD_KEY: &str = "retained_checkpoint_hold:PrimarySync";
const PRIOR: &str = "private-retained-cursor";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Trigger {
    Eligibility,
    Sparse,
    Legacy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fault {
    None,
    Partial,
    Cancel,
    HoldWrite,
    StalePlan,
    ConcurrentSparse,
    ConcurrentCursor,
    ConcurrentConfig,
}

#[derive(Clone)]
struct RetainedSession {
    records: Arc<Vec<Value>>,
    reject_prior: bool,
    fault: Fault,
    database: PathBuf,
    cancel: CancellationToken,
    queries: Arc<AtomicUsize>,
    requests: Arc<AtomicUsize>,
    inventories: Arc<AtomicUsize>,
    retained: Arc<AtomicUsize>,
    injected: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl PhotosSession for RetainedSession {
    async fn post(&self, url: &str, body: String, _: &[(&str, &str)]) -> anyhow::Result<Value> {
        self.requests.fetch_add(1, Ordering::SeqCst);
        let request: Value = serde_json::from_str(&body)?;
        if url.contains("/internal/records/query/batch?") {
            return Ok(album_count_response((self.records.len() / 2) as u64));
        }
        if url.contains("/records/lookup?") {
            let names = request["records"].as_array().unwrap();
            let records: Vec<_> = self
                .records
                .iter()
                .filter(|record| {
                    names
                        .iter()
                        .any(|name| name["recordName"] == record["recordName"])
                })
                .collect();
            return Ok(json!({"records": records}));
        }
        if url.contains("/records/query?") {
            self.queries.fetch_add(1, Ordering::SeqCst);
            if self.fault == Fault::ConcurrentSparse && !self.injected.swap(true, Ordering::SeqCst)
            {
                // A separate connection injects work after the cycle has planned
                // its inventory. The fixture, not selection code, owns this event.
                let db = state::SqliteStateDb::open(&self.database).await?;
                seed_sparse(&db, "arrived-during-scan").await;
            }
            if self.fault == Fault::ConcurrentCursor && !self.injected.swap(true, Ordering::SeqCst)
            {
                rusqlite::Connection::open(&self.database)?.execute(
                    "UPDATE metadata SET value='independently-advanced-cursor' WHERE key='sync_token:PrimarySync'", [],
                )?;
            }
            if self.fault == Fault::ConcurrentConfig && !self.injected.swap(true, Ordering::SeqCst)
            {
                rusqlite::Connection::open(&self.database)?.execute(
                    "UPDATE metadata SET value='independently-activated-config' WHERE key='enum_config_hash'", [],
                )?;
            }
            if self.fault == Fault::Cancel {
                self.cancel.cancel();
            }
            if self.fault == Fault::Partial {
                return Ok(json!({"records": null, "syncToken": "fresh-rank-anchor"}));
            }
            let offset = request["query"]["filterBy"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|filter| filter["fieldName"] == "startRank")
                .and_then(|filter| filter["fieldValue"]["value"].as_u64())
                .unwrap_or(0);
            let records = if offset == 0 {
                self.records.as_ref().clone()
            } else {
                Vec::new()
            };
            return Ok(json!({"records": records, "syncToken": "fresh-rank-anchor"}));
        }
        assert!(
            url.contains("/changes/zone?"),
            "unexpected retained fixture request"
        );
        let token = request["zones"][0]["syncToken"].as_str();
        if token == Some(PRIOR) {
            self.retained.fetch_add(1, Ordering::SeqCst);
            if self.reject_prior {
                return Ok(json!({"zones": [{
                    "zoneID": {"zoneName": ZONE, "ownerRecordName": "_defaultOwner"},
                    "serverErrorCode": "BAD_REQUEST", "reason": "private-owner private-provider-url",
                    "syncToken": "", "moreComing": false
                }]}));
            }
        }
        let records = if token.is_none() {
            self.inventories.fetch_add(1, Ordering::SeqCst);
            self.records.as_ref().clone()
        } else {
            Vec::new()
        };
        Ok(json!({"zones": [{
            "zoneID": {"zoneName": ZONE, "ownerRecordName": "_defaultOwner"},
            "syncToken": if token.is_none() {"fresh-source-anchor"} else {"validated-old-replay"},
            "moreComing": false, "records": records
        }]}))
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

async fn seed_sparse(db: &state::SqliteStateDb, source: &str) {
    let evidence = state::SparseEvidence::new(
        r#"[1,"private-child","SharedSync-absent","private-owner"]"#.to_owned(),
    );
    let row = db
        .observe_sparse_identity(
            ZONE,
            &state::SparseSourceId::new(source),
            &evidence,
            chrono::Utc::now(),
        )
        .await
        .unwrap();
    db.record_sparse_attempt(
        &row,
        state::SparseAttemptOutcome::Unresolved(evidence),
        chrono::Utc::now(),
    )
    .await
    .unwrap();
}

fn rows(database: &Path, sql: &str) -> Vec<Vec<rusqlite::types::Value>> {
    let connection = rusqlite::Connection::open(database).unwrap();
    let mut statement = connection.prepare(sql).unwrap();
    let width = statement.column_count();
    statement
        .query_map([], |row| (0..width).map(|index| row.get(index)).collect())
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

struct Fixture {
    _root: tempfile::TempDir,
    database: PathBuf,
    media: PathBuf,
    kept: PathBuf,
    records: Arc<Vec<Value>>,
    original: Vec<Vec<rusqlite::types::Value>>,
    original_revisions: Vec<Vec<rusqlite::types::Value>>,
    original_mappings: Vec<Vec<rusqlite::types::Value>>,
    original_retries: Vec<Vec<rusqlite::types::Value>>,
    original_sparse: Vec<Vec<rusqlite::types::Value>>,
    transfer_history: Vec<Vec<rusqlite::types::Value>>,
    bytes: Vec<u8>,
    modified: std::time::SystemTime,
}

impl Fixture {
    async fn new(trigger: Trigger) -> Self {
        Self::new_with_owner(trigger, None).await
    }
    async fn new_with_owner(
        trigger: Trigger,
        owner: Option<&state::db::account::AccountOwner>,
    ) -> Self {
        use base64::Engine as _;
        use sha2::{Digest, Sha256};
        let root = tempfile::tempdir().unwrap();
        let database = root.path().join("state.db");
        let media = root.path().join("media");
        std::fs::create_dir(&media).unwrap();
        let kept = media.join("historical.jpg");
        let bytes = include_bytes!("../../../../tests/data/media/pattern.jpg").to_vec();
        std::fs::write(&kept, &bytes).unwrap();
        std::fs::write(media.join("historical.jpg.xmp"), b"historical sidecar").unwrap();
        let companion = media.join("historical.MOV");
        let companion_bytes = b"historical companion media bytes";
        std::fs::write(&companion, companion_bytes).unwrap();
        let modified = std::fs::metadata(&kept).unwrap().modified().unwrap();
        let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&bytes));
        let mut records = Vec::new();
        {
            let db = if let Some(owner) = owner {
                state::SqliteStateDb::open_owned(&database, owner)
                    .await
                    .unwrap()
            } else {
                state::SqliteStateDb::open(&database).await.unwrap()
            };
            let date = chrono::DateTime::from_timestamp_millis(RUN_CYCLE_ASSET_DATE_MS).unwrap();
            let historical_created =
                chrono::DateTime::from_timestamp_millis(RUN_CYCLE_ASSET_DATE_MS - 86_400_123)
                    .unwrap();
            let id = if trigger == Trigger::Legacy {
                "legacy-master"
            } else {
                "asset-HISTORY"
            };
            let mut metadata = state::AssetMetadata::default();
            metadata.refresh_hash();
            let record = crate::test_helpers::TestAssetRecord::new(id)
                .filename("historical.jpg")
                .created_at(historical_created)
                .added_at(date)
                .checksum(&checksum)
                .size(bytes.len() as u64)
                .metadata(metadata.clone())
                .build();
            db.upsert_seen(&record).await.unwrap();
            let local = download::file::compute_sha256(&kept).await.unwrap();
            db.mark_downloaded(ZONE, id, "original", &kept, &local, Some(&local))
                .await
                .unwrap();
            let companion_checksum =
                base64::engine::general_purpose::STANDARD.encode(Sha256::digest(companion_bytes));
            let companion_local = download::file::compute_sha256(&companion).await.unwrap();
            db.upsert_seen(
                &crate::test_helpers::TestAssetRecord::new(id)
                    .version_size(state::VersionSizeKey::LiveOriginal)
                    .filename("historical.MOV")
                    .created_at(historical_created)
                    .added_at(date)
                    .checksum(&companion_checksum)
                    .size(companion_bytes.len() as u64)
                    .metadata(metadata.clone())
                    .build(),
            )
            .await
            .unwrap();
            db.mark_downloaded(
                ZONE,
                id,
                "live_original",
                &companion,
                &companion_local,
                Some(&companion_local),
            )
            .await
            .unwrap();
            if trigger == Trigger::Legacy {
                db.set_metadata_capture_revision_for_test(ZONE, id, 0);
                let page = full_album_page_with_download(
                    ZONE,
                    id,
                    "fresh-rank-anchor",
                    "https://p01.icloud-content.com/private-current.jpg",
                    bytes.len() as u64,
                    &checksum,
                );
                records.push(page["records"][0].clone());
                for child in ["current-child-a", "current-child-b"] {
                    db.upsert_asset_master_mapping(ZONE, child, id)
                        .await
                        .unwrap();
                    let mut source = page["records"][1].clone();
                    source["recordName"] = json!(child);
                    records.push(source);
                    let child_path = media.join(format!("{child}.jpg"));
                    std::fs::write(&child_path, &bytes).unwrap();
                    let child_record = crate::test_helpers::TestAssetRecord::new(child)
                        .filename("photo.jpg")
                        .created_at(date)
                        .added_at(date)
                        .checksum(&checksum)
                        .size(bytes.len() as u64)
                        .metadata(metadata.clone())
                        .build();
                    db.upsert_seen(&child_record).await.unwrap();
                    db.mark_downloaded(ZONE, child, "original", &child_path, &local, Some(&local))
                        .await
                        .unwrap();
                }
                db.begin_metadata_capture_revision(ZONE, state::METADATA_CAPTURE_REVISION)
                    .await
                    .unwrap();
                let candidate = db
                    .get_metadata_capture_candidates(ZONE, 1, 1)
                    .await
                    .unwrap()
                    .into_iter()
                    .find(|candidate| candidate.asset_id.as_str() == id)
                    .unwrap();
                assert!(
                    db.defer_metadata_capture_ambiguity(&candidate, 1)
                        .await
                        .unwrap()
                );
            } else {
                db.upsert_asset_master_mapping(ZONE, id, "HISTORY")
                    .await
                    .unwrap();
            }
            // Prior transfer failures remain durable historical retry evidence
            // while current policy excludes this non-enumerated identity.
            db.upsert_seen(
                &crate::test_helpers::TestAssetRecord::new("transfer-history")
                    .filename("transfer-history.jpg")
                    .created_at(historical_created)
                    .added_at(date)
                    .checksum("historical-provider-checksum")
                    .size(4096)
                    .build(),
            )
            .await
            .unwrap();
            db.upsert_asset_master_mapping(ZONE, "transfer-history", "transfer-history-master")
                .await
                .unwrap();
            for _ in 0..3 {
                db.mark_failed(
                    ZONE,
                    "transfer-history",
                    "original",
                    "historical transfer failure",
                )
                .await
                .unwrap();
            }
            db.acquire_lock("seed policy-excluded prior transfer history").unwrap().execute(
                "UPDATE assets SET status='pending' WHERE library='PrimarySync' AND id='transfer-history'", []
            ).unwrap();
            assert!(
                db.mark_policy_excluded(ZONE, "transfer-history", "original")
                    .await
                    .unwrap(),
                "fixture must seed an actual excluded row before preservation is asserted"
            );
            db.acquire_lock("seed retained transfer error history").unwrap().execute(
                "UPDATE assets SET last_error='historical transfer failure' WHERE library='PrimarySync' AND id='transfer-history'", []
            ).unwrap();
            let hash = if trigger == Trigger::Eligibility {
                "old-enumeration-generation".to_owned()
            } else {
                download::compute_config_hash(&make_run_cycle_config())
            };
            db.set_metadata(ENUM_CONFIG_HASH_KEY, &hash).await.unwrap();
            db.set_metadata(CURSOR_KEY, PRIOR).await.unwrap();
            if trigger == Trigger::Sparse {
                seed_sparse(&db, "private-source").await;
                let evidence = state::SparseEvidence::new(
                    r#"[1,"prior-deleted-child","SharedSync-absent","private-owner"]"#.to_owned(),
                );
                let row = db
                    .observe_sparse_identity(
                        ZONE,
                        &state::SparseSourceId::new("prior-deleted-source"),
                        &evidence,
                        chrono::Utc::now(),
                    )
                    .await
                    .unwrap();
                db.record_sparse_attempt(
                    &row,
                    state::SparseAttemptOutcome::SourceDeleted(
                        state::SparseDeletionCheckpoint::new(PRIOR).unwrap(),
                    ),
                    chrono::Utc::now(),
                )
                .await
                .unwrap();
                db.acquire_lock("defer saved deletion uncertainty").unwrap().execute(
                    "UPDATE unresolved_sparse_identities SET next_retry_at=?1 WHERE library='PrimarySync' AND source_record_name='prior-deleted-source'", [chrono::Utc::now().timestamp()+3600]
                ).unwrap();
            }
        }
        let original = rows(
            &database,
            "SELECT id, library, version_size, filename, created_at, added_at, status, local_path, checksum, local_checksum FROM assets WHERE id IN ('legacy-master','asset-HISTORY') ORDER BY id,version_size",
        );
        let original_revisions = rows(
            &database,
            "SELECT * FROM asset_metadata_capture_revisions WHERE asset_id IN ('legacy-master','asset-HISTORY') ORDER BY asset_id",
        );
        let original_mappings = rows(
            &database,
            "SELECT library,asset_record_name,master_record_name FROM asset_master_mappings WHERE master_record_name IN ('legacy-master','HISTORY','transfer-history-master') ORDER BY asset_record_name",
        );
        let original_retries = rows(
            &database,
            "SELECT * FROM metadata_capture_retries WHERE asset_id='legacy-master'",
        );
        let original_sparse = rows(
            &database,
            "SELECT * FROM unresolved_sparse_identities ORDER BY library,source_record_name",
        );
        let transfer_history = rows(
            &database,
            "SELECT library,id,version_size,filename,created_at,added_at,checksum,status,download_attempts,last_error FROM assets WHERE id='transfer-history'",
        );
        Self {
            _root: root,
            database,
            media,
            kept,
            records: Arc::new(records),
            original,
            original_revisions,
            original_mappings,
            original_retries,
            original_sparse,
            transfer_history,
            bytes,
            modified,
        }
    }
    fn assert_preserved(&self) {
        assert_eq!(std::fs::read(&self.kept).unwrap(), self.bytes);
        assert_eq!(
            std::fs::read(self.media.join("historical.jpg.xmp")).unwrap(),
            b"historical sidecar"
        );
        assert_eq!(
            std::fs::read(self.media.join("historical.MOV")).unwrap(),
            b"historical companion media bytes"
        );
        assert_eq!(
            std::fs::metadata(&self.kept).unwrap().modified().unwrap(),
            self.modified
        );
        assert_eq!(
            rows(
                &self.database,
                "SELECT id, library, version_size, filename, created_at, added_at, status, local_path, checksum, local_checksum FROM assets WHERE id IN ('legacy-master','asset-HISTORY') ORDER BY id,version_size"
            ),
            self.original
        );
        assert_eq!(
            rows(
                &self.database,
                "SELECT * FROM asset_metadata_capture_revisions WHERE asset_id IN ('legacy-master','asset-HISTORY') ORDER BY asset_id"
            ),
            self.original_revisions
        );
        assert_eq!(
            rows(
                &self.database,
                "SELECT library,asset_record_name,master_record_name FROM asset_master_mappings WHERE master_record_name IN ('legacy-master','HISTORY','transfer-history-master') ORDER BY asset_record_name"
            ),
            self.original_mappings
        );
        assert_eq!(
            rows(
                &self.database,
                "SELECT * FROM metadata_capture_retries WHERE asset_id='legacy-master'"
            ),
            self.original_retries
        );
        assert_eq!(
            rows(
                &self.database,
                "SELECT library,id,version_size,filename,created_at,added_at,checksum,status,download_attempts,last_error FROM assets WHERE id='transfer-history'"
            ),
            self.transfer_history
        );
    }
    fn session(
        &self,
        reject_prior: bool,
        fault: Fault,
        cancel: CancellationToken,
    ) -> RetainedSession {
        RetainedSession {
            records: self.records.clone(),
            reject_prior,
            fault,
            database: self.database.clone(),
            cancel,
            queries: Arc::new(AtomicUsize::new(0)),
            requests: Arc::new(AtomicUsize::new(0)),
            inventories: Arc::new(AtomicUsize::new(0)),
            retained: Arc::new(AtomicUsize::new(0)),
            injected: Arc::new(AtomicBool::new(false)),
        }
    }
}

#[tokio::test]
async fn retained_checkpoint_expiry_holds_all_trigger_classes_and_quiet_reopen() {
    for trigger in [Trigger::Eligibility, Trigger::Sparse, Trigger::Legacy] {
        let fixture = Fixture::new(trigger).await;
        let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
        let config = make_run_cycle_config();
        let old_hash = {
            let db = state::SqliteStateDb::open(&fixture.database).await.unwrap();
            db.get_metadata(ENUM_CONFIG_HASH_KEY).await.unwrap()
        };
        let mut receipt = None;
        for cycle in 0..8 {
            let db = Arc::new(state::SqliteStateDb::open(&fixture.database).await.unwrap());
            if [3, 5, 6, 7].contains(&cycle) {
                let mut due: Value =
                    serde_json::from_str(&db.get_metadata(HOLD_KEY).await.unwrap().unwrap())
                        .unwrap();
                due["next_attempt_at"] = json!(0);
                db.set_metadata(HOLD_KEY, &due.to_string()).await.unwrap();
            }
            let before_cycle_hold = db.get_metadata(HOLD_KEY).await.unwrap();
            let cancel = CancellationToken::new();
            let provider = fixture.session(true, Fault::None, cancel.clone());
            let mut library = make_run_cycle_library_state_with_album(
                ZONE,
                CURSOR_KEY,
                make_full_album_with_boxed_session(ZONE, Box::new(provider.clone())),
            );
            library.library = crate::icloud::photos::PhotoLibrary::new_stub_with_zone(
                Box::new(provider.clone()),
                ZONE,
            );
            if cycle > 0 {
                let mut precheck = crate::sync_loop::precheck::WatchPrecheck::SkipAll;
                crate::sync_loop::precheck::include_pending_local_work(
                    &mut precheck,
                    db.as_ref(),
                    &config.metadata,
                    std::slice::from_ref(&library),
                )
                .await;
                assert!(
                    precheck.should_sync_zone(ZONE),
                    "held selected zone must bypass quiet-provider shortcut"
                );
                assert!(!precheck.should_sync_zone("SharedSync-UNSELECTED"));
            }
            let builder = make_run_cycle_download_config_builder_with_options(
                &fixture.media,
                db.clone(),
                RunCycleDownloadConfigOptions {
                    media: media_without_photo_downloads(),
                    ..Default::default()
                },
            );
            let result = run_cycle(
                &[&library],
                &config,
                Some(db.as_ref()),
                false,
                &builder,
                download::DownloadControls::download_hidden(),
                &shared,
                &cancel,
            )
            .await
            .unwrap();
            assert!(
                !result.db_sync_token_advance_safe,
                "{trigger:?} cycle {cycle}: {result:?}"
            );
            assert_eq!(
                db.get_metadata(CURSOR_KEY).await.unwrap().as_deref(),
                Some(PRIOR)
            );
            assert_eq!(
                db.get_metadata(ENUM_CONFIG_HASH_KEY).await.unwrap(),
                old_hash
            );
            let raw = db
                .get_metadata(HOLD_KEY)
                .await
                .unwrap()
                .expect("expired retained replay must create a durable hold");
            let hold: Value = serde_json::from_str(&raw).unwrap();
            assert_eq!(hold["version"], 1);
            assert_eq!(
                hold["enum_config_hash"],
                download::compute_config_hash(&config)
            );
            assert_eq!(
                hold["attempt_count"],
                if cycle < 3 {
                    1
                } else if cycle < 5 {
                    2
                } else {
                    3
                }
            );
            if cycle < 6 {
                assert!(hold["next_attempt_at"].as_i64().unwrap() > chrono::Utc::now().timestamp());
            } else {
                assert_eq!(
                    Some(raw.clone()),
                    before_cycle_hold,
                    "exhausted receipts remain unchanged even when deadline is forced due"
                );
            }
            for private in [
                PRIOR,
                "private-owner",
                "private-provider-url",
                "legacy-master",
            ] {
                assert!(!raw.contains(private), "hold must be redacted: {raw}");
            }
            if [0, 3, 5].contains(&cycle) {
                assert!(provider.queries.load(Ordering::SeqCst) > 0);
                assert!(provider.retained.load(Ordering::SeqCst) > 0);
                receipt = Some(raw);
            } else {
                assert_eq!(
                    provider.requests.load(Ordering::SeqCst),
                    0,
                    "held zone must perform no provider operation"
                );
                assert_eq!(
                    provider.queries.load(Ordering::SeqCst),
                    0,
                    "held zone must not repeat rank inventory"
                );
                assert_eq!(
                    provider.inventories.load(Ordering::SeqCst),
                    0,
                    "held zone must not repeat legacy preparation"
                );
                assert_eq!(
                    provider.retained.load(Ordering::SeqCst),
                    0,
                    "reopen must preserve bounded retry deadline"
                );
                if cycle < 6 {
                    assert_eq!(Some(raw), receipt);
                }
            }
            assert_eq!(
                db.sparse_identities(ZONE).await.unwrap().len(),
                fixture.original_sparse.len()
            );
            assert_eq!(
                rows(
                    &fixture.database,
                    "SELECT * FROM unresolved_sparse_identities ORDER BY library,source_record_name"
                ),
                fixture.original_sparse,
                "sparse original evidence, generation, retries and deletion uncertainty must survive {trigger:?} cycle {cycle}"
            );
            if trigger == Trigger::Legacy {
                let preservation = db.legacy_preservations(ZONE).await.unwrap();
                assert_eq!(
                    preservation.len(),
                    1,
                    "fixture must reach real legacy preparation"
                );
                assert!(
                    preservation[0].active_generation.is_none(),
                    "expired retained replay must not activate preservation"
                );
                assert!(
                    rows(
                        &fixture.database,
                        "SELECT * FROM unattributed_legacy_proofs"
                    )
                    .is_empty()
                );
            }
            if trigger == Trigger::Sparse {
                assert!(
                    db.get_metadata(&state::unresolved_identity_key(ZONE))
                        .await
                        .unwrap()
                        .is_some()
                );
            }
            let summary = db.get_summary().await.unwrap();
            assert_eq!(
                summary.last_recovery_action.as_deref(),
                Some(if cycle < 5 {
                    "await_retained_checkpoint_evidence"
                } else {
                    "retained_checkpoint_retry_exhausted"
                })
            );
            fixture.assert_preserved();
        }
    }
}

#[tokio::test]
async fn retained_checkpoint_hold_valid_old_replay_recovers_then_two_quiet_reopens() {
    // This control repairs a previously rejected OLD cursor. A valid fresh
    // inventory alone never repairs permanent historical-token expiry.
    let fixture = Fixture::new(Trigger::Eligibility).await;
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let config = make_run_cycle_config();
    for cycle in 0..4 {
        let db = Arc::new(state::SqliteStateDb::open(&fixture.database).await.unwrap());
        if cycle == 1 {
            let raw = db.get_metadata(HOLD_KEY).await.unwrap().unwrap();
            let mut receipt: Value = serde_json::from_str(&raw).unwrap();
            receipt["next_attempt_at"] = json!(0);
            db.set_metadata(HOLD_KEY, &receipt.to_string())
                .await
                .unwrap();
        }
        let cancel = CancellationToken::new();
        let provider = fixture.session(cycle == 0, Fault::None, cancel.clone());
        let library = make_run_cycle_library_state_with_album(
            ZONE,
            CURSOR_KEY,
            make_full_album_with_boxed_session(ZONE, Box::new(provider.clone())),
        );
        let builder = make_run_cycle_download_config_builder_with_options(
            &fixture.media,
            db.clone(),
            RunCycleDownloadConfigOptions {
                media: media_without_photo_downloads(),
                ..Default::default()
            },
        );
        let result = run_cycle(
            &[&library],
            &config,
            Some(db.as_ref()),
            false,
            &builder,
            download::DownloadControls::download_hidden(),
            &shared,
            &cancel,
        )
        .await
        .unwrap();
        assert_eq!(
            db.get_metadata(CURSOR_KEY).await.unwrap().as_deref(),
            Some(if cycle == 0 {
                PRIOR
            } else {
                "validated-old-replay"
            }),
            "cycle {cycle}: {result:?}"
        );
        assert_eq!(
            db.get_metadata(HOLD_KEY).await.unwrap().is_some(),
            cycle == 0
        );
        if cycle > 0 {
            assert!(result.db_sync_token_advance_safe);
            assert_eq!(
                db.get_metadata(ENUM_CONFIG_HASH_KEY)
                    .await
                    .unwrap()
                    .as_deref(),
                Some(download::compute_config_hash(&config).as_str())
            );
            assert!(
                db.get_metadata(PENDING_ENUM_CONFIG_HASH_KEY)
                    .await
                    .unwrap()
                    .is_none()
            );
        }
        if cycle >= 2 {
            assert_eq!(provider.queries.load(Ordering::SeqCst), 0);
            assert_eq!(provider.inventories.load(Ordering::SeqCst), 0);
            assert_eq!(provider.retained.load(Ordering::SeqCst), 0);
        }
        fixture.assert_preserved();
    }
}

#[tokio::test]
async fn retained_checkpoint_expiry_controls_never_promote_or_clear_debt() {
    for fault in [
        Fault::Partial,
        Fault::Cancel,
        Fault::HoldWrite,
        Fault::StalePlan,
        Fault::ConcurrentSparse,
        Fault::ConcurrentCursor,
        Fault::ConcurrentConfig,
    ] {
        let fixture = Fixture::new(Trigger::Eligibility).await;
        let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
        let db = Arc::new(state::SqliteStateDb::open(&fixture.database).await.unwrap());
        if fault == Fault::HoldWrite {
            db.acquire_lock("retained hold write fault").unwrap().execute_batch(
                "CREATE TRIGGER retained_hold_fault BEFORE INSERT ON metadata WHEN NEW.key='retained_checkpoint_hold:PrimarySync' BEGIN SELECT RAISE(ABORT,'injected hold write failure'); END;"
            ).unwrap();
        }
        let cancel = CancellationToken::new();
        let provider = fixture.session(true, fault, cancel.clone());
        let mut library = make_run_cycle_library_state_with_album(
            ZONE,
            CURSOR_KEY,
            make_full_album_with_boxed_session(ZONE, Box::new(provider.clone())),
        );
        library.plan_is_stale = fault == Fault::StalePlan;
        let builder = make_run_cycle_download_config_builder_with_options(
            &fixture.media,
            db.clone(),
            RunCycleDownloadConfigOptions {
                media: media_without_photo_downloads(),
                ..Default::default()
            },
        );
        let result = run_cycle(
            &[&library],
            &make_run_cycle_config(),
            Some(db.as_ref()),
            false,
            &builder,
            download::DownloadControls::download_hidden(),
            &shared,
            &cancel,
        )
        .await;
        assert!(
            result.is_err()
                || result
                    .as_ref()
                    .is_ok_and(|result| !result.db_sync_token_advance_safe),
            "{fault:?}: {result:?}"
        );
        assert_eq!(
            db.get_metadata(CURSOR_KEY).await.unwrap().as_deref(),
            Some(if fault == Fault::ConcurrentCursor {
                "independently-advanced-cursor"
            } else {
                PRIOR
            })
        );
        assert_eq!(
            db.get_metadata(ENUM_CONFIG_HASH_KEY)
                .await
                .unwrap()
                .as_deref(),
            Some(if fault == Fault::ConcurrentConfig {
                "independently-activated-config"
            } else {
                "old-enumeration-generation"
            })
        );
        if fault == Fault::ConcurrentSparse {
            assert!(provider.injected.load(Ordering::SeqCst));
            assert_eq!(db.sparse_identities(ZONE).await.unwrap().len(), 1);
            assert!(
                db.get_metadata(&state::unresolved_identity_key(ZONE))
                    .await
                    .unwrap()
                    .is_some()
            );
        } else {
            assert!(
                db.get_metadata(HOLD_KEY).await.unwrap().is_none(),
                "incomplete/failed/stale inventories cannot write a completed hold: {fault:?}"
            );
        }
        fixture.assert_preserved();
    }
}

#[derive(Clone)]
struct UnaffectedSession {
    changes: Arc<AtomicUsize>,
}
#[async_trait::async_trait]
impl PhotosSession for UnaffectedSession {
    async fn post(&self, url: &str, _: String, _: &[(&str, &str)]) -> anyhow::Result<Value> {
        if url.contains("/changes/zone?") {
            self.changes.fetch_add(1, Ordering::SeqCst);
            return Ok(json!({"zones":[{
                "zoneID":{"zoneName":"SharedSync-CLEAN","ownerRecordName":"_defaultOwner"},
                "syncToken":"clean-zone-successor","moreComing":false,"records":[]
            }]}));
        }
        if url.contains("/internal/records/query/batch?") {
            return Ok(album_count_response(0));
        }
        Ok(json!({"records":[],"syncToken":"clean-inventory-anchor"}))
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

#[tokio::test]
async fn retained_checkpoint_held_zone_does_not_block_clean_zone_or_clear_aggregate_hold() {
    let fixture = Fixture::new(Trigger::Sparse).await;
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    {
        let db = state::SqliteStateDb::open(&fixture.database).await.unwrap();
        db.set_metadata("sync_token:SharedSync-CLEAN", "clean-zone-before")
            .await
            .unwrap();
    }
    for cycle in 0..3 {
        let db = Arc::new(state::SqliteStateDb::open(&fixture.database).await.unwrap());
        let cancel = CancellationToken::new();
        let primary_provider = fixture.session(true, Fault::None, cancel.clone());
        let primary = make_run_cycle_library_state_with_album(
            ZONE,
            CURSOR_KEY,
            make_full_album_with_boxed_session(ZONE, Box::new(primary_provider.clone())),
        );
        let clean_changes = Arc::new(AtomicUsize::new(0));
        let clean = make_run_cycle_library_state_with_album(
            "SharedSync-CLEAN",
            "sync_token:SharedSync-CLEAN",
            make_full_album_with_boxed_session(
                "SharedSync-CLEAN",
                Box::new(UnaffectedSession {
                    changes: clean_changes.clone(),
                }),
            ),
        );
        let builder = make_run_cycle_download_config_builder_with_options(
            &fixture.media,
            db.clone(),
            RunCycleDownloadConfigOptions {
                media: media_without_photo_downloads(),
                ..Default::default()
            },
        );
        let result = run_cycle(
            &[&primary, &clean],
            &make_run_cycle_config(),
            Some(db.as_ref()),
            false,
            &builder,
            download::DownloadControls::download_hidden(),
            &shared,
            &cancel,
        )
        .await
        .unwrap();
        assert!(
            !result.db_sync_token_advance_safe,
            "cycle {cycle}: {result:?}"
        );
        assert_eq!(
            db.get_metadata(CURSOR_KEY).await.unwrap().as_deref(),
            Some(PRIOR)
        );
        assert_eq!(
            db.get_metadata("sync_token:SharedSync-CLEAN")
                .await
                .unwrap()
                .as_deref(),
            Some("clean-zone-successor")
        );
        assert_eq!(
            clean_changes.load(Ordering::SeqCst),
            1,
            "unaffected zone must continue normal polling"
        );
        assert!(db.get_metadata(HOLD_KEY).await.unwrap().is_some());
        assert_eq!(
            db.sparse_identities(ZONE).await.unwrap().len(),
            fixture.original_sparse.len()
        );
        assert_eq!(
            rows(
                &fixture.database,
                "SELECT * FROM unresolved_sparse_identities ORDER BY library,source_record_name"
            ),
            fixture.original_sparse
        );
        assert_eq!(
            db.get_summary()
                .await
                .unwrap()
                .last_recovery_action
                .as_deref(),
            Some("await_retained_checkpoint_evidence")
        );
        if cycle > 0 {
            assert_eq!(primary_provider.queries.load(Ordering::SeqCst), 0);
            assert_eq!(primary_provider.retained.load(Ordering::SeqCst), 0);
        }
        fixture.assert_preserved();
    }
}

#[tokio::test]
async fn retained_checkpoint_due_retry_write_failure_prevents_every_provider_request() {
    let fixture = Fixture::new(Trigger::Eligibility).await;
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    for cycle in 0..2 {
        let db = Arc::new(state::SqliteStateDb::open(&fixture.database).await.unwrap());
        if cycle == 1 {
            let mut hold: Value =
                serde_json::from_str(&db.get_metadata(HOLD_KEY).await.unwrap().unwrap()).unwrap();
            hold["next_attempt_at"] = json!(0);
            db.set_metadata(HOLD_KEY, &hold.to_string()).await.unwrap();
            db.acquire_lock("due retry reservation failure").unwrap().execute_batch(
                "CREATE TRIGGER retained_retry_fault BEFORE UPDATE ON metadata WHEN NEW.key='retained_checkpoint_hold:PrimarySync' BEGIN SELECT RAISE(ABORT,'injected retry reservation failure'); END;"
            ).unwrap();
        }
        let cancel = CancellationToken::new();
        let provider = fixture.session(true, Fault::None, cancel.clone());
        let library = make_run_cycle_library_state_with_album(
            ZONE,
            CURSOR_KEY,
            make_full_album_with_boxed_session(ZONE, Box::new(provider.clone())),
        );
        let builder = make_run_cycle_download_config_builder_with_options(
            &fixture.media,
            db.clone(),
            RunCycleDownloadConfigOptions {
                media: media_without_photo_downloads(),
                ..Default::default()
            },
        );
        let result = run_cycle(
            &[&library],
            &make_run_cycle_config(),
            Some(db.as_ref()),
            false,
            &builder,
            download::DownloadControls::download_hidden(),
            &shared,
            &cancel,
        )
        .await;
        assert!(
            result.is_err()
                || result
                    .as_ref()
                    .is_ok_and(|result| !result.db_sync_token_advance_safe)
        );
        assert_eq!(
            db.get_metadata(CURSOR_KEY).await.unwrap().as_deref(),
            Some(PRIOR)
        );
        if cycle == 1 {
            assert_eq!(provider.queries.load(Ordering::SeqCst), 0);
            assert_eq!(provider.inventories.load(Ordering::SeqCst), 0);
            assert_eq!(provider.retained.load(Ordering::SeqCst), 0);
            let hold: Value =
                serde_json::from_str(&db.get_metadata(HOLD_KEY).await.unwrap().unwrap()).unwrap();
            assert_eq!(
                hold["next_attempt_at"], 0,
                "failed reservation cannot acknowledge a future retry"
            );
        }
        fixture.assert_preserved();
    }
}

#[derive(Clone)]
struct ArrivingSourceSession {
    base: RetainedSession,
    source: Arc<std::sync::Mutex<Vec<Value>>>,
    arrival: Arc<Vec<Value>>,
    injected: Arc<AtomicBool>,
    delivered: Arc<AtomicUsize>,
}
#[async_trait::async_trait]
impl PhotosSession for ArrivingSourceSession {
    async fn post(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        let request: Value = serde_json::from_str(&body)?;
        if url.contains("/records/query?") {
            self.base.queries.fetch_add(1, Ordering::SeqCst);
            let before = self.source.lock().unwrap().clone();
            // The provider fixture owns this independently specified event.
            // The first rank inventory has already taken its snapshot when the
            // change arrives. Subsequent inventory and retained replay see it.
            if !self.injected.swap(true, Ordering::SeqCst) {
                self.source
                    .lock()
                    .unwrap()
                    .extend(self.arrival.iter().cloned());
            }
            let offset = request["query"]["filterBy"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|filter| filter["fieldName"] == "startRank")
                .and_then(|filter| filter["fieldValue"]["value"].as_u64())
                .unwrap_or(0);
            return Ok(
                json!({"records": if offset == 0 {before} else {Vec::new()}, "syncToken":"arrival-rank-anchor"}),
            );
        }
        if url.contains("/changes/zone?")
            && request["zones"][0]["syncToken"] == PRIOR
            && !self.base.reject_prior
        {
            self.base.retained.fetch_add(1, Ordering::SeqCst);
            let records = self.source.lock().unwrap().clone();
            self.delivered.fetch_add(records.len(), Ordering::SeqCst);
            return Ok(json!({"zones":[{
                "zoneID":{"zoneName":ZONE,"ownerRecordName":"_defaultOwner"},
                "syncToken":"validated-old-replay","moreComing":false,"records":records
            }]}));
        }
        self.base.post(url, body, headers).await
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

#[tokio::test]
async fn retained_checkpoint_provider_change_during_scan_remains_replayable_then_is_durable() {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    let owner = state::db::account::AccountOwner::authenticated(
        "synthetic@example.invalid",
        "com",
        &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-provider"}})).unwrap(),
    )
    .unwrap();
    let fixture = Fixture::new_with_owner(Trigger::Eligibility, Some(&owner)).await;
    let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&fixture.bytes));
    let arrival_page = full_album_page_with_download(
        ZONE,
        "arrived-provider-master",
        "unused-provider-anchor",
        "https://p01.icloud-content.com/arrival.jpg",
        fixture.bytes.len() as u64,
        &checksum,
    );
    let arrival = Arc::new(arrival_page["records"].as_array().unwrap().clone());
    let source = Arc::new(std::sync::Mutex::new(Vec::new()));
    let injected = Arc::new(AtomicBool::new(false));
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    for cycle in 0..6 {
        let db = Arc::new(
            state::SqliteStateDb::open_owned(&fixture.database, &owner)
                .await
                .unwrap(),
        );
        if cycle == 2 || cycle == 3 {
            let mut hold: Value =
                serde_json::from_str(&db.get_metadata(HOLD_KEY).await.unwrap().unwrap()).unwrap();
            hold["next_attempt_at"] = json!(0);
            db.set_metadata(HOLD_KEY, &hold.to_string()).await.unwrap();
        }
        let cancel = CancellationToken::new();
        let base = fixture.session(cycle < 3, Fault::None, cancel.clone());
        let delivered = Arc::new(AtomicUsize::new(0));
        let provider = ArrivingSourceSession {
            base: base.clone(),
            source: source.clone(),
            arrival: arrival.clone(),
            injected: injected.clone(),
            delivered: delivered.clone(),
        };
        let capture =
            crate::icloud::photos::inbox::ShadowCapture::new(db.clone(), owner.clone(), "com");
        let mut album = make_full_album_with_boxed_session(ZONE, Box::new(provider));
        album.set_shadow_capture(capture, Arc::from("private"));
        let library = make_run_cycle_library_state_with_album(ZONE, CURSOR_KEY, album);
        let builder = make_run_cycle_download_config_builder_with_options(
            &fixture.media,
            db.clone(),
            RunCycleDownloadConfigOptions {
                media: media_without_photo_downloads(),
                ..Default::default()
            },
        );
        let result = run_cycle(
            &[&library],
            &make_run_cycle_config(),
            Some(db.as_ref()),
            false,
            &builder,
            download::DownloadControls::download_hidden(),
            &shared,
            &cancel,
        )
        .await
        .unwrap();
        assert!(injected.load(Ordering::SeqCst));
        assert_eq!(
            *source.lock().unwrap(),
            *arrival,
            "provider still offers the independently specified change"
        );
        assert_eq!(
            db.get_metadata(CURSOR_KEY).await.unwrap().as_deref(),
            Some(if cycle < 3 {
                PRIOR
            } else {
                "validated-old-replay"
            })
        );
        assert_eq!(
            db.get_metadata(HOLD_KEY).await.unwrap().is_some(),
            cycle < 3
        );
        if cycle < 3 {
            assert!(!result.db_sync_token_advance_safe);
            assert_eq!(
                delivered.load(Ordering::SeqCst),
                0,
                "fresh rank inventory must not count as retained replay"
            );
        } else {
            assert!(
                result.db_sync_token_advance_safe,
                "cycle {cycle}: {result:?}"
            );
            let captured = rows(
                &fixture.database,
                "SELECT record_name,record_type FROM provider_shadow_records WHERE record_name IN ('arrived-provider-master','asset-arrived-provider-master') ORDER BY record_name",
            );
            assert_eq!(
                captured.len(),
                2,
                "actual retained replay must durably capture both arrived records"
            );
            assert_eq!(
                db.get_master_record_name_for_asset(ZONE, "asset-arrived-provider-master")
                    .await
                    .unwrap()
                    .as_deref(),
                Some("arrived-provider-master"),
                "the known newly arrived identity may be added alongside preserved historical mappings"
            );
            if cycle == 3 {
                assert_eq!(delivered.load(Ordering::SeqCst), 2);
            }
        }
        if cycle == 1 || cycle >= 4 {
            assert_eq!(base.queries.load(Ordering::SeqCst), 0);
            assert_eq!(base.retained.load(Ordering::SeqCst), 0);
        }
        fixture.assert_preserved();
    }
}

#[tokio::test]
async fn retained_checkpoint_due_interrupted_inventory_consumes_attempt_without_promotion() {
    for fault in [Fault::Partial, Fault::Cancel] {
        let fixture = Fixture::new(Trigger::Eligibility).await;
        let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
        for cycle in 0..3 {
            let db = Arc::new(state::SqliteStateDb::open(&fixture.database).await.unwrap());
            if cycle == 1 {
                let mut hold: Value =
                    serde_json::from_str(&db.get_metadata(HOLD_KEY).await.unwrap().unwrap())
                        .unwrap();
                hold["next_attempt_at"] = json!(0);
                db.set_metadata(HOLD_KEY, &hold.to_string()).await.unwrap();
            }
            let cancel = CancellationToken::new();
            let provider = fixture.session(
                true,
                if cycle == 1 { fault } else { Fault::None },
                cancel.clone(),
            );
            let library = make_run_cycle_library_state_with_album(
                ZONE,
                CURSOR_KEY,
                make_full_album_with_boxed_session(ZONE, Box::new(provider.clone())),
            );
            let builder = make_run_cycle_download_config_builder_with_options(
                &fixture.media,
                db.clone(),
                RunCycleDownloadConfigOptions {
                    media: media_without_photo_downloads(),
                    ..Default::default()
                },
            );
            let result = run_cycle(
                &[&library],
                &make_run_cycle_config(),
                Some(db.as_ref()),
                false,
                &builder,
                download::DownloadControls::download_hidden(),
                &shared,
                &cancel,
            )
            .await;
            assert!(
                result.is_err()
                    || result
                        .as_ref()
                        .is_ok_and(|result| !result.db_sync_token_advance_safe),
                "{fault:?} cycle {cycle}: {result:?}"
            );
            assert_eq!(
                db.get_metadata(CURSOR_KEY).await.unwrap().as_deref(),
                Some(PRIOR)
            );
            assert_eq!(
                db.get_metadata(ENUM_CONFIG_HASH_KEY)
                    .await
                    .unwrap()
                    .as_deref(),
                Some("old-enumeration-generation")
            );
            let hold: Value =
                serde_json::from_str(&db.get_metadata(HOLD_KEY).await.unwrap().unwrap()).unwrap();
            assert_eq!(
                hold["attempt_count"],
                if cycle == 0 { 1 } else { 2 },
                "reservation must survive {fault:?}"
            );
            assert!(hold["next_attempt_at"].as_i64().unwrap() > chrono::Utc::now().timestamp());
            if cycle == 1 {
                assert!(provider.queries.load(Ordering::SeqCst) > 0);
            }
            if cycle == 2 {
                assert_eq!(provider.queries.load(Ordering::SeqCst), 0);
                assert_eq!(provider.retained.load(Ordering::SeqCst), 0);
            }
            fixture.assert_preserved();
        }
    }
}

#[tokio::test]
async fn retained_checkpoint_malformed_receipt_stops_provider_work_without_forgetting_hold() {
    for invalid in [0, 1, 2] {
        let fixture = Fixture::new(Trigger::Eligibility).await;
        let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
        for cycle in 0..2 {
            let db = Arc::new(state::SqliteStateDb::open(&fixture.database).await.unwrap());
            let mut invalid_raw = None;
            if cycle == 1 {
                let valid = db.get_metadata(HOLD_KEY).await.unwrap().unwrap();
                let mut value: Value = serde_json::from_str(&valid).unwrap();
                let raw = match invalid {
                    0 => "{invalid".to_owned(),
                    1 => {
                        value["version"] = json!(2);
                        value.to_string()
                    }
                    _ => {
                        value["unrecognized_authority"] = json!(true);
                        value.to_string()
                    }
                };
                db.set_metadata(HOLD_KEY, &raw).await.unwrap();
                invalid_raw = Some(raw);
            }
            let cancel = CancellationToken::new();
            let provider = fixture.session(true, Fault::None, cancel.clone());
            let library = make_run_cycle_library_state_with_album(
                ZONE,
                CURSOR_KEY,
                make_full_album_with_boxed_session(ZONE, Box::new(provider.clone())),
            );
            let builder = make_run_cycle_download_config_builder_with_options(
                &fixture.media,
                db.clone(),
                RunCycleDownloadConfigOptions {
                    media: media_without_photo_downloads(),
                    ..Default::default()
                },
            );
            let result = run_cycle(
                &[&library],
                &make_run_cycle_config(),
                Some(db.as_ref()),
                false,
                &builder,
                download::DownloadControls::download_hidden(),
                &shared,
                &cancel,
            )
            .await;
            assert!(
                result.is_err()
                    || result
                        .as_ref()
                        .is_ok_and(|result| !result.db_sync_token_advance_safe)
            );
            assert_eq!(
                db.get_metadata(CURSOR_KEY).await.unwrap().as_deref(),
                Some(PRIOR)
            );
            if cycle == 1 {
                assert_eq!(
                    db.get_metadata(HOLD_KEY).await.unwrap(),
                    invalid_raw,
                    "unsupported durable evidence must remain reviewable"
                );
                assert_eq!(provider.queries.load(Ordering::SeqCst), 0);
                assert_eq!(provider.inventories.load(Ordering::SeqCst), 0);
                assert_eq!(provider.retained.load(Ordering::SeqCst), 0);
            }
            fixture.assert_preserved();
        }
    }
}

#[tokio::test]
async fn retained_checkpoint_reopened_hold_preserves_actual_failed_transfer_history() {
    let fixture = Fixture::new(Trigger::Eligibility).await;
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let mut failed_history = None;
    for cycle in 0..4 {
        let db = Arc::new(state::SqliteStateDb::open(&fixture.database).await.unwrap());
        if cycle == 1 {
            // Independently arriving failed work must survive a retained hold.
            // Ordinary retry activation is not reached in these held cycles.
            db.upsert_seen(
                &crate::test_helpers::TestAssetRecord::new("held-failed-transfer")
                    .filename("held-failed-transfer.jpg")
                    .checksum("held-provider-checksum")
                    .size(8192)
                    .build(),
            )
            .await
            .unwrap();
            for _ in 0..3 {
                db.mark_failed(
                    ZONE,
                    "held-failed-transfer",
                    "original",
                    "retained failed transfer cause",
                )
                .await
                .unwrap();
            }
            failed_history = Some(rows(
                &fixture.database,
                "SELECT * FROM assets WHERE library='PrimarySync' AND id='held-failed-transfer'",
            ));
        }
        if cycle == 3 {
            let mut hold: Value =
                serde_json::from_str(&db.get_metadata(HOLD_KEY).await.unwrap().unwrap()).unwrap();
            hold["attempt_count"] = json!(3);
            hold["next_attempt_at"] = json!(0);
            db.set_metadata(HOLD_KEY, &hold.to_string()).await.unwrap();
        }
        let cancel = CancellationToken::new();
        let provider = fixture.session(true, Fault::None, cancel.clone());
        let library = make_run_cycle_library_state_with_album(
            ZONE,
            CURSOR_KEY,
            make_full_album_with_boxed_session(ZONE, Box::new(provider.clone())),
        );
        let builder = make_run_cycle_download_config_builder_with_options(
            &fixture.media,
            db.clone(),
            RunCycleDownloadConfigOptions {
                media: media_without_photo_downloads(),
                ..Default::default()
            },
        );
        let result = run_cycle(
            &[&library],
            &make_run_cycle_config(),
            Some(db.as_ref()),
            false,
            &builder,
            download::DownloadControls::download_hidden(),
            &shared,
            &cancel,
        )
        .await
        .unwrap();
        assert!(!result.db_sync_token_advance_safe);
        assert_eq!(
            db.get_metadata(CURSOR_KEY).await.unwrap().as_deref(),
            Some(PRIOR)
        );
        if cycle > 0 {
            assert_eq!(
                provider.requests.load(Ordering::SeqCst),
                0,
                "hold must stop before retries and hydration"
            );
            assert_eq!(
                Some(rows(
                    &fixture.database,
                    "SELECT * FROM assets WHERE library='PrimarySync' AND id='held-failed-transfer'"
                )),
                failed_history,
                "identity, status, checksum, attempts3 and original failure must survive held cycle {cycle}"
            );
        }
        fixture.assert_preserved();
    }
}
