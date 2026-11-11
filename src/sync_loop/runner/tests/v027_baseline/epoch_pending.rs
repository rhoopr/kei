//! Epoch hold acceptance: admitted generation work stays pending without
//! source enumeration while the epoch is deferred or promoting, then the
//! normal post-epoch path retries it exactly once.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use base64::Engine as _;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, Respond, ResponseTemplate};

use crate::commands::{AlbumPass, PassKind};
use crate::download;
use crate::icloud::photos::PhotoLibrary;
use crate::state::SqliteStateDb;
use crate::sync_cycle::{CycleResult, run_cycle, sync_token_key};
use crate::sync_loop::test_support::{
    full_album_page_with_download, make_run_cycle_config, make_run_cycle_download_config_builder,
    make_run_cycle_library_state_with_passes, make_shared_session_for_run_cycle,
};

use super::{
    FlakyBytes, MEDIA, ResetCycleSession, ZONE, baseline_owner, force_epoch_due,
    seed_existing_upgrade_db,
};

const OLD_ENUM_HASH: &str = "epoch-hold-old-enum";

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    db_path: PathBuf,
    _server: wiremock::MockServer,
    ok_calls: Arc<AtomicUsize>,
    fail_calls: Arc<AtomicUsize>,
    inventory: Vec<Value>,
    full_queries: Arc<AtomicUsize>,
    changes: Arc<AtomicUsize>,
    ok_lookups: Arc<AtomicUsize>,
    shared: crate::auth::session::SharedSession,
    fail_media: Vec<u8>,
    legacy_checksum: Option<String>,
    reset_boundary: AtomicBool,
}

#[derive(Clone)]
struct CountOkLookup {
    inner: ResetCycleSession,
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl crate::icloud::photos::PhotosSession for CountOkLookup {
    async fn post(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        if url.contains("/records/lookup?") {
            let request: Value = serde_json::from_str(&body)?;
            if request["records"].as_array().is_some_and(|records| {
                records
                    .iter()
                    .any(|record| record["recordName"] == "asset-ok")
            }) {
                self.calls.fetch_add(1, Ordering::SeqCst);
            }
        }
        crate::icloud::photos::PhotosSession::post(&self.inner, url, body, headers).await
    }

    fn clone_box(&self) -> Box<dyn crate::icloud::photos::PhotosSession> {
        Box::new(self.clone())
    }
}

struct BlockedDownload {
    started: Arc<Notify>,
    release: Arc<(Mutex<bool>, Condvar)>,
    calls: Arc<AtomicUsize>,
}

impl BlockedDownload {
    async fn started(&self) {
        self.started.notified().await;
    }

    fn release(&self) {
        let (released, changed) = &*self.release;
        *released.lock().unwrap() = true;
        changed.notify_all();
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl Drop for BlockedDownload {
    fn drop(&mut self) {
        self.release();
    }
}

struct BlockingOkDownload {
    started: Arc<Notify>,
    release: Arc<(Mutex<bool>, Condvar)>,
    calls: Arc<AtomicUsize>,
    bytes: Arc<Vec<u8>>,
}

impl Respond for BlockingOkDownload {
    fn respond(&self, _request: &wiremock::Request) -> ResponseTemplate {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.started.notify_one();
        let (released, changed) = &*self.release;
        let mut released = released.lock().unwrap();
        while !*released {
            released = changed.wait(released).unwrap();
        }
        drop(released);
        ResponseTemplate::new(200).set_body_bytes(self.bytes.as_slice())
    }
}

impl Fixture {
    async fn new(stage_ready: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("state.db");
        Self::new_at(stage_ready, dir, db_path, MEDIA.to_vec()).await
    }

    async fn new_with_ok_media(stage_ready: bool, ok_media: Vec<u8>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("state.db");
        Self::new_at(stage_ready, dir, db_path, ok_media).await
    }

    async fn new_cli(stage_ready: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db_path =
            crate::state::db::account::state_path(dir.path(), "baseline@example.invalid", "com")
                .await
                .unwrap();
        Self::new_at(stage_ready, dir, db_path, MEDIA.to_vec()).await
    }

    async fn new_at(
        stage_ready: bool,
        dir: tempfile::TempDir,
        db_path: PathBuf,
        ok_media: Vec<u8>,
    ) -> Self {
        let root = dir.path().join("photos");
        std::fs::create_dir_all(&root).unwrap();
        seed_existing_upgrade_db(&db_path);
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute(
                "INSERT OR REPLACE INTO metadata(key,value) VALUES(?1,'v36-token')",
                [format!("sync_token:{ZONE}")],
            )
            .unwrap();
            if stage_ready {
                conn.execute(
                    "INSERT OR REPLACE INTO metadata(key,value) VALUES('enum_config_hash',?1)",
                    [OLD_ENUM_HASH],
                )
                .unwrap();
            }
        }

        let ok_calls = Arc::new(AtomicUsize::new(0));
        let fail_calls = Arc::new(AtomicUsize::new(0));
        let fail_media = {
            let mut bytes = MEDIA.to_vec();
            bytes.extend_from_slice(b"epoch-hold-fail-payload");
            bytes
        };
        let server = wiremock::MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ok"))
            .respond_with(FlakyBytes {
                calls: Arc::clone(&ok_calls),
                bytes: Arc::new(ok_media.clone()),
                fail_first: 0,
            })
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/fail"))
            .respond_with(FlakyBytes {
                calls: Arc::clone(&fail_calls),
                bytes: Arc::new(fail_media.clone()),
                fail_first: 2,
            })
            .mount(&server)
            .await;

        let ok_checksum =
            base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&ok_media));
        let fail_checksum =
            base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&fail_media));
        let mut ok_page = full_album_page_with_download(
            ZONE,
            "ok",
            "inventory-last-page",
            &format!("{}/ok", server.uri()),
            ok_media.len() as u64,
            &ok_checksum,
        );
        let mut fail_page = full_album_page_with_download(
            ZONE,
            "fail",
            "inventory-last-page",
            &format!("{}/fail", server.uri()),
            fail_media.len() as u64,
            &fail_checksum,
        );
        ok_page["records"][0]["fields"]["filenameEnc"]["value"] =
            json!(base64::engine::general_purpose::STANDARD.encode("ok.JPG"));
        fail_page["records"][0]["fields"]["filenameEnc"]["value"] =
            json!(base64::engine::general_purpose::STANDARD.encode("fail.JPG"));
        ok_page["records"][0]["recordChangeTag"] = json!("ct-master-ok");
        ok_page["records"][1]["recordChangeTag"] = json!("ct-asset-ok");
        fail_page["records"][0]["recordChangeTag"] = json!("ct-master-fail");
        fail_page["records"][1]["recordChangeTag"] = json!("ct-asset-fail");
        for page in [&mut ok_page, &mut fail_page] {
            page["records"][1]["fields"]["timeZoneOffset"] =
                json!({"value": -28800, "type": "INT64"});
            page["records"][1]["fields"]["isFavorite"] = json!({"value": true});
        }
        let mut inventory = ok_page["records"].as_array().unwrap().clone();
        inventory.extend(fail_page["records"].as_array().unwrap().clone());

        let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
        Self {
            _dir: dir,
            root,
            db_path,
            _server: server,
            ok_calls,
            fail_calls,
            inventory,
            full_queries: Arc::new(AtomicUsize::new(0)),
            changes: Arc::new(AtomicUsize::new(0)),
            ok_lookups: Arc::new(AtomicUsize::new(0)),
            shared,
            fail_media,
            legacy_checksum: None,
            reset_boundary: AtomicBool::new(false),
        }
    }

    async fn block_ok_download(&self) -> BlockedDownload {
        let started = Arc::new(Notify::new());
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let calls = Arc::new(AtomicUsize::new(0));
        Mock::given(method("GET"))
            .and(path("/ok"))
            .respond_with(BlockingOkDownload {
                started: Arc::clone(&started),
                release: Arc::clone(&release),
                calls: Arc::clone(&calls),
                bytes: Arc::new(MEDIA.to_vec()),
            })
            .with_priority(1)
            .mount(&self._server)
            .await;
        BlockedDownload {
            started,
            release,
            calls,
        }
    }

    fn dir(&self) -> &std::path::Path {
        self._dir.path()
    }

    async fn add_legacy_asset(&mut self) -> Arc<AtomicUsize> {
        let media = {
            let mut bytes = MEDIA.to_vec();
            bytes.extend_from_slice(b"legacy-retry-payload");
            bytes
        };
        let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&media));
        let calls = Arc::new(AtomicUsize::new(0));
        Mock::given(method("GET"))
            .and(path("/legacy"))
            .respond_with(FlakyBytes {
                calls: Arc::clone(&calls),
                bytes: Arc::new(media.clone()),
                fail_first: 0,
            })
            .mount(&self._server)
            .await;
        let mut page = full_album_page_with_download(
            ZONE,
            "legacy",
            "inventory-last-page",
            &format!("{}/legacy", self._server.uri()),
            media.len() as u64,
            &checksum,
        );
        page["records"][0]["fields"]["filenameEnc"]["value"] =
            json!(base64::engine::general_purpose::STANDARD.encode("legacy.JPG"));
        page["records"][0]["recordChangeTag"] = json!("ct-master-legacy");
        page["records"][1]["recordChangeTag"] = json!("ct-asset-legacy");
        self.inventory
            .extend(page["records"].as_array().unwrap().clone());
        self.legacy_checksum = Some(checksum);
        calls
    }

    async fn open(&self) -> Arc<SqliteStateDb> {
        Arc::new(
            SqliteStateDb::open_owned(&self.db_path, &baseline_owner())
                .await
                .unwrap(),
        )
    }

    async fn run(&self, retry_failed: bool, cancel: bool) -> CycleResult {
        self.run_with_options(retry_failed, cancel, false).await
    }

    async fn run_with_repair(&self, retry_failed: bool) -> CycleResult {
        self.run_with_options(retry_failed, false, true).await
    }

    async fn run_with_options(
        &self,
        retry_failed: bool,
        cancel: bool,
        legal_repair: bool,
    ) -> CycleResult {
        self.run_with_options_and_passes(retry_failed, cancel, legal_repair, true)
            .await
    }

    async fn run_with_options_and_passes(
        &self,
        retry_failed: bool,
        cancel: bool,
        legal_repair: bool,
        include_pass: bool,
    ) -> CycleResult {
        self.run_with_inventory_failure(retry_failed, cancel, legal_repair, include_pass, false)
            .await
    }

    async fn run_with_inventory_failure(
        &self,
        retry_failed: bool,
        cancel: bool,
        legal_repair: bool,
        include_pass: bool,
        fail_full: bool,
    ) -> CycleResult {
        use crate::commands::{
            build_collection_context, pass_scope_for_zone, resolve_passes_for_scope, zone_name_set,
        };
        let db = self.open().await;
        let session = ResetCycleSession {
            zone: ZONE,
            inventory: Arc::new(self.inventory.clone()),
            full_queries: Arc::clone(&self.full_queries),
            changes: Arc::clone(&self.changes),
            fail_full,
            auth_expired: false,
            boundary: if self.reset_boundary.load(Ordering::SeqCst) {
                "reset-fresh-boundary"
            } else {
                "inventory-last-page"
            },
        };
        let mut library = PhotoLibrary::new(
            "https://synthetic.example/database/1/photos/production/private".into(),
            Arc::new(std::collections::HashMap::new()),
            Box::new(CountOkLookup {
                inner: session,
                calls: Arc::clone(&self.ok_lookups),
            }),
            Arc::new(super::zone()),
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
        let mut run_config = make_run_cycle_config();
        run_config.filters.selection.albums = crate::selection::AlbumSelector::None;
        run_config.filters.selection.unfiled = true;
        let passes = if include_pass {
            let selection = &run_config.filters.selection;
            let libraries = std::slice::from_ref(&library);
            let zones = zone_name_set(libraries);
            let context = build_collection_context(selection, libraries)
                .await
                .unwrap();
            let scope = pass_scope_for_zone(selection, ZONE, &zones, &zones);
            resolve_passes_for_scope(&library, selection, scope, &context, &[])
                .await
                .unwrap()
                .passes
        } else {
            Vec::new()
        };
        let mut selected =
            make_run_cycle_library_state_with_passes(ZONE, &sync_token_key(ZONE), passes);
        selected.library = library;
        let base = make_run_cycle_download_config_builder(&self.root, db.clone());
        let builder = move |mode, exclude, groupings, library| {
            let mut config = (*base(mode, exclude, groupings, library)).clone();
            if legal_repair {
                config.refresh_metadata = true;
                config.capture_timestamp_repair =
                    crate::download::CaptureTimestampRepair::ReplaceWithCaptureLocal;
                config.metadata.set_exif_datetime = true;
            }
            Arc::new(config)
        };
        let token = CancellationToken::new();
        if cancel {
            token.cancel();
        }
        if legal_repair {
            run_config.runtime.refresh_metadata = true;
            run_config.runtime.repair_capture_timestamps = true;
            run_config.metadata.set_exif_datetime = true;
        }
        let result = run_cycle(
            &[&selected],
            &run_config,
            Some(db.as_ref()),
            retry_failed,
            &builder,
            download::DownloadControls::download_hidden(),
            &self.shared,
            &token,
        )
        .await
        .expect("epoch hold cycle must return");
        drop(db);
        result
    }

    async fn install_completion_abort(&self) {
        let db = self.open().await;
        let conn = db.acquire_lock("install epoch completion abort").unwrap();
        conn.execute_batch(
            "CREATE TRIGGER fail_epoch_complete BEFORE UPDATE ON metadata \
             WHEN NEW.key LIKE 'sync_epoch_library:v1:%' \
             AND json_valid(NEW.value) \
             AND json_extract(NEW.value,'$.state') = 'complete' \
             BEGIN SELECT RAISE(ABORT,'synthetic epoch completion abort'); END;",
        )
        .unwrap();
    }

    async fn install_promotion_abort(&self) {
        let db = self.open().await;
        let conn = db.acquire_lock("install epoch promotion abort").unwrap();
        conn.execute_batch(
            "CREATE TRIGGER fail_epoch_promotion_clear BEFORE DELETE ON metadata \
             WHEN OLD.key LIKE 'baseline_cursor:v27:%' \
             BEGIN SELECT RAISE(ABORT,'synthetic epoch promotion abort'); END;",
        )
        .unwrap();
    }

    async fn drop_trigger(&self, name: &str) {
        let db = self.open().await;
        let conn = db.acquire_lock("drop epoch fixture trigger").unwrap();
        conn.execute(&format!("DROP TRIGGER IF EXISTS {name}"), [])
            .unwrap();
    }

    async fn force_due(&self) {
        let db = self.open().await;
        let conn = db.acquire_lock("force epoch due").unwrap();
        force_epoch_due(&conn, ZONE);
    }

    async fn make_failed_due(&self) {
        let db = self.open().await;
        db.make_selection_retry_due(baseline_owner(), "asset-fail".to_owned())
            .await
            .unwrap();
    }

    async fn snapshot(&self) -> Snapshot {
        let db = self.open().await;
        let conn = db.acquire_lock("snapshot epoch hold").unwrap();
        let record = crate::state::db::sync_epoch::read_library(&conn, ZONE)
            .unwrap()
            .unwrap();
        let active: Option<String> = conn
            .query_row(
                "SELECT value FROM metadata WHERE key=?1",
                [sync_token_key(ZONE)],
                |row| row.get(0),
            )
            .ok();
        let pending_token: Option<String> = conn
            .query_row(
                "SELECT value FROM metadata WHERE key LIKE 'pending_sync_token:v27:%' LIMIT 1",
                [],
                |row| row.get(0),
            )
            .ok();
        let generation: Vec<String> = conn
            .prepare("SELECT id,config_hash,specification_hash,sealed,checkpoint_ready,created_at FROM provider_active_generations ORDER BY id")
            .unwrap()
            .query_map([], |row| {
                Ok(format!(
                    "{}|{}|{}|{}|{}|{}",
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        let destination: Vec<String> = conn
            .prepare("SELECT generation,path,checksum,size_bytes,admitted,verified_media,verified_metadata,intent_hash,source_checksum FROM provider_active_destinations WHERE child='asset-fail' ORDER BY generation,path")
            .unwrap()
            .query_map([], |row| {
                Ok(format!(
                    "{}|{}|{}|{}|{}|{}|{}|{:?}|{:?}",
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, Option<String>>(8)?
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        let decision: Vec<String> = conn
            .prepare("SELECT manifest,manifest_hash,admission,reason,admission_hash,attempts,next_retry_at FROM provider_active_decisions WHERE child='asset-fail' ORDER BY generation,pass_key")
            .unwrap()
            .query_map([], |row| {
                let manifest = row.get::<_, Vec<u8>>(0)?;
                Ok(format!(
                    "{manifest:?}|{}|{}|{}|{}|{}|{}",
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        let ok_receipt: Option<(String, String)> = conn
            .query_row(
                "SELECT local_path,local_checksum FROM asset_metadata_paths WHERE id='asset-ok' AND version_size='original'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .ok();
        let fail_receipt: Option<(String, String)> = conn
            .query_row(
                "SELECT local_path,local_checksum FROM asset_metadata_paths WHERE id='asset-fail' AND version_size='original'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .ok();
        let binding_value: Option<String> = record
            .binding
            .as_ref()
            .map(|binding| serde_json::to_string(binding).unwrap());
        let binding_present = record.binding.is_some();
        let failed_asset_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM assets WHERE id='asset-fail'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let (failed_asset_status, failed_asset_local_path): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT status,local_path FROM assets WHERE id='asset-fail'",
                [],
                |row| Ok((Some(row.get(0)?), row.get(1)?)),
            )
            .unwrap_or((None, None));
        Snapshot {
            state: record.state.clone(),
            attempt: record.attempt.clone(),
            next_attempt_at: record.next_attempt_at,
            binding_present,
            binding_value,
            active_token: active,
            pending_token,
            generation,
            destination,
            decision,
            ok_receipt,
            fail_receipt,
            failed_asset_rows,
            failed_asset_status,
            failed_asset_local_path,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    state: String,
    attempt: String,
    next_attempt_at: i64,
    binding_present: bool,
    binding_value: Option<String>,
    active_token: Option<String>,
    pending_token: Option<String>,
    generation: Vec<String>,
    destination: Vec<String>,
    decision: Vec<String>,
    ok_receipt: Option<(String, String)>,
    fail_receipt: Option<(String, String)>,
    failed_asset_rows: i64,
    failed_asset_status: Option<String>,
    failed_asset_local_path: Option<String>,
}

#[tokio::test]
async fn epoch_defer_keeps_admitted_failed_destination_pending() {
    let fixture = Fixture::new(false).await;
    fixture.install_completion_abort().await;
    let baseline = fixture.run(false, false).await;
    assert_eq!(baseline.stats.downloaded, 1, "{baseline:?}");
    assert_eq!(baseline.stats.failed, 1, "{baseline:?}");
    assert_eq!(baseline.stats.state_write_failures, 1, "{baseline:?}");
    fixture.drop_trigger("fail_epoch_complete").await;

    let before = fixture.snapshot().await;
    assert_eq!(before.state, "pending");
    assert!(before.next_attempt_at > chrono::Utc::now().timestamp());
    assert!(!before.binding_present);
    assert!(
        before.active_token.is_none(),
        "the deferred hold must not activate a token"
    );
    assert!(
        before.pending_token.is_none(),
        "the aborted stage must not leave a staged token"
    );
    let peer_path = before
        .ok_receipt
        .as_ref()
        .expect("the healthy peer must retain its real receipt")
        .0
        .clone();
    assert_eq!(std::fs::read(&peer_path).unwrap(), MEDIA);
    let db = fixture.open().await;
    assert!(db.sync_epoch_has_pending(ZONE.to_owned()).await.unwrap());
    assert!(db.sync_epoch_backoff(ZONE.to_owned()).await.unwrap());
    drop(db);
    assert_eq!(
        before.failed_asset_rows, 1,
        "the failed asset row is the production retry owner, not a synthesized row"
    );
    assert!(
        matches!(
            before.failed_asset_status.as_deref(),
            Some("failed" | "pending")
        ),
        "the failed row must remain visibly unresolved: {:?}",
        before.failed_asset_status
    );
    assert!(
        before.failed_asset_local_path.is_none(),
        "the failed row must not claim a local path before the retry"
    );
    assert!(
        before.decision.iter().any(|row| row.contains("|admitted|")),
        "the failed destination must remain admitted: {:?}",
        before.decision
    );
    assert!(
        before.destination.iter().any(|row| row.contains("|1|0|0|")),
        "the failed destination must remain admitted and unverified: {:?}",
        before.destination
    );

    let full_before = fixture.full_queries.load(Ordering::SeqCst);
    let changes_before = fixture.changes.load(Ordering::SeqCst);
    let fail_before = fixture.fail_calls.load(Ordering::SeqCst);
    for _ in 0..2 {
        let deferred = fixture.run(true, false).await;
        assert_eq!(deferred.failed_count, 0, "{deferred:?}");
        let during = fixture.snapshot().await;
        assert_eq!(during.state, "pending");
        assert_eq!(during.attempt, before.attempt);
        assert_eq!(during.next_attempt_at, before.next_attempt_at);
        assert_eq!(during.binding_present, before.binding_present);
        assert_eq!(during.active_token, before.active_token);
        assert_eq!(during.generation, before.generation);
        assert_eq!(during.destination, before.destination);
        assert_eq!(during.decision, before.decision);
        assert_eq!(during.ok_receipt, before.ok_receipt);
        assert_eq!(during.fail_receipt, before.fail_receipt);
    }
    let canceled = fixture.run(true, true).await;
    assert_eq!(canceled.failed_count, 0, "{canceled:?}");
    assert_eq!(
        fixture.snapshot().await,
        Snapshot {
            state: before.state.clone(),
            attempt: before.attempt.clone(),
            next_attempt_at: before.next_attempt_at,
            binding_present: before.binding_present,
            binding_value: before.binding_value.clone(),
            active_token: before.active_token.clone(),
            pending_token: before.pending_token.clone(),
            generation: before.generation.clone(),
            destination: before.destination.clone(),
            decision: before.decision.clone(),
            ok_receipt: before.ok_receipt.clone(),
            fail_receipt: before.fail_receipt.clone(),
            failed_asset_rows: before.failed_asset_rows,
            failed_asset_status: before.failed_asset_status.clone(),
            failed_asset_local_path: before.failed_asset_local_path.clone(),
        },
        "canceled retry-failed hold must not alter epoch or ledger state"
    );
    assert_eq!(fixture.full_queries.load(Ordering::SeqCst), full_before);
    assert!(fixture.changes.load(Ordering::SeqCst) >= changes_before);
    assert_eq!(fixture.fail_calls.load(Ordering::SeqCst), fail_before);

    fixture.force_due().await;
    let completed = fixture.run(false, false).await;
    assert_eq!(completed.failed_count, 0, "{completed:?}");
    let after = fixture.snapshot().await;
    assert_eq!(after.state, "complete", "{completed:?}");
    assert!(!after.binding_present);
    assert!(after.active_token.is_some());
    assert_ne!(after.active_token, before.active_token);
    assert_eq!(after.ok_receipt, before.ok_receipt);
    assert_eq!(
        fixture.fail_calls.load(Ordering::SeqCst),
        3,
        "the first normal full replay after the hold must retry the admitted failed GET once"
    );
    let fail_receipt = after.fail_receipt.clone().expect("failed receipt");
    assert_eq!(std::fs::read(&fail_receipt.0).unwrap(), fixture.fail_media);
    assert_ne!(fail_receipt.0, after.ok_receipt.as_ref().unwrap().0);
    assert_eq!(after.failed_asset_rows, 1);
    assert_eq!(after.failed_asset_status.as_deref(), Some("downloaded"));
    assert_eq!(
        after.failed_asset_local_path.as_deref(),
        Some(fail_receipt.0.as_str())
    );
    assert_eq!(fixture.ok_calls.load(Ordering::SeqCst), 1);
    let full_after_retry = fixture.full_queries.load(Ordering::SeqCst);
    let quiet = fixture.run(false, false).await;
    assert_eq!(quiet.failed_count, 0, "{quiet:?}");
    assert_eq!(fixture.fail_calls.load(Ordering::SeqCst), 3);
    assert_eq!(
        fixture.full_queries.load(Ordering::SeqCst),
        full_after_retry
    );
}

#[tokio::test]
async fn epoch_promote_holds_admitted_failure_then_normal_path_retries_once() {
    let fixture = Fixture::new(true).await;
    fixture.install_promotion_abort().await;
    let baseline = fixture.run(false, false).await;
    assert_eq!(baseline.failed_count, 1, "{baseline:?}");
    fixture.drop_trigger("fail_epoch_promotion_clear").await;

    let before = fixture.snapshot().await;
    assert_eq!(before.state, "ready");
    assert!(before.binding_present);
    assert!(before.pending_token.is_some());
    assert_ne!(before.active_token, before.pending_token);
    let peer_path = before
        .ok_receipt
        .as_ref()
        .expect("the healthy peer must retain its real receipt")
        .0
        .clone();
    assert_eq!(std::fs::read(&peer_path).unwrap(), MEDIA);
    let db = fixture.open().await;
    assert!(db.sync_epoch_has_pending(ZONE.to_owned()).await.unwrap());
    assert!(matches!(
        db.sync_epoch_gate(ZONE.to_owned(), "scope".to_owned(), None)
            .await
            .unwrap(),
        crate::state::SyncEpochGate::Promote { .. }
    ));
    drop(db);
    assert!(before.decision.iter().any(|row| row.contains("|admitted|")));
    assert!(
        before.destination.iter().any(|row| row.contains("|1|0|0|")),
        "{:?}",
        before.destination
    );
    let full_before = fixture.full_queries.load(Ordering::SeqCst);
    let fail_before = fixture.fail_calls.load(Ordering::SeqCst);

    let promoted = fixture.run(false, false).await;
    assert_eq!(promoted.failed_count, 0, "{promoted:?}");
    assert_eq!(fixture.full_queries.load(Ordering::SeqCst), full_before);
    assert_eq!(fixture.fail_calls.load(Ordering::SeqCst), fail_before);
    let after = fixture.snapshot().await;
    assert_eq!(after.state, "complete");
    assert!(!after.binding_present);
    assert_eq!(after.active_token, before.pending_token);
    assert_eq!(after.ok_receipt, before.ok_receipt);

    assert_eq!(fixture.fail_calls.load(Ordering::SeqCst), fail_before);
    fixture.make_failed_due().await;
    let retried = fixture.run(false, false).await;
    assert_eq!(retried.failed_count, 0, "{retried:?}");
    assert_eq!(fixture.fail_calls.load(Ordering::SeqCst), 3);
    let final_state = fixture.snapshot().await;
    let fail_receipt = final_state.fail_receipt.expect("failed receipt");
    assert_eq!(std::fs::read(&fail_receipt.0).unwrap(), fixture.fail_media);
    assert_ne!(fail_receipt.0, final_state.ok_receipt.as_ref().unwrap().0);
    assert_eq!(final_state.failed_asset_rows, 1);
    assert_eq!(
        final_state.failed_asset_status.as_deref(),
        Some("downloaded")
    );
    assert_eq!(
        final_state.failed_asset_local_path.as_deref(),
        Some(fail_receipt.0.as_str())
    );
    let full_after_retry = fixture.full_queries.load(Ordering::SeqCst);
    let quiet = fixture.run(false, false).await;
    assert_eq!(quiet.failed_count, 0, "{quiet:?}");
    assert_eq!(fixture.fail_calls.load(Ordering::SeqCst), 3);
    assert_eq!(
        fixture.full_queries.load(Ordering::SeqCst),
        full_after_retry
    );
    assert_eq!(fixture.ok_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn reset_sync_token_ready_epoch_atomically_rotates_and_preserves_evidence() {
    use crate::config::GlobalArgs;
    use crate::types::Domain;

    let fixture = Fixture::new_cli(true).await;
    fixture.install_promotion_abort().await;
    let baseline = fixture.run(false, false).await;
    assert_eq!(baseline.stats.downloaded, 1, "{baseline:?}");
    fixture.drop_trigger("fail_epoch_promotion_clear").await;

    let before = fixture.snapshot().await;
    assert_eq!(before.state, "ready");
    let old_attempt = before.attempt.clone();
    let old_binding = before.binding_value.clone().expect("staged binding");
    let old_pending = before.pending_token.clone().expect("staged token");
    let old_baseline = {
        let db = fixture.open().await;
        let conn = db.acquire_lock("read old baseline key").unwrap();
        let record = crate::state::db::sync_epoch::read_library(&conn, ZONE)
            .unwrap()
            .unwrap();
        record.binding.expect("staged binding").baseline_key
    };
    let full_before = fixture.full_queries.load(Ordering::SeqCst);
    let fail_before = fixture.fail_calls.load(Ordering::SeqCst);

    let globals = GlobalArgs {
        username: Some("baseline@example.invalid".to_owned()),
        domain: Some(Domain::Com),
        data_dir: Some(fixture.dir().to_string_lossy().into_owned()),
    };
    crate::commands::run_reset_sync_token(true, &globals, None, crate::InputMode::NoInput)
        .await
        .expect("production reset sync-token path");

    let after_reset = fixture.snapshot().await;
    assert_eq!(after_reset.state, "pending");
    assert_ne!(after_reset.attempt, old_attempt);
    assert!(after_reset.next_attempt_at <= chrono::Utc::now().timestamp());
    assert!(!after_reset.binding_present);
    assert!(after_reset.active_token.is_none());
    assert!(after_reset.pending_token.is_none());
    assert_eq!(after_reset.ok_receipt, before.ok_receipt);
    assert_eq!(after_reset.generation, before.generation);
    assert_eq!(after_reset.destination, before.destination);
    assert_eq!(after_reset.decision, before.decision);
    assert_eq!(after_reset.failed_asset_status, before.failed_asset_status);
    assert_eq!(
        after_reset.failed_asset_local_path,
        before.failed_asset_local_path
    );
    assert!(after_reset.fail_receipt.is_none());

    {
        let db = fixture.open().await;
        let conn = db
            .acquire_lock("inspect quarantined epoch binding")
            .unwrap();
        let quarantined: Option<String> = conn
            .query_row(
                "SELECT value FROM metadata WHERE key LIKE 'source_revalidation_reset_proof_quarantine:v1:%' LIMIT 1",
                [],
                |row| row.get(0),
            )
            .ok();
        assert_eq!(quarantined.as_deref(), Some(old_binding.as_str()));
        let descriptor_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM metadata WHERE key LIKE 'observed_selection_staged_quarantine:v1:%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            descriptor_count >= 1,
            "quarantined descriptor evidence missing"
        );
        let old_baseline_present: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM metadata WHERE key=?1)",
                [old_baseline.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            !old_baseline_present,
            "reset must not retain the old boundary"
        );
    }

    let completed = fixture.run(false, false).await;
    assert_eq!(completed.failed_count, 0, "{completed:?}");
    assert_eq!(
        fixture.full_queries.load(Ordering::SeqCst),
        full_before + 1,
        "reset must force a fresh full baseline"
    );
    let retried_during_full = fixture.fail_calls.load(Ordering::SeqCst) > fail_before;
    assert!(
        fixture.fail_calls.load(Ordering::SeqCst) <= fail_before + 1,
        "the fresh baseline may retry the admitted failed GET once, never duplicate it"
    );
    let after = fixture.snapshot().await;
    assert_eq!(after.state, "complete");
    assert!(!after.binding_present);
    assert_ne!(after.active_token.as_deref(), Some(old_pending.as_str()));
    assert_eq!(after.ok_receipt, before.ok_receipt);
    if retried_during_full {
        assert_eq!(after.failed_asset_status.as_deref(), Some("downloaded"));
        let receipt = after.fail_receipt.as_ref().expect("own receipt");
        assert_eq!(std::fs::read(&receipt.0).unwrap(), fixture.fail_media);
    } else {
        assert!(matches!(
            after.failed_asset_status.as_deref(),
            Some("failed" | "pending")
        ));
        assert!(after.fail_receipt.is_none());
    }

    fixture.make_failed_due().await;
    let retried = fixture.run(false, false).await;
    assert_eq!(retried.failed_count, 0, "{retried:?}");
    let final_state = fixture.snapshot().await;
    let final_receipt = final_state.fail_receipt.expect("own receipt");
    assert_eq!(std::fs::read(&final_receipt.0).unwrap(), fixture.fail_media);
    let quiet = fixture.run(false, false).await;
    assert_eq!(quiet.failed_count, 0, "{quiet:?}");
    assert_eq!(fixture.fail_calls.load(Ordering::SeqCst), fail_before + 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn epoch_baseline_refuses_staging_after_reset_rotates_attempt() {
    use crate::config::GlobalArgs;
    use crate::types::Domain;

    let mut fixture = Fixture::new_cli(false).await;
    // One media GET keeps the reset free of concurrent download writes while
    // still exercising the real production cycle.
    fixture
        .inventory
        .retain(|record| matches!(record["recordName"].as_str(), Some("ok" | "asset-ok")));
    let blocked = fixture.block_ok_download().await;
    let cycle = fixture.run(false, false);
    tokio::pin!(cycle);
    tokio::select! {
        () = blocked.started() => {}
        result = &mut cycle => panic!("cycle finished before the reset: {result:?}"),
    }
    let inflight_attempt = {
        let db = fixture.open().await;
        let conn = db.acquire_lock("read inflight epoch attempt").unwrap();
        crate::state::db::sync_epoch::read_library(&conn, ZONE)
            .unwrap()
            .unwrap()
            .attempt
    };
    let old_census = {
        let db = fixture.open().await;
        let conn = db
            .acquire_lock("read reset in-flight catalog owner")
            .unwrap();
        let record = crate::state::db::sync_epoch::read_library(&conn, ZONE)
            .unwrap()
            .unwrap();
        let (id, scope, policy, boundary, phase): (String, String, String, String, String) = conn.query_row(
            "SELECT id,scope,enum_hash,boundary,phase FROM source_revalidation_rounds WHERE library=?1 AND mode='catalog_v2' AND phase='observing'",
            [ZONE], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?)),
        ).unwrap();
        let origin = crate::state::db::source_revalidation::CatalogOrigin::parse(&scope).unwrap();
        assert_eq!(origin.nonce, id);
        assert_eq!(origin.epoch, record.epoch);
        assert_eq!(origin.attempt, inflight_attempt);
        let debt: Vec<(String, i64)> = conn
            .prepare(
                "SELECT id,next_retry_at FROM source_revalidation_debt WHERE round=?1 ORDER BY id",
            )
            .unwrap()
            .query_map([&id], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        (id, scope, policy, boundary, phase, debt)
    };
    let globals = GlobalArgs {
        username: Some("baseline@example.invalid".to_owned()),
        domain: Some(Domain::Com),
        data_dir: Some(fixture.dir().to_string_lossy().into_owned()),
    };
    crate::commands::run_reset_sync_token(true, &globals, None, crate::InputMode::NoInput)
        .await
        .expect("reset while the epoch baseline is in flight");
    blocked.release();
    let _old_cycle = cycle.await;

    let after = fixture.snapshot().await;
    {
        let db = fixture.open().await;
        let conn = db
            .acquire_lock("inspect reset retained catalog owner")
            .unwrap();
        let saved: (String, String, String, String, String) = conn.query_row(
            "SELECT id,scope,enum_hash,boundary,phase FROM source_revalidation_rounds WHERE id=?1 AND library=?2",
            rusqlite::params![old_census.0, ZONE], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?)),
        ).unwrap();
        assert_eq!(
            saved,
            (
                old_census.0.clone(),
                old_census.1.clone(),
                old_census.2.clone(),
                old_census.3.clone(),
                "abandoned".to_owned()
            )
        );
        let debt: Vec<(String, i64)> = conn
            .prepare(
                "SELECT id,next_retry_at FROM source_revalidation_debt WHERE round=?1 ORDER BY id",
            )
            .unwrap()
            .query_map([&old_census.0], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(debt, old_census.5);
        let (new_id, new_scope, phase): (String, String, String) = conn.query_row(
            "SELECT id,scope,phase FROM source_revalidation_rounds WHERE library=?1 AND mode='catalog_v2' AND phase=?2 AND boundary=?3",
            rusqlite::params![ZONE,old_census.4,old_census.3], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
        ).unwrap();
        let origin =
            crate::state::db::source_revalidation::CatalogOrigin::parse(&new_scope).unwrap();
        assert_eq!(origin.nonce, new_id);
        assert_eq!(origin.attempt, after.attempt);
        assert_eq!(phase, old_census.4);
        let copied: Vec<(String, i64)> = conn
            .prepare(
                "SELECT id,next_retry_at FROM source_revalidation_debt WHERE round=?1 ORDER BY id",
            )
            .unwrap()
            .query_map([new_id], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(copied, debt);
        eprintln!("RESET_OLD_CENSUS_PRESERVED debt_rows={}", debt.len());
    }
    assert_ne!(
        after.attempt, inflight_attempt,
        "the reset must rotate the in-flight attempt"
    );
    assert_eq!(after.state, "pending");
    assert!(!after.binding_present);
    assert!(
        after.active_token.is_none(),
        "the older baseline must not restore the active checkpoint cleared by reset"
    );
    assert!(
        after.pending_token.is_none(),
        "the older baseline must not leave a staged checkpoint"
    );
    let local_receipt = after
        .ok_receipt
        .clone()
        .expect("the completed local download keeps its receipt");
    assert_eq!(std::fs::read(&local_receipt.0).unwrap(), MEDIA);
    let full_after_old = fixture.full_queries.load(Ordering::SeqCst);
    assert!(
        full_after_old > 0,
        "the refused cycle must have run its full inventory"
    );

    fixture.force_due().await;
    fixture.reset_boundary.store(true, Ordering::SeqCst);
    let interrupted = fixture
        .run_with_inventory_failure(false, false, false, true, true)
        .await;
    assert!(!interrupted.db_sync_token_advance_safe, "{interrupted:?}");
    {
        let db = fixture.open().await;
        let conn = db
            .acquire_lock("inspect reset fresh boundary after reopen")
            .unwrap();
        let record = crate::state::db::sync_epoch::read_library(&conn, ZONE)
            .unwrap()
            .unwrap();
        assert_eq!(record.state, "pending");
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM metadata WHERE key LIKE 'baseline_cursor:v27:%' AND value='reset-fresh-boundary'", [], |row| row.get(0)).unwrap();
        assert_eq!(
            count, 1,
            "the new owner must persist the fresh provider boundary"
        );
        let scopes: Vec<String> = conn.prepare("SELECT scope FROM source_revalidation_rounds WHERE library=?1 AND mode='catalog_v2' AND phase='observing' AND boundary=?2").unwrap().query_map(rusqlite::params![ZONE,crate::state::db::source_revalidation::CatalogOrigin::boundary("reset-fresh-boundary")], |row| row.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
        assert!(scopes.iter().any(|scope| {
            crate::state::db::source_revalidation::CatalogOrigin::parse(scope).is_ok_and(|origin| {
                origin.epoch == record.epoch && origin.attempt == record.attempt
            })
        }));
    }
    fixture.force_due().await;
    let fresh = fixture.run(false, false).await;
    let done = fixture.snapshot().await;
    assert_eq!(done.state, "complete", "{fresh:?}");
    assert!(!done.binding_present);
    assert!(done.active_token.is_some());
    assert_eq!(
        done.ok_receipt.as_ref(),
        Some(&local_receipt),
        "the fresh attempt must retain the earlier local proof"
    );
    assert_eq!(std::fs::read(&local_receipt.0).unwrap(), MEDIA);
    let full_after_fresh = fixture.full_queries.load(Ordering::SeqCst);
    assert!(
        full_after_fresh > full_after_old,
        "the fresh due attempt must run a new full baseline"
    );
    assert!(blocked.calls() >= 1);
}

#[tokio::test]
async fn stale_proof_baseline_precedes_pending_zone_token() {
    let fixture = Fixture::new(true).await;
    fixture.install_promotion_abort().await;
    let baseline = fixture.run(false, false).await;
    assert_eq!(baseline.stats.downloaded, 1, "{baseline:?}");
    fixture.drop_trigger("fail_epoch_promotion_clear").await;
    let before = fixture.snapshot().await;
    assert_eq!(before.state, "ready");
    let old_pending = before.pending_token.clone().expect("staged token");

    let (
        descriptor_key,
        descriptor_bytes,
        token_key,
        token_bytes,
        baseline_key,
        old_origin,
        old_debt,
    ) = {
        let db = fixture.open().await;
        let conn = db.acquire_lock("make staged epoch proof stale").unwrap();
        let record = crate::state::db::sync_epoch::read_library(&conn, ZONE)
            .unwrap()
            .unwrap();
        let binding = record.binding.expect("staged binding");
        let descriptor: String = conn
            .query_row(
                "SELECT value FROM metadata WHERE key=?1",
                [&binding.descriptor_key],
                |row| row.get(0),
            )
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&descriptor).unwrap();
        let round = parsed["revalidation_round"]
            .as_str()
            .expect("production staged descriptor must own a catalog round");
        let (scope, mode, phase): (String, String, String) = conn
            .query_row(
                "SELECT scope,mode,phase FROM source_revalidation_rounds WHERE id=?1 AND library=?2",
                rusqlite::params![round, ZONE],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        let origin = crate::state::db::source_revalidation::CatalogOrigin::parse(&scope).unwrap();
        assert_eq!(origin.nonce, round);
        assert_eq!(origin.epoch, record.epoch);
        assert_eq!(origin.attempt, record.attempt);
        assert_eq!(mode, "catalog_v2");
        assert_eq!(phase, "observing");
        let debt: Vec<(String, i64)> = conn
            .prepare(
                "SELECT id,next_retry_at FROM source_revalidation_debt WHERE round=?1 ORDER BY id",
            )
            .unwrap()
            .query_map([round], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let token: String = conn
            .query_row(
                "SELECT value FROM metadata WHERE key=?1",
                [&binding.token_key],
                |row| row.get(0),
            )
            .unwrap();
        conn.execute(
            "DELETE FROM metadata WHERE key=?1",
            [binding.baseline_key.as_str()],
        )
        .unwrap();
        conn.execute(
            "UPDATE metadata SET value=?1 WHERE key='enum_config_hash'",
            [OLD_ENUM_HASH],
        )
        .unwrap();
        (
            binding.descriptor_key,
            descriptor,
            binding.token_key,
            token,
            binding.baseline_key,
            origin,
            debt,
        )
    };

    // Fail only the later inventory, after the rotated owner has installed its
    // own fresh boundary. Reopen before any new staged token can supersede the old one.
    let interrupted = fixture
        .run_with_inventory_failure(false, false, false, true, true)
        .await;
    assert!(!interrupted.db_sync_token_advance_safe, "{interrupted:?}");
    {
        let db = fixture.open().await;
        let conn = db
            .acquire_lock("inspect stale staged evidence after reopen")
            .unwrap();
        let record = crate::state::db::sync_epoch::read_library(&conn, ZONE)
            .unwrap()
            .unwrap();
        assert_eq!(record.state, "pending");
        assert_ne!(record.attempt, before.attempt);
        assert!(record.binding.is_none());
        let saved_descriptor: String = conn
            .query_row(
                "SELECT value FROM metadata WHERE key=?1",
                [&descriptor_key],
                |row| row.get(0),
            )
            .unwrap();
        let saved_token: String = conn
            .query_row(
                "SELECT value FROM metadata WHERE key=?1",
                [&token_key],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(saved_descriptor, descriptor_bytes);
        assert_eq!(saved_token, token_bytes);
        let phase: String = conn
            .query_row(
                "SELECT phase FROM source_revalidation_rounds WHERE id=?1 AND library=?2",
                rusqlite::params![old_origin.nonce, ZONE],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(phase, "abandoned");
        let debt: Vec<(String, i64)> = conn
            .prepare(
                "SELECT id,next_retry_at FROM source_revalidation_debt WHERE round=?1 ORDER BY id",
            )
            .unwrap()
            .query_map([&old_origin.nonce], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(debt, old_debt);
        let fresh_cursor: String = conn
            .query_row(
                "SELECT value FROM metadata WHERE key=?1",
                [&baseline_key],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(fresh_cursor, "inventory-last-page");
        let current_rounds: Vec<String> = conn.prepare("SELECT scope FROM source_revalidation_rounds WHERE library=?1 AND mode='catalog_v2' AND phase='observing'").unwrap().query_map([ZONE], |row| row.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
        assert!(current_rounds.iter().any(|scope| {
            crate::state::db::source_revalidation::CatalogOrigin::parse(scope).is_ok_and(|origin| {
                origin.epoch == record.epoch
                    && origin.attempt == record.attempt
                    && origin.plan_digest == old_origin.plan_digest
            })
        }));

        let staged: serde_json::Value = serde_json::from_str(&descriptor_bytes).unwrap();
        let enum_hash = staged["policy"][0].as_str().unwrap();
        let blocks = |plan: &str| {
            crate::state::db::provider_generations::observation::test_pending_catalog_staged_block(
                &conn,
                ZONE,
                &record.epoch,
                &record.attempt,
                enum_hash,
                plan,
            )
            .unwrap()
        };
        assert!(
            !blocks(&old_origin.plan_digest),
            "the owner-linked prior attempt is noncurrent after reopen"
        );
        assert!(blocks(&"f".repeat(64)), "a foreign plan remains blocked");
        let mut current = old_origin.clone();
        current.attempt = record.attempt.clone();
        let mut foreign_epoch = old_origin.clone();
        foreign_epoch.epoch = uuid::Uuid::new_v4().to_string();
        let mut wrong_nonce = old_origin.clone();
        wrong_nonce.nonce = uuid::Uuid::new_v4().to_string();
        for (scope, mode, phase) in [
            (current.scope(), "catalog_v2", "abandoned"),
            (foreign_epoch.scope(), "catalog_v2", "abandoned"),
            (wrong_nonce.scope(), "catalog_v2", "abandoned"),
            ("catalog:v2:malformed".to_owned(), "catalog_v2", "abandoned"),
            (old_origin.scope(), "legacy", "abandoned"),
            (old_origin.scope(), "future", "abandoned"),
            (old_origin.scope(), "catalog_v2", "observing"),
        ] {
            conn.execute_batch("SAVEPOINT staged_negative").unwrap();
            conn.execute(
                "UPDATE source_revalidation_rounds SET scope=?2,mode=?3,phase=?4 WHERE id=?1",
                rusqlite::params![old_origin.nonce, scope, mode, phase],
            )
            .unwrap();
            assert!(
                blocks(&old_origin.plan_digest),
                "unqualified round must block"
            );
            conn.execute_batch("ROLLBACK TO staged_negative; RELEASE staged_negative")
                .unwrap();
        }
        conn.execute_batch("SAVEPOINT staged_negative").unwrap();
        conn.execute(
            "INSERT INTO metadata(key,value) VALUES(?1,'independent')",
            [format!("pending_sync_token:v27:other:{ZONE}")],
        )
        .unwrap();
        assert!(
            blocks(&old_origin.plan_digest),
            "an unlinked same-library staged token must block"
        );
        conn.execute_batch("ROLLBACK TO staged_negative; RELEASE staged_negative")
            .unwrap();
        conn.execute_batch("SAVEPOINT staged_negative").unwrap();
        conn.execute(
            "INSERT INTO metadata(key,value) VALUES(?1,'other-library')",
            [format!("pending_sync_token:v27:other:{ZONE}:other")],
        )
        .unwrap();
        assert!(
            !blocks(&old_origin.plan_digest),
            "a colon-suffixed library must not match"
        );
        conn.execute_batch("ROLLBACK TO staged_negative; RELEASE staged_negative")
            .unwrap();
    }
    fixture.force_due().await;

    let full_before = fixture.full_queries.load(Ordering::SeqCst);
    let changes_before = fixture.changes.load(Ordering::SeqCst);
    let fail_before = fixture.fail_calls.load(Ordering::SeqCst);
    let cycle = fixture.run(false, false).await;
    assert_eq!(
        fixture.full_queries.load(Ordering::SeqCst),
        full_before + 1,
        "epoch baseline must win over pending-zone-token reconciliation: {cycle:?}"
    );
    assert!(fixture.changes.load(Ordering::SeqCst) >= changes_before);
    assert!(
        fixture.fail_calls.load(Ordering::SeqCst) <= fail_before + 1,
        "the full baseline may retry the admitted failed GET once"
    );
    let after = fixture.snapshot().await;
    assert_eq!(after.state, "complete", "{cycle:?}");
    assert_ne!(after.active_token.as_deref(), Some(old_pending.as_str()));
}

#[tokio::test]
async fn checkpoint_baseline_preserve_is_exact_to_ready_binding() {
    let fixture = Fixture::new(true).await;
    fixture.install_promotion_abort().await;
    let baseline = fixture.run(false, false).await;
    assert_eq!(baseline.stats.downloaded, 1, "{baseline:?}");
    fixture.drop_trigger("fail_epoch_promotion_clear").await;
    let ready = fixture.snapshot().await;
    let proof_baseline = {
        let db = fixture.open().await;
        let conn = db.acquire_lock("read ready binding baseline").unwrap();
        let record = crate::state::db::sync_epoch::read_library(&conn, ZONE)
            .unwrap()
            .unwrap();
        assert_eq!(record.attempt, ready.attempt);
        record.binding.expect("ready binding").baseline_key
    };
    let db = fixture.open().await;
    {
        let conn = db
            .acquire_lock("seed malformed unrelated epoch library")
            .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO metadata(key,value) VALUES('sync_epoch_library:v1:malformed-unrelated','not-json')",
            [],
        )
        .unwrap();
    }
    let transition = |key: String| crate::state::db::CheckpointTransition {
        expected_metadata: Vec::new(),
        legacy_preservation_proofs: Vec::new(),
        legacy_config_hash: None,
        sparse_identity_proofs: Vec::new(),
        metadata_updates: Vec::new(),
        metadata_deletes: vec![key],
    };
    db.commit_checkpoint_transition(transition(proof_baseline.clone()))
        .await
        .unwrap();
    {
        let conn = db.acquire_lock("check ready baseline preserve").unwrap();
        let present: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM metadata WHERE key=?1)",
                [proof_baseline.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            present,
            "the ready staged proof's baseline must be preserved"
        );
    }
    let completed = fixture.run(false, false).await;
    assert_eq!(completed.failed_count, 0, "{completed:?}");
    let epoch = db.sync_epoch_id().await.unwrap().expect("epoch id");
    let unrelated = format!("baseline_cursor:v27:{epoch}:unrelated");
    db.set_metadata(&unrelated, "unrelated-boundary")
        .await
        .unwrap();
    db.commit_checkpoint_transition(transition(unrelated.clone()))
        .await
        .unwrap();
    let conn = db.acquire_lock("check completed baseline delete").unwrap();
    let present: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM metadata WHERE key=?1)",
            [unrelated.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        !present,
        "a completed or unrelated baseline must not be preserved"
    );
}

#[tokio::test]
async fn capture_repair_applies_during_pending_epoch_and_leaves_epoch_pending() {
    let metadata_media = include_bytes!("../../../../../tests/data/media/metadata.jpg").to_vec();
    let fixture = Fixture::new_with_ok_media(false, metadata_media).await;
    fixture.install_completion_abort().await;
    let baseline = fixture.run(false, false).await;
    assert_eq!(baseline.stats.downloaded, 1, "{baseline:?}");
    fixture.drop_trigger("fail_epoch_complete").await;
    let baseline_snapshot = fixture.snapshot().await;
    let ok_path = baseline_snapshot
        .ok_receipt
        .as_ref()
        .expect("ok receipt")
        .0
        .clone();
    let before_probe = crate::download::metadata::probe_exif(std::path::Path::new(&ok_path))
        .expect("probe ok media");
    assert!(before_probe.datetime_original.is_some());

    // Explicit repair baseline queues the durable capture-repair marker. This
    // run may retry the admitted failed GET, but it must not claim epoch
    // completion or rewrite unrequested media metadata.
    fixture.force_due().await;
    let queued = fixture.run_with_repair(false).await;
    let queued_snapshot = fixture.snapshot().await;
    assert_ne!(queued_snapshot.state, "complete");
    assert!(!queued_snapshot.binding_present);
    assert!(queued_snapshot.active_token.is_none());
    assert_eq!(fixture.full_queries.load(Ordering::SeqCst), 2);
    let queued_marker = {
        let db = fixture.open().await;
        let conn = db.acquire_lock("inspect queued repair marker").unwrap();
        let marker: Option<String> = conn
            .query_row(
                "SELECT capture_repair_metadata_hash FROM assets WHERE id='asset-ok' AND version_size='original'",
                [],
                |row| row.get(0),
            )
            .ok();
        marker
    };
    assert!(
        queued_marker
            .as_deref()
            .is_some_and(|value| !value.is_empty()),
        "explicit repair must queue its durable capture marker: {queued:?}"
    );
    assert_eq!(
        crate::download::metadata::probe_exif(std::path::Path::new(&ok_path))
            .unwrap()
            .datetime_original,
        before_probe.datetime_original
    );

    // The pending epoch stays deferred; local recovery drains only the explicit
    // requested repair, then leaves the epoch pending for a normal baseline.
    let repaired = fixture.run_with_repair(false).await;
    let repaired_snapshot = fixture.snapshot().await;
    assert_ne!(repaired_snapshot.state, "complete");
    assert!(!repaired_snapshot.binding_present);
    assert!(repaired_snapshot.active_token.is_none());
    assert_eq!(fixture.full_queries.load(Ordering::SeqCst), 2);
    let after_probe = crate::download::metadata::probe_exif(std::path::Path::new(&ok_path))
        .expect("probe repaired media");
    assert_ne!(
        after_probe.datetime_original, before_probe.datetime_original,
        "the explicit repair must replace the embedded capture timestamp"
    );
    #[cfg(feature = "xmp")]
    assert_eq!(
        after_probe.datetime_original.as_deref(),
        Some("2023-11-14T14:13:20-08:00"),
        "the explicit repair must write the capture-local timestamp with its matching offset: {after_probe:?}"
    );
    #[cfg(not(feature = "xmp"))]
    {
        assert_eq!(
            after_probe.datetime_original.as_deref(),
            Some("2023:11:14 14:13:20"),
            "the explicit repair must write the capture-local EXIF timestamp: {after_probe:?}"
        );
        assert_eq!(
            after_probe.offset_time_original.as_deref(),
            Some("-08:00"),
            "the explicit repair must write the matching EXIF offset: {after_probe:?}"
        );
    }

    // A compact receipt for this exact completed asset/path/rendition/provider
    // resource is the only local-byte proof an explicit capture repair may
    // advance. Assert it exists and owns the repaired bytes before relying on
    // the ordinary baseline reuse path.
    let repaired_checksum = crate::download::file::compute_sha256(std::path::Path::new(&ok_path))
        .await
        .unwrap();
    let receipt_evidence = |conn: &rusqlite::Connection| -> (i64, String, String) {
        let count: i64 = conn
            .query_row(
                "SELECT count(*) FROM download_receipts WHERE state_id='asset-ok'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let proof: String = conn
            .query_row(
                "SELECT json_array(account_key,provider_key,scope,library,child,master,state_id,\
                 version_size,provider_checksum,provider_size,native_path,source_checksum) \
                 FROM download_receipts WHERE state_id='asset-ok'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let local: String = conn
            .query_row(
                "SELECT local_checksum FROM download_receipts WHERE state_id='asset-ok'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        (count, proof, local)
    };
    let repaired_receipt = {
        let db = fixture.open().await;
        let conn = db.acquire_lock("inspect repaired compact receipt").unwrap();
        receipt_evidence(&conn)
    };
    let legacy_census = {
        let db = fixture.open().await;
        let conn = db.acquire_lock("inspect repair legacy census").unwrap();
        let rows: Vec<(String, String, String, String, String, String)> = conn.prepare(
            "SELECT r.id,r.scope,r.enum_hash,r.boundary,r.phase,COALESCE((SELECT json_group_array(json_array(id,next_retry_at)) FROM (SELECT id,next_retry_at FROM source_revalidation_debt WHERE round=r.id ORDER BY id)),'[]') FROM source_revalidation_rounds r WHERE r.library=?1 AND r.mode='legacy_unfiled_v1' ORDER BY r.id"
        ).unwrap().query_map([ZONE], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?))).unwrap().collect::<Result<_,_>>().unwrap();
        assert_eq!(
            rows.len(),
            1,
            "the real explicit repair must retain its legacy census"
        );
        rows
    };
    assert_eq!(
        repaired_receipt.0, 1,
        "the repaired asset must have exactly one compact receipt"
    );
    assert_eq!(
        repaired_receipt.2, repaired_checksum,
        "the compact receipt must own the repaired local bytes"
    );

    // A later normal baseline reuses those exact bytes: no provider GET, no
    // rewrite, no full enumeration, and no change to the receipt's source,
    // provider identity or native path. The unrelated failed admission keeps
    // its own unresolved evidence.
    let repaired_bytes = std::fs::read(&ok_path).unwrap();
    let repaired_modified = std::fs::metadata(&ok_path).unwrap().modified().unwrap();
    let lookups_before = fixture.ok_lookups.load(Ordering::SeqCst);
    fixture.force_due().await;
    let normal = fixture.run(false, false).await;
    assert_eq!(normal.failed_count, 0, "{normal:?}");
    assert_eq!(normal.stats.downloaded, 0, "{normal:?}");
    assert_eq!(fixture.full_queries.load(Ordering::SeqCst), 3, "{normal:?}");
    assert_eq!(fixture.ok_calls.load(Ordering::SeqCst), 1);
    assert!(
        fixture.ok_lookups.load(Ordering::SeqCst) > lookups_before,
        "the current source must confirm the repaired asset before legacy debt is retired"
    );
    let completed = fixture.snapshot().await;
    assert_eq!(completed.state, "complete");
    assert!(!completed.binding_present);
    assert!(completed.active_token.is_some());
    assert_eq!(
        std::fs::read(&ok_path).unwrap(),
        repaired_bytes,
        "ordinary baseline reuse must not rewrite repaired media"
    );
    assert_eq!(
        std::fs::metadata(&ok_path).unwrap().modified().unwrap(),
        repaired_modified,
        "ordinary baseline reuse must not touch repaired media"
    );
    assert_eq!(completed.ok_receipt, repaired_snapshot.ok_receipt);
    assert_eq!(completed.fail_receipt, repaired_snapshot.fail_receipt);
    assert_eq!(completed.failed_asset_rows, 1);
    assert_eq!(
        queued_snapshot.generation, repaired_snapshot.generation,
        "explicit capture repair must not reseal or alter generation checkpoint evidence"
    );
    let completed_receipt = {
        let db = fixture.open().await;
        let conn = db.acquire_lock("inspect reused compact receipt").unwrap();
        let legacy_after: Vec<(String, String, String, String, String, String)> = conn.prepare(
            "SELECT r.id,r.scope,r.enum_hash,r.boundary,r.phase,COALESCE((SELECT json_group_array(json_array(id,next_retry_at)) FROM (SELECT id,next_retry_at FROM source_revalidation_debt WHERE round=r.id ORDER BY id)),'[]') FROM source_revalidation_rounds r WHERE r.library=?1 AND r.mode='legacy_unfiled_v1' ORDER BY r.id"
        ).unwrap().query_map([ZONE], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?))).unwrap().collect::<Result<_,_>>().unwrap();
        assert_eq!(legacy_after.len(), 1);
        assert_eq!(
            (
                &legacy_after[0].0,
                &legacy_after[0].1,
                &legacy_after[0].2,
                &legacy_after[0].3,
                &legacy_after[0].4
            ),
            (
                &legacy_census[0].0,
                &legacy_census[0].1,
                &legacy_census[0].2,
                &legacy_census[0].3,
                &legacy_census[0].4
            ),
            "the separate legacy round remains intact"
        );
        assert_eq!(legacy_census[0].5, "[[\"asset-ok\",0]]");
        assert_eq!(
            legacy_after[0].5, "[]",
            "only the freshly confirmed legacy identity is retired"
        );
        receipt_evidence(&conn)
    };
    assert_eq!(
        completed_receipt, repaired_receipt,
        "baseline reuse must not change the repaired receipt's identity or bytes"
    );
    let _ = (queued, repaired);
}

#[tokio::test]
async fn defer_retry_failed_replays_a_legacy_pending_asset_through_its_owner() {
    use crate::test_helpers::TestAssetRecord;

    let mut fixture = Fixture::new(false).await;
    fixture.install_completion_abort().await;
    let baseline = fixture.run(false, false).await;
    assert_eq!(baseline.stats.downloaded, 1, "{baseline:?}");
    fixture.drop_trigger("fail_epoch_complete").await;

    let calls = fixture.add_legacy_asset().await;
    let checksum = fixture.legacy_checksum.clone().expect("legacy checksum");
    let size = (MEDIA.len() + b"legacy-retry-payload".len()) as u64;
    let db = fixture.open().await;
    db.upsert_seen(
        &TestAssetRecord::new("asset-legacy")
            .library(ZONE)
            .filename("legacy.JPG")
            .checksum(&checksum)
            .size(size)
            .build(),
    )
    .await
    .unwrap();
    db.upsert_asset_master_mapping(ZONE, "asset-legacy", "legacy")
        .await
        .unwrap();
    drop(db);

    let before = fixture.snapshot().await;
    assert_eq!(before.state, "pending");
    let full_before = fixture.full_queries.load(Ordering::SeqCst);
    let changes_before = fixture.changes.load(Ordering::SeqCst);
    let fail_before = fixture.fail_calls.load(Ordering::SeqCst);

    let retry = fixture.run(true, false).await;
    assert_eq!(retry.failed_count, 0, "{retry:?}");
    assert_eq!(fixture.full_queries.load(Ordering::SeqCst), full_before);
    assert_eq!(fixture.changes.load(Ordering::SeqCst), changes_before);
    assert_eq!(fixture.fail_calls.load(Ordering::SeqCst), fail_before);
    assert_eq!(calls.load(Ordering::SeqCst), 1, "legacy owner must retry");

    {
        let db = fixture.open().await;
        let conn = db.acquire_lock("inspect legacy retry").unwrap();
        let (status, local_path): (String, Option<String>) = conn
            .query_row(
                "SELECT status,local_path FROM assets WHERE library=?1 AND id='asset-legacy' AND version_size='original'",
                [ZONE],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, "downloaded");
        let path = local_path.expect("legacy receipt path");
        assert_eq!(std::fs::read(&path).unwrap().len() as u64, size);
    }
    let after = fixture.snapshot().await;
    assert_eq!(after.state, "pending");
    assert_eq!(after.binding_value, before.binding_value);
    assert_eq!(after.destination, before.destination);
    assert_eq!(after.decision, before.decision);
}

#[tokio::test]
async fn reset_sync_token_rotates_multiple_libraries_including_colon_names() {
    use crate::config::GlobalArgs;
    use crate::types::Domain;

    const PEER: &str = "SharedSync:peer:colon";
    let fixture = Fixture::new_cli(false).await;
    let db = fixture.open().await;
    db.enroll_sync_epoch(vec![(ZONE.to_owned(), "scope".to_owned())])
        .await
        .unwrap();
    let epoch = db.sync_epoch_id().await.unwrap().expect("epoch id");
    {
        let conn = db.acquire_lock("seed second epoch library").unwrap();
        let record = json!({
            "version": 2,
            "epoch": epoch,
            "library": PEER,
            "state": "pending",
            "reason": "seed",
            "attempt": "peer-old",
            "attempts": 1,
            "next_attempt_at": 0,
            "last_outcome": "seed",
            "baseline_scope": "peer-scope",
            "enrolled_at": 0,
            "completed_at": null
        });
        conn.execute(
            "INSERT OR REPLACE INTO metadata(key,value) VALUES(?1,?2)",
            rusqlite::params![
                crate::state::db::sync_epoch::library_key(PEER),
                record.to_string()
            ],
        )
        .unwrap();
        for key in [
            format!("sync_token:v27:{PEER}"),
            format!("sync_token:{PEER}"),
            format!("pending_sync_token:v27:enum:{PEER}"),
            format!("observed_selection_staged:v27:enum:{PEER}"),
            "pending_sync_token:v27:enum:inert-retained-zone".to_owned(),
        ] {
            conn.execute(
                "INSERT OR REPLACE INTO metadata(key,value) VALUES(?1,'old')",
                [key],
            )
            .unwrap();
        }
    }
    drop(db);

    let globals = GlobalArgs {
        username: Some("baseline@example.invalid".to_owned()),
        domain: Some(Domain::Com),
        data_dir: Some(fixture.dir().to_string_lossy().into_owned()),
    };
    crate::commands::run_reset_sync_token(true, &globals, None, crate::InputMode::NoInput)
        .await
        .expect("production reset sync-token path");

    let db = fixture.open().await;
    let conn = db.acquire_lock("inspect reset multiple libraries").unwrap();
    for library in [ZONE, PEER] {
        let record = crate::state::db::sync_epoch::read_library(&conn, library)
            .unwrap()
            .expect("library record");
        assert_eq!(record.state, "pending");
        assert!(record.next_attempt_at <= chrono::Utc::now().timestamp());
        assert!(
            record.binding.is_none(),
            "reset must clear the staged binding for {library}"
        );
        let mut statement = conn
            .prepare(
                "SELECT key FROM metadata WHERE key LIKE 'sync_token:%' \
                 OR key LIKE 'pending_sync_token:v27:%' \
                 OR key LIKE 'observed_selection_staged:v27:%'",
            )
            .unwrap();
        let remaining: Vec<String> = statement
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert!(
            remaining.iter().all(|key| !key.contains(library)),
            "reset must clear exact token/staged keys for {library}: {remaining:?}"
        );
    }
    let peer_record = crate::state::db::sync_epoch::read_library(&conn, PEER)
        .unwrap()
        .unwrap();
    assert_ne!(peer_record.attempt, "peer-old");
    assert_eq!(peer_record.library, PEER);
    let inert_present: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM metadata WHERE key='pending_sync_token:v27:enum:inert-retained-zone')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        inert_present,
        "unenrolled/legacy staged evidence must be retained"
    );
}

#[tokio::test]
async fn healthy_second_library_downloads_new_media_during_epoch_hold() {
    const PEER: &str = "SharedSync-epoch-peer-new-media";
    let fixture = Fixture::new(false).await;
    fixture.install_completion_abort().await;
    let baseline = fixture.run(false, false).await;
    assert_eq!(baseline.stats.downloaded, 1, "{baseline:?}");
    fixture.drop_trigger("fail_epoch_complete").await;
    let zone_before = fixture.snapshot().await;
    assert_eq!(zone_before.state, "pending");

    let peer_media = {
        let mut bytes = MEDIA.to_vec();
        bytes.extend_from_slice(b"healthy-peer-new-media");
        bytes
    };
    let peer_checksum =
        base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&peer_media));
    let peer_calls = Arc::new(AtomicUsize::new(0));
    Mock::given(method("GET"))
        .and(path("/peer-new"))
        .respond_with(FlakyBytes {
            calls: Arc::clone(&peer_calls),
            bytes: Arc::new(peer_media.clone()),
            fail_first: 0,
        })
        .mount(&fixture._server)
        .await;
    let mut peer_page = full_album_page_with_download(
        PEER,
        "peer-new",
        "inventory-last-page",
        &format!("{}/peer-new", fixture._server.uri()),
        peer_media.len() as u64,
        &peer_checksum,
    );
    peer_page["records"][0]["fields"]["filenameEnc"]["value"] =
        json!(base64::engine::general_purpose::STANDARD.encode("peer-new.JPG"));
    peer_page["records"][0]["recordChangeTag"] = json!("ct-master-peer-new");
    peer_page["records"][1]["recordChangeTag"] = json!("ct-asset-peer-new");
    let peer_inventory = peer_page["records"].as_array().unwrap().clone();
    let peer_full = Arc::new(AtomicUsize::new(0));
    let peer_changes = Arc::new(AtomicUsize::new(0));

    let db = fixture.open().await;
    let epoch = db.sync_epoch_id().await.unwrap().expect("epoch id");
    {
        let conn = db.acquire_lock("seed healthy peer epoch").unwrap();
        let record = json!({
            "version": 2,
            "epoch": epoch,
            "library": PEER,
            "state": "complete",
            "reason": "existing_upgrade",
            "attempt": "peer-complete",
            "attempts": 1,
            "next_attempt_at": 0,
            "last_outcome": "complete",
            "baseline_scope": "peer-scope",
            "enrolled_at": 0,
            "completed_at": 0
        });
        conn.execute(
            "INSERT OR REPLACE INTO metadata(key,value) VALUES(?1,?2)",
            rusqlite::params![
                crate::state::db::sync_epoch::library_key(PEER),
                record.to_string()
            ],
        )
        .unwrap();
    }

    let zone_session = ResetCycleSession {
        zone: ZONE,
        inventory: Arc::new(fixture.inventory.clone()),
        full_queries: Arc::clone(&fixture.full_queries),
        changes: Arc::clone(&fixture.changes),
        fail_full: false,
        auth_expired: false,
        boundary: "inventory-last-page",
    };
    let peer_session = ResetCycleSession {
        zone: PEER,
        inventory: Arc::new(peer_inventory),
        full_queries: Arc::clone(&peer_full),
        changes: Arc::clone(&peer_changes),
        fail_full: false,
        auth_expired: false,
        boundary: "inventory-last-page",
    };
    let make_state = |zone_name: &'static str, session: ResetCycleSession| {
        let db = Arc::clone(&db);
        async move {
            let mut library = PhotoLibrary::new(
                "https://synthetic.example/database/1/photos/production/private".into(),
                Arc::new(std::collections::HashMap::new()),
                Box::new(session),
                Arc::new(json!({"zoneName":zone_name,"ownerRecordName":"_defaultOwner"})),
                "private".into(),
                crate::retry::RetryConfig::default(),
            )
            .await
            .unwrap();
            library.shadow_capture = Some(crate::icloud::photos::inbox::ShadowCapture::new(
                db,
                baseline_owner(),
                "com",
            ));
            let passes = vec![AlbumPass {
                kind: PassKind::Unfiled,
                album: library.all(),
                exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
            }];
            let mut state = make_run_cycle_library_state_with_passes(
                zone_name,
                &sync_token_key(zone_name),
                passes,
            );
            state.library = library;
            state
        }
    };
    let zone_state = make_state(ZONE, zone_session).await;
    let peer_state = make_state(PEER, peer_session).await;
    let builder = make_run_cycle_download_config_builder(&fixture.root, db.clone());
    let full_before = fixture.full_queries.load(Ordering::SeqCst);
    let fail_before = fixture.fail_calls.load(Ordering::SeqCst);
    let cycle = run_cycle(
        &[&zone_state, &peer_state],
        &make_run_cycle_config(),
        Some(db.as_ref()),
        false,
        &builder,
        download::DownloadControls::download_hidden(),
        &fixture.shared,
        &CancellationToken::new(),
    )
    .await
    .expect("healthy peer cycle");
    assert_eq!(peer_full.load(Ordering::SeqCst), 1, "{cycle:?}");
    assert_eq!(peer_calls.load(Ordering::SeqCst), 1, "peer new media GET");
    assert_eq!(fixture.full_queries.load(Ordering::SeqCst), full_before);
    assert_eq!(fixture.fail_calls.load(Ordering::SeqCst), fail_before);

    let peer_receipt = {
        let conn = db.acquire_lock("inspect healthy peer receipt").unwrap();
        let (status, path): (String, Option<String>) = conn
            .query_row(
                "SELECT status,local_path FROM assets WHERE library=?1 AND id='asset-peer-new' AND version_size='original'",
                [PEER],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, "downloaded");
        let path = path.expect("peer receipt path");
        (path, peer_media.clone())
    };
    assert_eq!(std::fs::read(&peer_receipt.0).unwrap(), peer_receipt.1);
    let zone_after = fixture.snapshot().await;
    assert_eq!(zone_after.state, "pending");
    assert_eq!(zone_after.binding_value, zone_before.binding_value);
    assert_eq!(zone_after.destination, zone_before.destination);
    assert_eq!(zone_after.decision, zone_before.decision);
}

/// A real deferred epoch is a selected library hold: the completed cycle must
/// report partial_failure and be eligible for the one-shot partial exit without
/// inventing failed assets or counts.
#[tokio::test]
async fn epoch_hold_reports_partial_failure_without_fabricated_counts() {
    let fixture = Fixture::new(false).await;
    fixture.install_completion_abort().await;
    let baseline = fixture.run(false, false).await;
    assert_eq!(baseline.stats.downloaded, 1, "{baseline:?}");
    fixture.drop_trigger("fail_epoch_complete").await;
    assert_eq!(fixture.snapshot().await.state, "pending");

    let held = fixture.run(false, false).await;
    assert_eq!(held.failed_count, 0, "{held:?}");
    assert_eq!(held.stats.failed, 0, "{held:?}");
    assert_eq!(held.stats.downloaded, 0, "{held:?}");

    let facts = crate::cycle_reporter::CycleFacts::from_cycle_result(
        &held,
        std::time::Duration::from_secs(1),
    );
    assert_eq!(
        facts.status,
        crate::cycle_reporter::CycleStatus::Failed,
        "an actual epoch hold must not classify as success: {held:?}"
    );
    assert_eq!(
        held.holds.len(),
        1,
        "one held library must have exactly one real hold fact: {held:?}"
    );
    let hold = &held.holds[0];
    assert_eq!(hold.label, "SharedSync-v027-syn");
    assert_eq!(hold.source, ZONE);
    assert_eq!(hold.ordinal, 0);
    assert_eq!(
        hold.kind,
        crate::sync_cycle::LibraryHoldKind::BaselinePending
    );
    assert_eq!(hold.reason, "epoch_backoff");
    assert!(
        !held.db_sync_token_advance_safe,
        "a pending selected baseline holds the aggregate database checkpoint"
    );
    assert_eq!(
        held.stats.sync_token_blocked_zone.as_deref(),
        Some(ZONE),
        "the existing singular machine block field keeps the full zone"
    );

    let partial = crate::sync_loop::runner::one_shot_partial_error(&held)
        .expect("a one-shot epoch hold must exit partial");
    let partial_text = partial.to_string();
    assert!(
        partial_text.contains("1 library held incomplete")
            && partial_text.contains("SharedSync-v027-syn")
            && partial_text.contains("epoch_backoff"),
        "the one-shot error must carry safe scope and reason: {partial_text}"
    );
    assert!(
        !partial_text.contains("0 sync failures"),
        "a hold must never fabricate a zero failure count"
    );
    let exit_error: anyhow::Error = partial.into();
    assert_eq!(crate::classify_exit_error(&exit_error).exit_code(), 2);

    let db = fixture.open().await;
    let report_path = fixture.dir().join("sync_report.json");
    let notifier = crate::notifications::Notifier::new(None);
    let run_config = make_run_cycle_config();
    let reporter =
        crate::cycle_reporter::CycleReporter::new(crate::cycle_reporter::CycleReporterConfig {
            watch_mode: false,
            report_path: Some(&report_path),
            run_options: crate::report::RunOptions::from_config(&run_config),
            health_dir: fixture.dir(),
            personality_mode: crate::personality::Mode::Off,
            state_db: Some(db.as_ref()),
            metrics_handle: None,
            notifier: &notifier,
        });
    let mut health = crate::health::HealthStatus::new();
    reporter.report_completed_cycle(&mut health, facts).await;

    let report: Value =
        serde_json::from_str(&std::fs::read_to_string(&report_path).expect("epoch hold report"))
            .expect("epoch hold report json");
    assert_eq!(report["stats"]["failed"], 0, "{report}");
    assert_eq!(report["stats"]["downloaded"], 0, "{report}");
    assert_eq!(report["status"], "partial_failure");
    assert_eq!(
        report["stats"]["sync_token_blocked_zone"], ZONE,
        "the existing singular machine block field is retained: {report}"
    );
    let health_error = health.last_error.as_deref().expect("hold health error");
    assert!(
        health_error.contains("SharedSync-v027-syn")
            && health_error.contains("baseline_pending")
            && health_error.contains("epoch_backoff"),
        "health must name the safe label, kind and reason: {health_error}"
    );

    let configured_interval = 3_600;
    let mut metadata_followup = held.stats.clone();
    metadata_followup.metadata_capture_remaining = 1;
    metadata_followup.metadata_capture_progressed = true;
    assert_eq!(
        crate::sync_loop::watch::metadata_capture_watch_interval_for_cycle(
            Some(configured_interval),
            &metadata_followup,
            held.failed_count,
            true,
        ),
        Some(configured_interval),
        "a selected hold keeps the configured cadence even when a real metadata follow-up exists"
    );
}

#[tokio::test]
async fn reset_sync_token_failure_rolls_back_every_component() {
    use crate::config::GlobalArgs;
    use crate::types::Domain;

    let fixture = Fixture::new_cli(true).await;
    fixture.install_promotion_abort().await;
    let baseline = fixture.run(false, false).await;
    assert_eq!(baseline.stats.downloaded, 1, "{baseline:?}");
    fixture.drop_trigger("fail_epoch_promotion_clear").await;
    let before = fixture.snapshot().await;
    assert_eq!(before.state, "ready");
    let before_token = {
        let db = fixture.open().await;
        let conn = db.acquire_lock("seed malformed reset library").unwrap();
        let token: Option<String> = conn
            .query_row(
                "SELECT value FROM metadata WHERE key=?1",
                [sync_token_key(ZONE)],
                |row| row.get(0),
            )
            .ok();
        conn.execute(
            "INSERT INTO metadata(key,value) VALUES('sync_epoch_library:v1:bad-reset','not-json')",
            [],
        )
        .unwrap();
        token
    };

    let globals = GlobalArgs {
        username: Some("baseline@example.invalid".to_owned()),
        domain: Some(Domain::Com),
        data_dir: Some(fixture.dir().to_string_lossy().into_owned()),
    };
    let error =
        crate::commands::run_reset_sync_token(true, &globals, None, crate::InputMode::NoInput)
            .await
            .expect_err("malformed epoch library must fail the reset");
    let _ = error;
    let after = fixture.snapshot().await;
    assert_eq!(after.state, before.state);
    assert_eq!(after.attempt, before.attempt);
    assert_eq!(after.binding_value, before.binding_value);
    assert_eq!(after.pending_token, before.pending_token);
    assert_eq!(after.active_token, before.active_token);
    assert_eq!(after.destination, before.destination);
    assert_eq!(after.decision, before.decision);
    let current_token = {
        let db = fixture.open().await;
        let conn = db.acquire_lock("inspect rolled back reset").unwrap();
        let token: Option<String> = conn
            .query_row(
                "SELECT value FROM metadata WHERE key=?1",
                [sync_token_key(ZONE)],
                |row| row.get(0),
            )
            .ok();
        token
    };
    assert_eq!(current_token, before_token);
}

fn hold_fact(
    label: &str,
    source: &str,
    ordinal: usize,
    kind: crate::sync_cycle::LibraryHoldKind,
    reason: &'static str,
) -> crate::sync_cycle::LibraryHoldFact {
    crate::sync_cycle::LibraryHoldFact {
        label: label.to_owned(),
        source: source.to_owned(),
        ordinal,
        kind,
        reason,
    }
}

#[tokio::test]
async fn identity_hold_keeps_zero_failed_count_and_real_hold() {
    let fixture = Fixture::new(true).await;
    fixture.install_promotion_abort().await;
    let baseline = fixture.run(false, false).await;
    assert_eq!(baseline.failed_count, 1, "{baseline:?}");
    fixture.drop_trigger("fail_epoch_promotion_clear").await;
    assert_eq!(fixture.snapshot().await.state, "ready");
    {
        let db = fixture.open().await;
        db.set_metadata(&crate::state::unresolved_identity_key(ZONE), "1")
            .await
            .unwrap();
    }

    let held = fixture.run(false, false).await;
    assert_eq!(
        held.failed_count, 0,
        "a current identity hold must not invent a failure count: {held:?}"
    );
    assert_eq!(held.stats.failed, 0, "{held:?}");
    assert_eq!(held.stats.state_write_failures, 0, "{held:?}");
    let hold = held
        .holds
        .iter()
        .find(|hold| hold.kind == crate::sync_cycle::LibraryHoldKind::IdentityUnresolved)
        .expect("actual identity hold fact");
    assert_eq!(hold.source, ZONE);
    assert_eq!(hold.ordinal, 0);
    let facts = crate::cycle_reporter::CycleFacts::from_cycle_result(
        &held,
        std::time::Duration::from_secs(1),
    );
    assert_eq!(facts.status, crate::cycle_reporter::CycleStatus::Failed);
}

#[tokio::test]
async fn retained_hold_keeps_zero_failed_count_and_real_hold() {
    super::ordinary_expiry::exercise_retained_hold_reporting().await;
}

#[tokio::test]
async fn preservation_pending_keeps_current_token_truth_and_partial_report() {
    let fixture = Fixture::new(false).await;
    let baseline = fixture.run(false, false).await;
    assert_eq!(baseline.stats.downloaded, 1, "{baseline:?}");
    {
        let db = fixture.open().await;
        let conn = db.acquire_lock("seed pending preservation").unwrap();
        conn.execute(
            "INSERT INTO unattributed_legacy(library,asset_id,evidence_version,original_evidence,files,prepared_at) \
             VALUES(?1,?2,1,'{}','[]',0)",
            rusqlite::params![ZONE, "legacy-preservation-pending"],
        )
        .unwrap();
    }

    let held = fixture.run(false, false).await;
    assert_eq!(held.stats.unattributed_legacy_pending, 1, "{held:?}");
    assert_eq!(held.failed_count, 0, "{held:?}");
    assert_eq!(held.stats.failed, 0, "{held:?}");
    assert!(
        !held.stats.sync_token_blocked,
        "pending preservation must not fabricate a token-block fact: {held:?}"
    );
    assert!(
        held.can_advance_database_checkpoint(),
        "current source checkpoint authority stays separate from historical pending: {held:?}"
    );
    let hold = held
        .holds
        .iter()
        .find(|hold| hold.kind == crate::sync_cycle::LibraryHoldKind::PreservationPending)
        .expect("actual preservation hold fact");
    assert_eq!(hold.source, ZONE);
    assert_eq!(hold.ordinal, 0);

    let db = fixture.open().await;
    let report_path = fixture.dir().join("preservation_report.json");
    let notifier = crate::notifications::Notifier::new(None);
    let run_config = make_run_cycle_config();
    let reporter =
        crate::cycle_reporter::CycleReporter::new(crate::cycle_reporter::CycleReporterConfig {
            watch_mode: false,
            report_path: Some(&report_path),
            run_options: crate::report::RunOptions::from_config(&run_config),
            health_dir: fixture.dir(),
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
        serde_json::from_str(&std::fs::read_to_string(&report_path).expect("preservation report"))
            .expect("preservation report json");
    assert_eq!(report["status"], "partial_failure");
    assert_eq!(report["stats"]["failed"], 0, "{report}");
    assert_eq!(report["stats"]["sync_token_blocked"], false, "{report}");
    assert!(
        health
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains("legacy_preservation_pending")),
        "{:?}",
        health.last_error
    );
}

#[tokio::test]
async fn one_shot_hold_error_uses_distinct_sources_and_lists_every_reason() {
    let holds = vec![
        hold_fact(
            "SharedSync-A1B2C3D4",
            "SharedSync-A1B2C3D4-EEEE",
            0,
            crate::sync_cycle::LibraryHoldKind::BaselinePending,
            "epoch_backoff",
        ),
        hold_fact(
            "SharedSync-A1B2C3D4",
            "SharedSync-A1B2C3D4-EEEE",
            0,
            crate::sync_cycle::LibraryHoldKind::IdentityUnresolved,
            "asset_delta_hydration_incomplete",
        ),
        hold_fact(
            "SharedSync-A1B2C3D4",
            "SharedSync-A1B2C3D4-FFFF",
            1,
            crate::sync_cycle::LibraryHoldKind::PreservationPending,
            "legacy_preservation_pending",
        ),
    ];
    let result = CycleResult {
        failed_count: 0,
        session_expired: false,
        stats: download::SyncStats::default(),
        db_sync_token_advance_safe: false,
        holds,
        sources: Vec::new(),
        operational_safe_holds: false,
    };
    assert_eq!(
        result.held_source_count(),
        2,
        "two facts from one source are one held library"
    );
    assert_eq!(
        crate::cycle_reporter::CycleFacts::from_cycle_result(&result, std::time::Duration::ZERO)
            .status,
        crate::cycle_reporter::CycleStatus::Failed
    );
    let partial = crate::sync_loop::runner::one_shot_partial_error(&result)
        .expect("two held sources must exit partial");
    let text = partial.to_string();
    assert!(
        text.contains("2 libraries held incomplete")
            && text.contains("epoch_backoff")
            && text.contains("asset_delta_hydration_incomplete")
            && text.contains("legacy_preservation_pending"),
        "{text}"
    );
    assert!(!text.contains("3 libraries"), "{text}");

    let dir = tempfile::tempdir().unwrap();
    let report_path = dir.path().join("holds_report.json");
    let notifier = crate::notifications::Notifier::new(None);
    let run_config = make_run_cycle_config();
    let reporter: crate::cycle_reporter::CycleReporter<'_, crate::state::SqliteStateDb> =
        crate::cycle_reporter::CycleReporter::new(crate::cycle_reporter::CycleReporterConfig {
            watch_mode: false,
            report_path: Some(&report_path),
            run_options: crate::report::RunOptions::from_config(&run_config),
            health_dir: dir.path(),
            personality_mode: crate::personality::Mode::Off,
            state_db: None,
            metrics_handle: None,
            notifier: &notifier,
        });
    let mut health = crate::health::HealthStatus::new();
    reporter
        .report_completed_cycle(
            &mut health,
            crate::cycle_reporter::CycleFacts::from_cycle_result(
                &result,
                std::time::Duration::ZERO,
            ),
        )
        .await;
    assert!(
        health
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains("epoch_backoff")
                && error.contains("asset_delta_hydration_incomplete")
                && error.contains("legacy_preservation_pending")),
        "{:?}",
        health.last_error
    );

    let auth = CycleResult {
        failed_count: 0,
        session_expired: true,
        stats: download::SyncStats::default(),
        db_sync_token_advance_safe: false,
        holds: result.holds.clone(),
        sources: Vec::new(),
        operational_safe_holds: false,
    };
    assert_eq!(
        crate::cycle_reporter::CycleFacts::from_cycle_result(&auth, std::time::Duration::ZERO)
            .status,
        crate::cycle_reporter::CycleStatus::SessionExpired
    );
    assert!(crate::sync_loop::runner::one_shot_partial_error(&auth).is_none());

    let interrupted = CycleResult {
        failed_count: 0,
        session_expired: false,
        stats: download::SyncStats {
            interrupted: true,
            ..download::SyncStats::default()
        },
        db_sync_token_advance_safe: false,
        holds: result.holds.clone(),
        sources: Vec::new(),
        operational_safe_holds: false,
    };
    assert_eq!(
        crate::cycle_reporter::CycleFacts::from_cycle_result(
            &interrupted,
            std::time::Duration::ZERO
        )
        .status,
        crate::cycle_reporter::CycleStatus::Interrupted
    );
    assert!(crate::sync_loop::runner::one_shot_partial_error(&interrupted).is_none());
}
