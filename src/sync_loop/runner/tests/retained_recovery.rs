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
    make_shared_session_for_run_cycle, media_without_photo_downloads, run_cycle_account_owner,
};
use crate::{download, state};

const ZONE: &str = "PrimarySync";
const CURSOR_KEY: &str = "sync_token:v27:PrimarySync";
const HOLD_KEY: &str = "retained_checkpoint_hold:v27:PrimarySync";
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
    priming: bool,
    priming_successor: &'static str,
    reject_historical: bool,
    historical_rejected: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl PhotosSession for RetainedSession {
    async fn post(&self, url: &str, body: String, _: &[(&str, &str)]) -> anyhow::Result<Value> {
        self.requests.fetch_add(1, Ordering::SeqCst);
        let request: Value = serde_json::from_str(&body)?;
        if url.ends_with("/zones/list") {
            return Ok(
                json!({"zones":[{"zoneID":{"zoneName":ZONE,"ownerRecordName":"_defaultOwner"},"syncToken":"fresh-source-anchor","deleted":false}]}),
            );
        }
        if request["query"]["recordType"] == "CheckIndexingState" {
            return Ok(
                json!({"records":[{"recordName":"index-state","recordType":"CheckIndexingState","fields":{"state":{"value":"FINISHED"}}}]}),
            );
        }
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
                    "UPDATE metadata SET value='independently-advanced-cursor' WHERE key='sync_token:v27:PrimarySync'", [],
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
        if self.reject_historical && token == Some("fresh-source-anchor") {
            self.historical_rejected.fetch_add(1, Ordering::SeqCst);
            return Ok(
                json!({"zones": [{"zoneID":{"zoneName":ZONE,"ownerRecordName":"_defaultOwner"},"serverErrorCode":"BAD_REQUEST","syncToken":"","moreComing":false}]}),
            );
        }
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
            "syncToken": if token.is_none() {"fresh-source-anchor"} else if self.priming {self.priming_successor} else {"validated-old-replay"},
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
        Self::new_with_owner_and_priming(trigger, owner, false).await
    }
    async fn new_with_owner_and_priming(
        trigger: Trigger,
        owner: Option<&state::db::account::AccountOwner>,
        priming: bool,
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
            if !priming {
                db.set_metadata(ENUM_CONFIG_HASH_KEY, &hash).await.unwrap();
                db.set_metadata(CURSOR_KEY, PRIOR).await.unwrap();
            }
            if trigger == Trigger::Sparse && !priming {
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
            priming: false,
            priming_successor: PRIOR,
            reject_historical: false,
            historical_rejected: Arc::new(AtomicUsize::new(0)),
        }
    }
}

async fn retained_account_library(
    db: &Arc<state::SqliteStateDb>,
    owner: &state::db::account::AccountOwner,
    provider: Box<dyn PhotosSession>,
) -> crate::icloud::photos::PhotoLibrary {
    let mut library = crate::icloud::photos::PhotoLibrary::new(
        "https://example.com/database/1/photos/production/private".into(),
        Arc::new(std::collections::HashMap::new()),
        provider,
        Arc::new(json!({"zoneName":ZONE,"ownerRecordName":"_defaultOwner"})),
        "private".into(),
        crate::retry::RetryConfig::default(),
    )
    .await
    .unwrap();
    library.shadow_capture = Some(crate::icloud::photos::inbox::ShadowCapture::new(
        db.clone(),
        owner.clone(),
        "com",
    ));
    library
}

// Each invocation drops its account-owned database, library, passes and builder
// before returning. The next cycle opens SQLite without a surviving fixture handle.
fn owned_sparse_config() -> crate::config::Config {
    let mut config = make_run_cycle_config();
    // The fixture resolves exactly one unfiled pass and no named albums.
    config.filters.selection.albums = crate::selection::AlbumSelector::None;
    config
}

async fn owned_sparse_cycle(
    fixture: &Fixture,
    owner: &state::db::account::AccountOwner,
    shared: &crate::auth::SharedSession,
    provider: RetainedSession,
) -> crate::sync_cycle::CycleResult {
    owned_sparse_cycle_with_media(
        fixture,
        owner,
        shared,
        provider,
        media_without_photo_downloads(),
    )
    .await
}

async fn owned_sparse_cycle_with_media(
    fixture: &Fixture,
    owner: &state::db::account::AccountOwner,
    shared: &crate::auth::SharedSession,
    mut provider: RetainedSession,
    media: crate::config::MediaSelection,
) -> crate::sync_cycle::CycleResult {
    let db = Arc::new(
        state::SqliteStateDb::open_owned(&fixture.database, owner)
            .await
            .unwrap(),
    );
    let mut source = crate::icloud::photos::PhotoLibrary::new(
        "https://example.com/database/1/photos/production/private".into(),
        Arc::new(std::collections::HashMap::new()),
        Box::new(provider.clone()),
        Arc::new(json!({"zoneName":ZONE,"ownerRecordName":"_defaultOwner"})),
        "private".into(),
        crate::retry::RetryConfig::default(),
    )
    .await
    .unwrap();
    source.shadow_capture = Some(crate::icloud::photos::inbox::ShadowCapture::new(
        db.clone(),
        owner.clone(),
        "com",
    ));
    let mut library = make_run_cycle_library_state_with_album(ZONE, CURSOR_KEY, source.all());
    library.library = source;
    let base = make_run_cycle_download_config_builder_with_options(
        &fixture.media,
        db.clone(),
        RunCycleDownloadConfigOptions {
            media,
            ..Default::default()
        },
    );
    let builder = |mode, exclude, groups, library| {
        let mut config = (*base(mode, exclude, groups, library)).clone();
        config.retry.max_retries = 0;
        Arc::new(config)
    };
    let cancel = std::mem::take(&mut provider.cancel);
    let mut result = run_cycle(
        &[&library],
        &owned_sparse_config(),
        Some(db.as_ref()),
        false,
        &builder,
        download::DownloadControls::download_hidden(),
        shared,
        &cancel,
    )
    .await
    .unwrap();
    // CycleResult retains pre-tail operational readers for the real runner.
    // This fixture has inspected the cycle result and does not run that tail.
    for source in &mut result.sources {
        source.operational_prework = None;
    }
    drop(base);
    drop(library);
    assert!(
        Arc::try_unwrap(db).is_ok(),
        "all cycle-owned database handles must close before reopen"
    );
    result
}

async fn primed_sparse_fixture(
    owner: &state::db::account::AccountOwner,
    shared: &crate::auth::SharedSession,
    deletion_checkpoint: &str,
) -> Fixture {
    primed_sparse_fixture_with_media(
        owner,
        shared,
        deletion_checkpoint,
        media_without_photo_downloads(),
    )
    .await
}

async fn primed_sparse_fixture_with_media(
    owner: &state::db::account::AccountOwner,
    shared: &crate::auth::SharedSession,
    deletion_checkpoint: &str,
    media: crate::config::MediaSelection,
) -> Fixture {
    let mut fixture = Fixture::new_with_owner_and_priming(Trigger::Sparse, Some(owner), true).await;
    let mut provider = fixture.session(false, Fault::None, CancellationToken::new());
    provider.priming = true;
    let primed = owned_sparse_cycle_with_media(&fixture, owner, shared, provider, media).await;
    assert!(
        primed.db_sync_token_advance_safe,
        "starting baseline must complete: {primed:?}"
    );
    seed_original_sparse_history(&mut fixture, owner, deletion_checkpoint).await;
    fixture
}

async fn seed_original_sparse_history(
    fixture: &mut Fixture,
    owner: &state::db::account::AccountOwner,
    deletion_checkpoint: &str,
) {
    {
        let db = state::SqliteStateDb::open_owned(&fixture.database, owner)
            .await
            .unwrap();
        assert_eq!(
            db.get_metadata(CURSOR_KEY).await.unwrap().as_deref(),
            Some(PRIOR)
        );
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
                state::SparseDeletionCheckpoint::new(deletion_checkpoint).unwrap(),
            ),
            chrono::Utc::now(),
        )
        .await
        .unwrap();
        // This is the original retained-recovery fixture's synthetic history,
        // not a new source receipt or an authenticated deletion observation.
        db.acquire_lock("defer saved deletion uncertainty").unwrap().execute(
            "UPDATE unresolved_sparse_identities SET next_retry_at=?1 WHERE library='PrimarySync' AND source_record_name='prior-deleted-source'", [chrono::Utc::now().timestamp()+3600]
        ).unwrap();
    }
    fixture.original_sparse = rows(
        &fixture.database,
        "SELECT * FROM unresolved_sparse_identities ORDER BY library,source_record_name",
    );
    fixture.assert_preserved();
}

#[tokio::test]
async fn retained_main_rejection_with_sparse_debt_is_bounded_across_closed_reopens() {
    use sha2::{Digest, Sha256};
    let owner = run_cycle_account_owner();
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let fixture = primed_sparse_fixture(&owner, &shared, PRIOR).await;
    let mut saved_hold = None;
    let mut saved_rounds = None;
    let mut saved_catalog_debt = None;
    let active_metadata = rows(
        &fixture.database,
        "SELECT key,value FROM metadata WHERE key IN ('sync_token:v27:PrimarySync','observed_selection:v27:PrimarySync','enum_config_hash','config_hash') ORDER BY key",
    );
    for cycle in 0..3 {
        let provider = fixture.session(true, Fault::None, CancellationToken::new());
        let result = owned_sparse_cycle(&fixture, &owner, &shared, provider.clone()).await;
        assert!(
            !result.db_sync_token_advance_safe,
            "cycle {cycle}: {result:?}"
        );
        let db = state::SqliteStateDb::open_owned(&fixture.database, &owner)
            .await
            .unwrap();
        assert_eq!(
            db.get_metadata(CURSOR_KEY).await.unwrap().as_deref(),
            Some(PRIOR)
        );
        let raw = db
            .get_metadata(HOLD_KEY)
            .await
            .unwrap()
            .expect("typed main rejection must retain a bounded hold");
        let hold: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            hold["prior_cursor_sha256"],
            data_encoding::HEXLOWER.encode(&Sha256::digest(PRIOR))
        );
        assert_eq!(
            hold["enum_config_hash"],
            download::compute_config_hash(&owned_sparse_config())
        );
        assert_eq!(hold["attempt_count"], 1);
        assert!(hold["next_attempt_at"].as_i64().unwrap() > chrono::Utc::now().timestamp());
        if let Some(previous) = &saved_hold {
            assert_eq!(&raw, previous, "not-due cycles cannot change the hold");
            assert_eq!(provider.retained.load(Ordering::SeqCst), 0);
            assert_eq!(provider.queries.load(Ordering::SeqCst), 0);
        } else {
            assert!(
                provider.retained.load(Ordering::SeqCst) > 0,
                "main rejection must be reached"
            );
            assert!(
                provider.queries.load(Ordering::SeqCst) > 0,
                "fresh replacement inventory must run"
            );
            saved_hold = Some(raw);
        }
        drop(db);
        assert_eq!(
            rows(
                &fixture.database,
                "SELECT key,value FROM metadata WHERE key IN ('sync_token:v27:PrimarySync','observed_selection:v27:PrimarySync','enum_config_hash','config_hash') ORDER BY key"
            ),
            active_metadata
        );
        let rounds = rows(
            &fixture.database,
            "SELECT id,mode,phase,boundary FROM source_revalidation_rounds ORDER BY id",
        );
        let debt = rows(
            &fixture.database,
            "SELECT round,id,next_retry_at FROM source_revalidation_debt ORDER BY round,id",
        );
        if let Some(expected) = &saved_rounds {
            assert_eq!(
                &rounds, expected,
                "not-due cycles cannot replace or activate the fresh origin"
            );
            assert_eq!(
                Some(&debt),
                saved_catalog_debt.as_ref(),
                "not-due cycles cannot clear catalog debt"
            );
        } else {
            assert!(
                rounds.iter().any(|row| row.get(1)
                    == Some(&rusqlite::types::Value::Text("catalog_v2".into()))
                    && row.get(2) == Some(&rusqlite::types::Value::Text("observing".into()))),
                "replacement origin must remain observing"
            );
            saved_rounds = Some(rounds);
            saved_catalog_debt = Some(debt);
        }
        assert_eq!(
            rows(
                &fixture.database,
                "SELECT * FROM unresolved_sparse_identities ORDER BY library,source_record_name"
            ),
            fixture.original_sparse
        );
        fixture.assert_preserved();
    }
    for attempt in 2..=3 {
        {
            let db = state::SqliteStateDb::open_owned(&fixture.database, &owner)
                .await
                .unwrap();
            let mut hold: Value =
                serde_json::from_str(&db.get_metadata(HOLD_KEY).await.unwrap().unwrap()).unwrap();
            assert_eq!(hold["attempt_count"], attempt - 1);
            // Exercise the existing hold's due boundary without waiting an hour.
            hold["next_attempt_at"] = json!(0);
            db.set_metadata(HOLD_KEY, &hold.to_string()).await.unwrap();
        }
        let provider = fixture.session(true, Fault::None, CancellationToken::new());
        let result = owned_sparse_cycle(&fixture, &owner, &shared, provider.clone()).await;
        assert!(
            !result.db_sync_token_advance_safe,
            "due attempt {attempt}: {result:?}"
        );
        assert!(
            provider.queries.load(Ordering::SeqCst) > 0,
            "due attempt must resume fresh inventory"
        );
        let db = state::SqliteStateDb::open_owned(&fixture.database, &owner)
            .await
            .unwrap();
        let raw = db.get_metadata(HOLD_KEY).await.unwrap().unwrap();
        let hold: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(hold["attempt_count"], attempt);
        assert_eq!(
            hold["prior_cursor_sha256"],
            data_encoding::HEXLOWER.encode(&Sha256::digest(PRIOR))
        );
        assert!(hold["next_attempt_at"].as_i64().unwrap() > chrono::Utc::now().timestamp());
        assert_eq!(
            db.get_metadata(CURSOR_KEY).await.unwrap().as_deref(),
            Some(PRIOR)
        );
        drop(db);
        assert_eq!(
            rows(
                &fixture.database,
                "SELECT key,value FROM metadata WHERE key IN ('sync_token:v27:PrimarySync','observed_selection:v27:PrimarySync','enum_config_hash','config_hash') ORDER BY key"
            ),
            active_metadata
        );
        assert_eq!(
            rows(
                &fixture.database,
                "SELECT * FROM unresolved_sparse_identities ORDER BY library,source_record_name"
            ),
            fixture.original_sparse
        );
        fixture.assert_preserved();
    }
    let exhausted = {
        let db = state::SqliteStateDb::open_owned(&fixture.database, &owner)
            .await
            .unwrap();
        let mut hold: Value =
            serde_json::from_str(&db.get_metadata(HOLD_KEY).await.unwrap().unwrap()).unwrap();
        hold["next_attempt_at"] = json!(0);
        let raw = hold.to_string();
        db.set_metadata(HOLD_KEY, &raw).await.unwrap();
        raw
    };
    let provider = fixture.session(true, Fault::None, CancellationToken::new());
    let result = owned_sparse_cycle(&fixture, &owner, &shared, provider.clone()).await;
    assert!(!result.db_sync_token_advance_safe);
    assert_eq!(provider.retained.load(Ordering::SeqCst), 0);
    assert_eq!(provider.queries.load(Ordering::SeqCst), 0);
    let db = state::SqliteStateDb::open_owned(&fixture.database, &owner)
        .await
        .unwrap();
    assert_eq!(
        db.get_metadata(HOLD_KEY).await.unwrap().as_deref(),
        Some(exhausted.as_str())
    );
    assert_eq!(
        db.get_metadata(CURSOR_KEY).await.unwrap().as_deref(),
        Some(PRIOR)
    );
    drop(db);
    assert_eq!(
        rows(
            &fixture.database,
            "SELECT * FROM unresolved_sparse_identities ORDER BY library,source_record_name"
        ),
        fixture.original_sparse
    );
    fixture.assert_preserved();
}

#[tokio::test]
async fn retained_main_hold_write_failure_is_visible_without_checkpoint_promotion() {
    let owner = run_cycle_account_owner();
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let fixture = primed_sparse_fixture(&owner, &shared, PRIOR).await;
    {
        let db = state::SqliteStateDb::open_owned(&fixture.database, &owner)
            .await
            .unwrap();
        db.acquire_lock("inject hold write failure").unwrap().execute_batch(
            "CREATE TRIGGER retained_hold_fault BEFORE INSERT ON metadata WHEN NEW.key='retained_checkpoint_hold:v27:PrimarySync' BEGIN SELECT RAISE(ABORT,'injected hold write failure'); END;"
        ).unwrap();
    }
    let provider = fixture.session(true, Fault::None, CancellationToken::new());
    let result = owned_sparse_cycle(&fixture, &owner, &shared, provider.clone()).await;
    assert!(provider.retained.load(Ordering::SeqCst) > 0);
    assert!(!result.db_sync_token_advance_safe);
    assert!(
        result.stats.state_write_failures > 0,
        "failed hold write must be visible: {result:?}"
    );
    let db = state::SqliteStateDb::open_owned(&fixture.database, &owner)
        .await
        .unwrap();
    assert_eq!(
        db.get_metadata(CURSOR_KEY).await.unwrap().as_deref(),
        Some(PRIOR)
    );
    assert!(db.get_metadata(HOLD_KEY).await.unwrap().is_none());
    drop(db);
    assert_eq!(
        rows(
            &fixture.database,
            "SELECT * FROM unresolved_sparse_identities ORDER BY library,source_record_name"
        ),
        fixture.original_sparse
    );
    fixture.assert_preserved();
}

#[tokio::test]
async fn retained_historical_sparse_rejection_alone_does_not_create_main_hold() {
    let owner = run_cycle_account_owner();
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let fixture = primed_sparse_fixture(&owner, &shared, "fresh-source-anchor").await;
    let mut provider = fixture.session(false, Fault::None, CancellationToken::new());
    provider.reject_historical = true;
    let result = owned_sparse_cycle(&fixture, &owner, &shared, provider.clone()).await;
    assert!(
        provider.retained.load(Ordering::SeqCst) > 0,
        "valid main token must be requested"
    );
    assert!(
        provider.historical_rejected.load(Ordering::SeqCst) > 0,
        "different historical deletion token must be rejected"
    );
    assert!(
        !result.db_sync_token_advance_safe,
        "unresolved identity remains held: {result:?}"
    );
    let db = state::SqliteStateDb::open_owned(&fixture.database, &owner)
        .await
        .unwrap();
    assert_eq!(
        db.get_metadata(CURSOR_KEY).await.unwrap().as_deref(),
        Some(PRIOR)
    );
    assert!(
        db.get_metadata(HOLD_KEY).await.unwrap().is_none(),
        "historical rejection is not a main rejection"
    );
    drop(db);
    assert_eq!(
        rows(
            &fixture.database,
            "SELECT * FROM unresolved_sparse_identities ORDER BY library,source_record_name"
        ),
        fixture.original_sparse
    );
    fixture.assert_preserved();
}

#[derive(Clone)]
struct HeldRetryBytes {
    allow: Arc<AtomicBool>,
    calls: Arc<AtomicUsize>,
    bytes: Arc<Vec<u8>>,
}

impl wiremock::Respond for HeldRetryBytes {
    fn respond(&self, _: &wiremock::Request) -> wiremock::ResponseTemplate {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.allow.load(Ordering::SeqCst) {
            wiremock::ResponseTemplate::new(200).set_body_bytes(self.bytes.as_slice())
        } else {
            wiremock::ResponseTemplate::new(404)
        }
    }
}

#[tokio::test]
async fn retained_main_hold_allows_due_admitted_media_retry_without_source_replay() {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};

    let owner = run_cycle_account_owner();
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let media = crate::config::MediaSelection::all();
    let fixture = primed_sparse_fixture_with_media(&owner, &shared, PRIOR, media).await;
    let allow = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(AtomicUsize::new(0));
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(method("GET"))
        .and(path("/retry.jpg"))
        .respond_with(HeldRetryBytes {
            allow: allow.clone(),
            calls: calls.clone(),
            bytes: Arc::new(fixture.bytes.clone()),
        })
        .mount(&server)
        .await;
    let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&fixture.bytes));
    let page = full_album_page_with_download(
        ZONE,
        "retry-new",
        "unused-rank",
        &format!("{}/retry.jpg", server.uri()),
        fixture.bytes.len() as u64,
        &checksum,
    );
    let records = Arc::new(page["records"].as_array().unwrap().clone());
    let mut provider = fixture.session(true, Fault::None, CancellationToken::new());
    provider.records = records.clone();
    let first =
        owned_sparse_cycle_with_media(&fixture, &owner, &shared, provider.clone(), media).await;
    assert!(
        provider.retained.load(Ordering::SeqCst) > 0,
        "the main rejection must be reached"
    );
    assert!(
        !first.db_sync_token_advance_safe,
        "the sparse identity remains held: {first:?}"
    );
    assert!(
        calls.load(Ordering::SeqCst) > 0,
        "the admitted media must actually fail first"
    );
    let initial_calls = calls.load(Ordering::SeqCst);
    let hold = {
        let db = state::SqliteStateDb::open_owned(&fixture.database, &owner)
            .await
            .unwrap();
        {
            let conn = db.acquire_lock("inspect admitted failed media").unwrap();
            let (admitted, verified): (i64, i64) = conn.query_row(
                "SELECT admitted,verified_media FROM provider_active_destinations WHERE child='asset-retry-new'", [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            ).unwrap();
            assert_eq!((admitted, verified), (1, 0));
        }
        let hold = db
            .get_metadata(HOLD_KEY)
            .await
            .unwrap()
            .expect("rejected main cursor must be held");
        assert_eq!(
            db.get_metadata(CURSOR_KEY).await.unwrap().as_deref(),
            Some(PRIOR)
        );
        db.make_selection_retry_due(owner.clone(), "asset-retry-new".to_owned())
            .await
            .unwrap();
        hold
    };
    allow.store(true, Ordering::SeqCst);
    let mut provider = fixture.session(true, Fault::None, CancellationToken::new());
    provider.records = records;
    let retried =
        owned_sparse_cycle_with_media(&fixture, &owner, &shared, provider.clone(), media).await;
    assert!(
        !retried.db_sync_token_advance_safe,
        "media retry cannot promote the source checkpoint"
    );
    assert_eq!(
        provider.queries.load(Ordering::SeqCst),
        0,
        "backoff must not repeat inventory"
    );
    assert_eq!(
        provider.retained.load(Ordering::SeqCst),
        0,
        "backoff must not replay the rejected source"
    );
    assert!(
        calls.load(Ordering::SeqCst) > initial_calls,
        "the due admitted media must retry: {retried:?}"
    );
    let db = state::SqliteStateDb::open_owned(&fixture.database, &owner)
        .await
        .unwrap();
    {
        let conn = db.acquire_lock("inspect verified retry").unwrap();
        let (admitted, verified): (i64, i64) = conn.query_row(
            "SELECT admitted,verified_media FROM provider_active_destinations WHERE child='asset-retry-new'", [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        ).unwrap();
        assert_eq!((admitted, verified), (1, 1));
        let local: String = conn.query_row("SELECT local_path FROM assets WHERE library=?1 AND id='asset-retry-new' AND version_size='original'", [ZONE], |row| row.get(0)).unwrap();
        assert_eq!(std::fs::read(local).unwrap(), fixture.bytes);
    }
    assert_eq!(
        db.get_metadata(CURSOR_KEY).await.unwrap().as_deref(),
        Some(PRIOR)
    );
    assert_eq!(
        db.get_metadata(HOLD_KEY).await.unwrap().as_deref(),
        Some(hold.as_str())
    );
    drop(db);
    assert_eq!(
        rows(
            &fixture.database,
            "SELECT * FROM unresolved_sparse_identities ORDER BY library,source_record_name"
        ),
        fixture.original_sparse
    );
    fixture.assert_preserved();
}

const HELD_PEER: &str = "SharedSync-retained-peer";

#[derive(Clone)]
struct HeldPeerSession {
    step: usize,
    changes: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl PhotosSession for HeldPeerSession {
    async fn post(&self, url: &str, body: String, _: &[(&str, &str)]) -> anyhow::Result<Value> {
        let request: Value = serde_json::from_str(&body)?;
        let zone = json!({"zoneName":HELD_PEER,"ownerRecordName":"synthetic-peer-owner"});
        if request["query"]["recordType"] == "CheckIndexingState" {
            return Ok(
                json!({"records":[{"recordName":"index-state","recordType":"CheckIndexingState","fields":{"state":{"value":"FINISHED"}}}]}),
            );
        }
        if url.ends_with("/zones/list") {
            return Ok(
                json!({"zones":[{"zoneID":zone,"syncToken":"peer-baseline","deleted":false}]}),
            );
        }
        if url.contains("/internal/records/query/batch?") {
            return Ok(album_count_response(0));
        }
        if url.contains("/records/query?") {
            return Ok(json!({"records":[],"syncToken":"peer-rank"}));
        }
        if url.contains("/changes/zone?") {
            self.changes.fetch_add(1, Ordering::SeqCst);
            return Ok(
                json!({"zones":[{"zoneID":zone,"records":[],"syncToken":format!("peer-after-{}", self.step),"moreComing":false}]}),
            );
        }
        anyhow::bail!("unexpected synthetic peer endpoint")
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

async fn owned_held_pair_cycle(
    fixture: &Fixture,
    owner: &state::db::account::AccountOwner,
    shared: &crate::auth::SharedSession,
    mut provider: RetainedSession,
    peer: HeldPeerSession,
) -> crate::sync_cycle::CycleResult {
    let db = Arc::new(
        state::SqliteStateDb::open_owned(&fixture.database, owner)
            .await
            .unwrap(),
    );
    let mut primary = crate::icloud::photos::PhotoLibrary::new(
        "https://example.com/database/1/photos/production/private".into(),
        Arc::new(std::collections::HashMap::new()),
        Box::new(provider.clone()),
        Arc::new(json!({"zoneName":ZONE,"ownerRecordName":"_defaultOwner"})),
        "private".into(),
        crate::retry::RetryConfig::default(),
    )
    .await
    .unwrap();
    primary.shadow_capture = Some(crate::icloud::photos::inbox::ShadowCapture::new(
        db.clone(),
        owner.clone(),
        "com",
    ));
    let mut other = crate::icloud::photos::PhotoLibrary::new(
        "https://example.com/database/1/photos/production/shared".into(),
        Arc::new(std::collections::HashMap::new()),
        Box::new(peer),
        Arc::new(json!({"zoneName":HELD_PEER,"ownerRecordName":"synthetic-peer-owner"})),
        "shared".into(),
        crate::retry::RetryConfig::default(),
    )
    .await
    .unwrap();
    other.shadow_capture = Some(crate::icloud::photos::inbox::ShadowCapture::new(
        db.clone(),
        owner.clone(),
        "com",
    ));
    let mut first = make_run_cycle_library_state_with_album(ZONE, CURSOR_KEY, primary.all());
    first.library = primary;
    let mut second = make_run_cycle_library_state_with_album(
        HELD_PEER,
        &format!("sync_token:v27:{HELD_PEER}"),
        other.all(),
    );
    second.library = other;
    let base = make_run_cycle_download_config_builder_with_options(
        &fixture.media,
        db.clone(),
        RunCycleDownloadConfigOptions {
            media: media_without_photo_downloads(),
            ..Default::default()
        },
    );
    let builder = |mode, exclude, groups, library| {
        let mut config = (*base(mode, exclude, groups, library)).clone();
        config.retry.max_retries = 0;
        Arc::new(config)
    };
    let cancel = std::mem::take(&mut provider.cancel);
    let mut result = run_cycle(
        &[&first, &second],
        &owned_sparse_config(),
        Some(db.as_ref()),
        false,
        &builder,
        download::DownloadControls::download_hidden(),
        shared,
        &cancel,
    )
    .await
    .unwrap();
    for source in &mut result.sources {
        source.operational_prework = None;
    }
    drop(base);
    drop(first);
    drop(second);
    assert!(
        Arc::try_unwrap(db).is_ok(),
        "all pair-cycle database handles must close"
    );
    result
}

#[tokio::test]
async fn retained_main_backoff_does_not_block_healthy_peer_progress() {
    let owner = run_cycle_account_owner();
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let mut fixture =
        Fixture::new_with_owner_and_priming(Trigger::Sparse, Some(&owner), true).await;
    let mut primary = fixture.session(false, Fault::None, CancellationToken::new());
    primary.priming = true;
    let primed = owned_held_pair_cycle(
        &fixture,
        &owner,
        &shared,
        primary,
        HeldPeerSession {
            step: 0,
            changes: Arc::new(AtomicUsize::new(0)),
        },
    )
    .await;
    assert!(
        primed.db_sync_token_advance_safe,
        "both production baselines must complete: {primed:?}"
    );
    seed_original_sparse_history(&mut fixture, &owner, PRIOR).await;
    let primary = fixture.session(true, Fault::None, CancellationToken::new());
    let peer_changes = Arc::new(AtomicUsize::new(0));
    let held = owned_held_pair_cycle(
        &fixture,
        &owner,
        &shared,
        primary.clone(),
        HeldPeerSession {
            step: 1,
            changes: peer_changes.clone(),
        },
    )
    .await;
    assert!(!held.db_sync_token_advance_safe);
    assert!(primary.retained.load(Ordering::SeqCst) > 0);
    assert!(
        peer_changes.load(Ordering::SeqCst) > 0,
        "healthy peer must run while the source becomes held"
    );
    let first_hold = {
        let db = state::SqliteStateDb::open_owned(&fixture.database, &owner)
            .await
            .unwrap();
        assert_eq!(
            db.get_metadata(&format!("sync_token:v27:{HELD_PEER}"))
                .await
                .unwrap()
                .as_deref(),
            Some("peer-after-1")
        );
        db.get_metadata(HOLD_KEY)
            .await
            .unwrap()
            .expect("primary hold")
    };
    let primary = fixture.session(true, Fault::None, CancellationToken::new());
    let peer_changes = Arc::new(AtomicUsize::new(0));
    let deferred = owned_held_pair_cycle(
        &fixture,
        &owner,
        &shared,
        primary.clone(),
        HeldPeerSession {
            step: 2,
            changes: peer_changes.clone(),
        },
    )
    .await;
    assert!(!deferred.db_sync_token_advance_safe);
    assert_eq!(primary.retained.load(Ordering::SeqCst), 0);
    assert_eq!(primary.queries.load(Ordering::SeqCst), 0);
    assert!(
        peer_changes.load(Ordering::SeqCst) > 0,
        "healthy peer must continue during backoff"
    );
    let db = state::SqliteStateDb::open_owned(&fixture.database, &owner)
        .await
        .unwrap();
    assert_eq!(
        db.get_metadata(CURSOR_KEY).await.unwrap().as_deref(),
        Some(PRIOR)
    );
    assert_eq!(
        db.get_metadata(HOLD_KEY).await.unwrap().as_deref(),
        Some(first_hold.as_str())
    );
    assert_eq!(
        db.get_metadata(&format!("sync_token:v27:{HELD_PEER}"))
            .await
            .unwrap()
            .as_deref(),
        Some("peer-after-2")
    );
    drop(db);
    assert_eq!(
        rows(
            &fixture.database,
            "SELECT * FROM unresolved_sparse_identities ORDER BY library,source_record_name"
        ),
        fixture.original_sparse
    );
    fixture.assert_preserved();
}

#[tokio::test]
async fn retained_checkpoint_expiry_holds_all_trigger_classes_and_quiet_reopen() {
    for trigger in [Trigger::Eligibility, Trigger::Sparse] {
        let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
        let config = make_run_cycle_config();
        let owner = run_cycle_account_owner();
        let fixture = if trigger == Trigger::Sparse {
            let mut fixture =
                Fixture::new_with_owner_and_priming(trigger, Some(&owner), true).await;
            {
                let db = Arc::new(
                    state::SqliteStateDb::open_owned(&fixture.database, &owner)
                        .await
                        .unwrap(),
                );
                let cancel = CancellationToken::new();
                let mut provider = fixture.session(false, Fault::None, cancel.clone());
                provider.priming = true;
                let mut source = retained_account_library(&db, &owner, Box::new(provider)).await;
                source.use_stub_indexing_for_test();
                let mut selected =
                    make_run_cycle_library_state_with_album(ZONE, CURSOR_KEY, source.all());
                selected.library = source;
                let builder = make_run_cycle_download_config_builder_with_options(
                    &fixture.media,
                    db.clone(),
                    RunCycleDownloadConfigOptions {
                        media: media_without_photo_downloads(),
                        ..Default::default()
                    },
                );
                let result = run_cycle(
                    &[&selected],
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
                    result.db_sync_token_advance_safe,
                    "owner-produced starting baseline: {result:?}"
                );
                assert_eq!(
                    db.get_metadata(CURSOR_KEY).await.unwrap().as_deref(),
                    Some(PRIOR)
                );
            }
            seed_original_sparse_history(&mut fixture, &owner, PRIOR).await;
            fixture
        } else {
            Fixture::new(trigger).await
        };
        let old_hash = {
            let db = state::SqliteStateDb::open(&fixture.database).await.unwrap();
            db.get_metadata(ENUM_CONFIG_HASH_KEY).await.unwrap()
        };
        let mut receipt = None;
        for cycle in 0..8 {
            let db = Arc::new(if trigger == Trigger::Sparse {
                state::SqliteStateDb::open_owned(&fixture.database, &owner)
                    .await
                    .unwrap()
            } else {
                state::SqliteStateDb::open(&fixture.database).await.unwrap()
            });
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
            let library = if trigger == Trigger::Sparse {
                let mut source =
                    retained_account_library(&db, &owner, Box::new(provider.clone())).await;
                source.use_stub_indexing_for_test();
                let mut selected =
                    make_run_cycle_library_state_with_album(ZONE, CURSOR_KEY, source.all());
                selected.library = source;
                provider.requests.store(0, Ordering::SeqCst);
                selected
            } else {
                let mut selected = make_run_cycle_library_state_with_album(
                    ZONE,
                    CURSOR_KEY,
                    make_full_album_with_boxed_session(ZONE, Box::new(provider.clone())),
                );
                selected.library = crate::icloud::photos::PhotoLibrary::new_stub_with_zone(
                    Box::new(provider.clone()),
                    ZONE,
                );
                selected
            };
            if cycle > 0 {
                let mut precheck = crate::sync_loop::precheck::WatchPrecheck::SkipAll;
                crate::sync_loop::precheck::include_pending_local_work(
                    &mut precheck,
                    db.as_ref(),
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
            let raw = db.get_metadata(HOLD_KEY).await.unwrap().unwrap_or_else(|| {
                panic!("expired retained replay must create a durable hold: {trigger:?} cycle {cycle}: {result:?}")
            });
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
async fn retained_checkpoint_historical_metadata_does_not_require_preparation() {
    let owner = run_cycle_account_owner();
    let fixture = Fixture::new_with_owner_and_priming(Trigger::Legacy, Some(&owner), true).await;
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    {
        // Produce the retained cursor with the same current provider inventory,
        // selection and options before testing historical metadata preservation.
        let db = Arc::new(
            state::SqliteStateDb::open_owned(&fixture.database, &owner)
                .await
                .unwrap(),
        );
        let cancel = CancellationToken::new();
        let mut provider = fixture.session(false, Fault::None, cancel.clone());
        provider.priming = true;
        let source = retained_account_library(&db, &owner, Box::new(provider)).await;
        let mut library = make_run_cycle_library_state_with_album(ZONE, CURSOR_KEY, source.all());
        library.library = source;
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
        assert!(
            result.db_sync_token_advance_safe,
            "starting historical baseline: {result:?}"
        );
        assert_eq!(
            db.get_metadata(CURSOR_KEY).await.unwrap().as_deref(),
            Some(PRIOR)
        );
    }
    fixture.assert_preserved();
    for cycle in 0..3 {
        let db = Arc::new(
            state::SqliteStateDb::open_owned(&fixture.database, &owner)
                .await
                .unwrap(),
        );
        let cancel = CancellationToken::new();
        let mut provider = fixture.session(true, Fault::None, cancel.clone());
        provider.priming = cycle == 0;
        provider.priming_successor = "fresh-rank-anchor";
        let source = retained_account_library(&db, &owner, Box::new(provider.clone())).await;
        let mut library = make_run_cycle_library_state_with_album(ZONE, CURSOR_KEY, source.all());
        library.library = source;
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
        assert_eq!(result.failed_count, 0);
        assert_eq!(result.stats.failed, 0);
        assert_eq!(result.stats.state_write_failures, 0);
        assert_eq!(result.stats.enumeration_errors, 0);
        assert!(!result.stats.enumeration_incomplete);
        assert_eq!(
            result.stats.assets_seen,
            if cycle == 0 { 2 } else { 0 },
            "cycle {cycle}: {result:?}"
        );
        assert_eq!(
            result.stats.skipped.by_media_type,
            if cycle == 0 { 2 } else { 0 }
        );
        assert_eq!(result.stats.metadata_capture_remaining, 1);
        assert_eq!(result.stats.metadata_capture_unresolved, 1);
        assert_eq!(result.stats.metadata_capture_deferred, 1);
        assert!(
            result.db_sync_token_advance_safe,
            "cycle {cycle}: {result:?}"
        );
        assert!(db.get_metadata(HOLD_KEY).await.unwrap().is_none());
        assert!(
            db.legacy_preservations(ZONE).await.unwrap().is_empty(),
            "ordinary progress must not fabricate historical preparation"
        );
        assert!(
            rows(
                &fixture.database,
                "SELECT * FROM unattributed_legacy_proofs"
            )
            .is_empty()
        );
        assert_eq!(
            db.get_metadata(CURSOR_KEY).await.unwrap().as_deref(),
            Some(if cycle == 0 {
                "fresh-rank-anchor"
            } else {
                "validated-old-replay"
            })
        );
        if cycle > 0 {
            assert_eq!(provider.queries.load(Ordering::SeqCst), 0);
            assert_eq!(provider.inventories.load(Ordering::SeqCst), 0);
            assert_eq!(provider.retained.load(Ordering::SeqCst), 0);
        }
        fixture.assert_preserved();
    }
}

#[tokio::test]
async fn retained_checkpoint_hold_valid_old_replay_recovers_then_two_quiet_reopens() {
    // This control repairs a previously rejected OLD cursor. A valid fresh
    // inventory alone never repairs permanent historical-token expiry.
    let owner = run_cycle_account_owner();
    let fixture =
        Fixture::new_with_owner_and_priming(Trigger::Eligibility, Some(&owner), true).await;
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let config = make_run_cycle_config();
    {
        // Establish the old cursor and old eligibility policy through a real
        // baseline. The requested config and selected pass below are unchanged.
        let db = Arc::new(
            state::SqliteStateDb::open_owned(&fixture.database, &owner)
                .await
                .unwrap(),
        );
        let cancel = CancellationToken::new();
        let mut provider = fixture.session(false, Fault::None, cancel.clone());
        provider.priming = true;
        let source = retained_account_library(&db, &owner, Box::new(provider)).await;
        let mut library = make_run_cycle_library_state_with_album(ZONE, CURSOR_KEY, source.all());
        library.library = source;
        let base = make_run_cycle_download_config_builder_with_options(
            &fixture.media,
            db.clone(),
            RunCycleDownloadConfigOptions {
                media: media_without_photo_downloads(),
                ..Default::default()
            },
        );
        let builder = |mode, exclude, groups, library| {
            let mut value = (*base(mode, exclude, groups, library)).clone();
            value.edited = true;
            Arc::new(value)
        };
        let mut old_config = make_run_cycle_config();
        old_config.photos.edited = true;
        let result = run_cycle(
            &[&library],
            &old_config,
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
            result.db_sync_token_advance_safe,
            "old eligibility baseline: {result:?}"
        );
        assert_eq!(
            db.get_metadata(CURSOR_KEY).await.unwrap().as_deref(),
            Some(PRIOR)
        );
        assert_ne!(
            download::compute_config_hash(&old_config),
            download::compute_config_hash(&config)
        );
    }
    for cycle in 0..6 {
        let db = Arc::new(
            state::SqliteStateDb::open_owned(&fixture.database, &owner)
                .await
                .unwrap(),
        );
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
        let source = retained_account_library(&db, &owner, Box::new(provider.clone())).await;
        let mut library = make_run_cycle_library_state_with_album(ZONE, CURSOR_KEY, source.all());
        library.library = source;
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
            if cycle == 2 {
                assert!(
                    !result.db_sync_token_advance_safe,
                    "observed delta awaits its catalog comparison: {result:?}"
                );
                assert_eq!(
                    result.stats.sync_token_blocked_reason,
                    Some("catalog_bootstrap_pending")
                );
            } else {
                assert!(
                    result.db_sync_token_advance_safe,
                    "cycle {cycle}: {result:?}"
                );
            }
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
        if cycle == 2 || cycle >= 4 {
            assert_eq!(provider.queries.load(Ordering::SeqCst), 0);
            assert_eq!(provider.inventories.load(Ordering::SeqCst), 0);
            assert_eq!(provider.retained.load(Ordering::SeqCst), 0);
        }
        if cycle == 3 {
            assert!(
                provider.queries.load(Ordering::SeqCst) > 0,
                "saved catalog boundary must be compared"
            );
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
                "CREATE TRIGGER retained_hold_fault BEFORE INSERT ON metadata WHEN NEW.key='retained_checkpoint_hold:v27:PrimarySync' BEGIN SELECT RAISE(ABORT,'injected hold write failure'); END;"
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
    priming: bool,
}
#[async_trait::async_trait]
impl PhotosSession for UnaffectedSession {
    async fn post(&self, url: &str, body: String, _: &[(&str, &str)]) -> anyhow::Result<Value> {
        let request: Value = serde_json::from_str(&body)?;
        if request["query"]["recordType"] == "CheckIndexingState" {
            return Ok(
                json!({"records":[{"recordName":"index-state","fields":{"state":{"value":"FINISHED"}}}]}),
            );
        }
        if url.ends_with("/zones/list") {
            return Ok(
                json!({"zones":[{"zoneID":{"zoneName":"SharedSync-CLEAN","ownerRecordName":"_defaultOwner"},"syncToken":"clean-zone-boundary","deleted":false}]}),
            );
        }
        if url.contains("/changes/zone?") {
            self.changes.fetch_add(1, Ordering::SeqCst);
            return Ok(json!({"zones":[{
                "zoneID":{"zoneName":"SharedSync-CLEAN","ownerRecordName":"_defaultOwner"},
                "syncToken":if self.priming {"clean-zone-before"} else {"clean-zone-successor"},"moreComing":false,"records":[]
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

async fn clean_peer_states(
    db: Arc<state::SqliteStateDb>,
    owner: &state::db::account::AccountOwner,
    primary: RetainedSession,
    peer: UnaffectedSession,
) -> (
    crate::sync_cycle::LibraryState,
    crate::sync_cycle::LibraryState,
) {
    let source = retained_account_library(&db, owner, Box::new(primary)).await;
    let mut other = crate::icloud::photos::PhotoLibrary::new(
        "https://example.com/database/1/photos/production/shared".into(),
        Arc::new(std::collections::HashMap::new()),
        Box::new(peer),
        Arc::new(json!({"zoneName":"SharedSync-CLEAN","ownerRecordName":"_defaultOwner"})),
        "shared".into(),
        crate::retry::RetryConfig::default(),
    )
    .await
    .unwrap();
    other.shadow_capture = Some(crate::icloud::photos::inbox::ShadowCapture::new(
        db,
        owner.clone(),
        "com",
    ));
    let mut first = make_run_cycle_library_state_with_album(ZONE, CURSOR_KEY, source.all());
    first.library = source;
    let mut second = make_run_cycle_library_state_with_album(
        "SharedSync-CLEAN",
        "sync_token:v27:SharedSync-CLEAN",
        other.all(),
    );
    second.library = other;
    (first, second)
}

#[tokio::test]
async fn retained_checkpoint_held_zone_does_not_block_clean_zone_or_clear_aggregate_hold() {
    let owner = run_cycle_account_owner();
    let mut fixture =
        Fixture::new_with_owner_and_priming(Trigger::Sparse, Some(&owner), true).await;
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    {
        let db = Arc::new(
            state::SqliteStateDb::open_owned(&fixture.database, &owner)
                .await
                .unwrap(),
        );
        let cancel = CancellationToken::new();
        let mut provider = fixture.session(false, Fault::None, cancel.clone());
        provider.priming = true;
        let (primary, clean) = clean_peer_states(
            db.clone(),
            &owner,
            provider,
            UnaffectedSession {
                changes: Arc::new(AtomicUsize::new(0)),
                priming: true,
            },
        )
        .await;
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
            result.db_sync_token_advance_safe,
            "both source baselines must complete: {result:?}"
        );
        assert_eq!(
            db.get_metadata(CURSOR_KEY).await.unwrap().as_deref(),
            Some(PRIOR)
        );
        assert_eq!(
            db.get_metadata("sync_token:v27:SharedSync-CLEAN")
                .await
                .unwrap()
                .as_deref(),
            Some("clean-zone-before")
        );
    }
    seed_original_sparse_history(&mut fixture, &owner, PRIOR).await;
    for cycle in 0..3 {
        let db = Arc::new(
            state::SqliteStateDb::open_owned(&fixture.database, &owner)
                .await
                .unwrap(),
        );
        let cancel = CancellationToken::new();
        let primary_provider = fixture.session(true, Fault::None, cancel.clone());
        let clean_changes = Arc::new(AtomicUsize::new(0));
        let (primary, clean) = clean_peer_states(
            db.clone(),
            &owner,
            primary_provider.clone(),
            UnaffectedSession {
                changes: clean_changes.clone(),
                priming: false,
            },
        )
        .await;
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
            db.get_metadata("sync_token:v27:SharedSync-CLEAN")
                .await
                .unwrap()
                .as_deref(),
            Some("clean-zone-successor"),
            "cycle {cycle}: {result:?}"
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
                "CREATE TRIGGER retained_retry_fault BEFORE UPDATE ON metadata WHEN NEW.key='retained_checkpoint_hold:v27:PrimarySync' BEGIN SELECT RAISE(ABORT,'injected retry reservation failure'); END;"
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
    inject_arrival: bool,
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
        if request["query"]["recordType"] == "CheckIndexingState" {
            return Ok(
                json!({"records":[{"recordName":"index-state","recordType":"CheckIndexingState","fields":{"state":{"value":"FINISHED"}}}]}),
            );
        }
        if url.contains("/records/lookup?") {
            let names = request["records"].as_array().unwrap();
            let records: Vec<Value> = self
                .source
                .lock()
                .unwrap()
                .iter()
                .filter(|record| {
                    names
                        .iter()
                        .any(|name| name["recordName"] == record["recordName"])
                })
                .cloned()
                .collect();
            return Ok(json!({"records":records}));
        }
        if url.contains("/records/query?") {
            self.base.queries.fetch_add(1, Ordering::SeqCst);
            let before = self.source.lock().unwrap().clone();
            // The provider fixture owns this independently specified event.
            // The successful retry inventory has already taken its empty snapshot
            // when this change arrives. Only its subsequent replay can admit it.
            if self.inject_arrival && !self.injected.swap(true, Ordering::SeqCst) {
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
    let mut admitted_specs = None;
    for cycle in 0..8 {
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
            inject_arrival: cycle == 3,
        };
        let capture =
            crate::icloud::photos::inbox::ShadowCapture::new(db.clone(), owner.clone(), "com");
        let mut authenticated_library = crate::icloud::photos::PhotoLibrary::new(
            "https://example.com/database/1/photos/production/private".into(),
            Arc::new(std::collections::HashMap::new()),
            Box::new(provider),
            Arc::new(json!({"zoneName":ZONE,"ownerRecordName":"_defaultOwner"})),
            "private".into(),
            crate::retry::RetryConfig::default(),
        )
        .await
        .unwrap();
        authenticated_library.shadow_capture = Some(capture);
        let mut library =
            make_run_cycle_library_state_with_album(ZONE, CURSOR_KEY, authenticated_library.all());
        library.library = authenticated_library;
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
        assert_eq!(injected.load(Ordering::SeqCst), cycle >= 3);
        assert_eq!(
            *source.lock().unwrap(),
            if cycle >= 3 {
                arrival.as_ref().clone()
            } else {
                Vec::new()
            },
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
            assert!(rows(&fixture.database, "SELECT child FROM provider_active_decisions WHERE child='asset-arrived-provider-master'").is_empty());
            assert!(
                db.get_master_record_name_for_asset(ZONE, "asset-arrived-provider-master")
                    .await
                    .unwrap()
                    .is_none()
            );
            assert!(!result.db_sync_token_advance_safe);
            assert_eq!(
                delivered.load(Ordering::SeqCst),
                0,
                "fresh rank inventory must not count as retained replay"
            );
        } else {
            if cycle == 4 {
                assert!(
                    !result.db_sync_token_advance_safe,
                    "the observed delta waits for the saved-boundary inventory and bridge: {result:?}"
                );
                assert_eq!(
                    result.stats.sync_token_blocked_reason,
                    Some("catalog_bootstrap_pending")
                );
            } else {
                assert!(
                    result.db_sync_token_advance_safe,
                    "cycle {cycle}: {result:?}"
                );
            }
            assert_eq!(result.failed_count, 0);
            assert_eq!(result.stats.skipped.uncertain, 0);
            assert_eq!(
                result.stats.assets_seen,
                usize::from(cycle == 3 || cycle == 5) as u64
            );
            assert_eq!(
                result.stats.skipped.by_media_type,
                usize::from(cycle == 3 || cycle == 5)
            );
            if cycle == 3 {
                let connection = rusqlite::Connection::open(&fixture.database).unwrap();
                let mut statement = connection.prepare(
                    "SELECT g.specification FROM provider_active_decisions d
                     JOIN provider_active_generations g ON g.id=d.generation
                     JOIN account_owner a ON a.account_key=g.account_key AND a.provider_key=g.provider_key
                     WHERE d.child='asset-arrived-provider-master'
                       AND d.outcome='excluded' AND d.admission='excluded'
                       AND g.sealed=1 AND g.checkpoint_ready=1 AND g.checkpoint_veto IS NULL
                       AND json_extract(g.scope,'$.zone.zoneName')='PrimarySync'
                       AND json_extract(g.scope,'$.zone.ownerRecordName')='_defaultOwner'
                       AND json_extract(g.scope,'$.database')='private'"
                ).unwrap();
                let encoded = statement
                    .query_map([], |row| row.get::<_, Vec<u8>>(0))
                    .unwrap()
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap();
                assert_eq!(
                    encoded.len(),
                    1,
                    "replay must account for the arrived child in its authenticated scope"
                );
                admitted_specs = Some(
                    encoded
                        .iter()
                        .map(|blob| serde_json::from_slice(blob).unwrap())
                        .collect::<Vec<state::db::provider_generations::GenerationSpec>>(),
                );
            }
            assert!(
                db.current_observation_matches(
                    owner.clone(),
                    CURSOR_KEY.into(),
                    admitted_specs.clone().unwrap()
                )
                .await
                .unwrap(),
                "reopen must retain valid cursor-bound selection evidence"
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
        if cycle == 1 || cycle == 4 || cycle >= 6 {
            assert_eq!(base.queries.load(Ordering::SeqCst), 0);
            assert_eq!(base.retained.load(Ordering::SeqCst), 0);
        }
        if cycle == 5 {
            assert!(
                base.queries.load(Ordering::SeqCst) > 0,
                "the saved boundary must run its own inventory"
            );
            assert_eq!(
                base.retained.load(Ordering::SeqCst),
                0,
                "the rejected old cursor must not be replayed"
            );
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
