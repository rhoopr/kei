use super::{admit_retained_work, config_hash, has_due_retained_work};
use crate::commands::{AlbumPass, PassKind};
use crate::download::orchestration::models::{
    DownloadControls, DownloadReporting, DownloadRunMode, DownloadStore,
};
use crate::download::orchestration::test_support::incremental_photo_records;
use crate::download::planner::{TaskPlanner, pending_record_for_task};
use crate::icloud::photos::inbox::ShadowCapture;
use crate::icloud::photos::{PhotoAlbum, PhotoAlbumConfig, PhotosSession};
use crate::state::SqliteStateDb;
use crate::state::db::account::AccountOwner;
use crate::state::db::provider_selection::{MAX_SELECTION_BYTES, SelectionOutcome, SelectionPath};
use crate::state::db::provider_work::{WorkAdmission, WorkPlan};
use rustc_hash::FxHashSet;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
struct CurrentSession {
    records: Arc<Mutex<Vec<Value>>>,
    calls: Arc<AtomicUsize>,
    rank_calls: Arc<AtomicUsize>,
    corrupt: Arc<Mutex<Option<&'static str>>>,
    discovery: bool,
    zone_name: Arc<str>,
}

#[async_trait::async_trait]
impl PhotosSession for CurrentSession {
    async fn post(
        &self,
        url: &str,
        body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        if self.discovery && url.ends_with("/zones/list") {
            let zones = if url.contains("/private/") {
                json!([
                    {"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"}},
                    {"zoneID":{"zoneName":"SharedSync-queue","ownerRecordName":"_defaultOwner","zoneType":"REGULAR_CUSTOM_ZONE"}},
                    {"zoneID":{"zoneName":"SharedSync-no-owner"}},
                    {"zoneID":{"zoneName":"SharedSync-other-owner","ownerRecordName":"other"}}
                ])
            } else {
                json!([{"zoneID":{"zoneName":"SharedSync-foreign","ownerRecordName":"_defaultOwner"}}])
            };
            return Ok(json!({"zones":zones}));
        }
        if self.discovery && url.contains("/records/query?") {
            let request: Value = serde_json::from_str(&body)?;
            assert_eq!(request["query"]["recordType"], "CheckIndexingState");
            return Ok(
                json!({"records":[{"recordName":"index-state","fields":{"state":{"value":"FINISHED"}}}]}),
            );
        }
        if url.contains("/changes/zone?") {
            return Ok(
                json!({"zones":[{"zoneID":{"zoneName":self.zone_name.as_ref(),"ownerRecordName":"_defaultOwner"},"records":[],"syncToken":"quiet-zone-cursor","moreComing":false}]}),
            );
        }
        if url.contains("/internal/records/query/batch?") {
            return Ok(json!({"batch":[{"records":[{"fields":{"itemCount":{"value":0}}}]}]}));
        }
        anyhow::ensure!(
            url.contains("/records/lookup?"),
            "unexpected enumeration in catalog work"
        );
        self.calls.fetch_add(1, Ordering::SeqCst);
        let fault = *self.corrupt.lock().unwrap();
        if matches!(fault, Some("http401" | "http403" | "http421" | "http400")) {
            let endpoint =
                self.records.lock().unwrap()[0]["fields"]["resOriginalRes"]["value"]["downloadURL"]
                    .as_str()
                    .unwrap()
                    .to_owned();
            return PhotosSession::post(&reqwest::Client::new(), &endpoint, body, _headers).await;
        }
        let request: Value = serde_json::from_str(&body)?;
        assert_eq!(request["zoneID"]["zoneName"], self.zone_name.as_ref());
        assert_eq!(request["zoneID"]["ownerRecordName"], "_defaultOwner");
        let names: Vec<_> = request["records"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["recordName"].as_str().unwrap())
            .collect();
        let mut records: Vec<_> = self
            .records
            .lock()
            .unwrap()
            .iter()
            .filter(|r| names.contains(&r["recordName"].as_str().unwrap()))
            .cloned()
            .collect();
        match *self.corrupt.lock().unwrap() {
            Some("duplicate") => {
                if let Some(record) = records.first().cloned() {
                    records.push(record);
                }
            }
            Some("owner") => {
                for record in &mut records {
                    record["zoneID"] =
                        json!({"zoneName":"PrimarySync","ownerRecordName":"other-owner"});
                }
            }
            Some("pair") if names.len() == 2 => records
                .iter_mut()
                .filter(|r| r["recordType"] == "CPLAsset")
                .for_each(|r| {
                    r["fields"]["masterRef"]["value"]["recordName"] = json!("other-master")
                }),
            Some("missing") => records.clear(),
            Some("missing-c") => records
                .retain(|record| !matches!(record["recordName"].as_str(), Some("asset-c" | "c"))),
            Some("record_error") => records
                .iter_mut()
                .for_each(|r| r["serverErrorCode"] = json!("ACCESS_DENIED")),
            _ => {}
        }
        Ok(json!({"records":records,"futureResponse":{"precision":"1.2300e+30"}}))
    }
    async fn post_changes_body(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<Vec<u8>> {
        let response = self.post(url, body, headers).await?;
        let mut raw = serde_json::to_vec(&response)?;
        raw.pop();
        raw.extend_from_slice(b",\"futureExact\":1.2300e+30}");
        if *self.corrupt.lock().unwrap() == Some("duplicate_keys") {
            let mut duplicate = b"{\"records\":[],".to_vec();
            duplicate.extend_from_slice(&raw[1..]);
            return Ok(duplicate);
        }
        Ok(raw)
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

fn owner() -> AccountOwner {
    AccountOwner::authenticated(
        "queue-stage@example.invalid",
        "com",
        &serde_json::from_value(json!({"dsInfo":{"dsid":"queue-provider"}})).unwrap(),
    )
    .unwrap()
}
fn controls() -> DownloadControls {
    DownloadControls::new(DownloadRunMode::Download, DownloadReporting::hidden())
}
fn records(name: &str, checksum: &str, title: &str) -> Vec<Value> {
    let mut records = incremental_photo_records(name);
    records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(checksum);
    records[1]["fields"]["captionEnc"] = json!({"value":title,"type":"STRING"});
    records
}
const OLD: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
const CURRENT: &str = "AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=";

struct Fixture {
    dir: TempDir,
    db: Arc<SqliteStateDb>,
    capture: ShadowCapture,
    session: CurrentSession,
    pass: AlbumPass,
    config: super::DownloadConfig,
}
impl Fixture {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(
            SqliteStateDb::open_owned(&dir.path().join("state.db"), &owner())
                .await
                .unwrap(),
        );
        let capture = ShadowCapture::new(Arc::clone(&db), owner(), "com");
        let session = CurrentSession {
            records: Arc::new(Mutex::new(records("m", CURRENT, "current"))),
            calls: Arc::new(AtomicUsize::new(0)),
            rank_calls: Arc::new(AtomicUsize::new(0)),
            corrupt: Arc::new(Mutex::new(None)),
            discovery: false,
            zone_name: Arc::from("PrimarySync"),
        };
        let mut album = PhotoAlbum::new(
            PhotoAlbumConfig {
                params: Arc::new(std::collections::HashMap::new()),
                service_endpoint: Arc::from("https://example.invalid"),
                name: Arc::from(""),
                list_type: Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
                obj_type: Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
                query_filter: None,
                page_size: 100,
                zone_id: Arc::new(
                    json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner","zoneType":"REGULAR_CUSTOM_ZONE"}),
                ),
                retry_config: crate::retry::RetryConfig::default(),
                container_id: None,
                cross_zone_sources: Vec::new(),
            },
            Box::new(session.clone()),
        );
        album.set_shadow_capture(capture.clone(), Arc::from("private"));
        let pass = AlbumPass {
            kind: PassKind::Unfiled,
            album,
            exclude_ids: Arc::new(FxHashSet::default()),
        };
        let mut config = super::DownloadConfig::test_default();
        let media = dir.path().join("media");
        std::fs::create_dir(&media).unwrap();
        std::fs::write(media.join("existing.jpg"), b"existing-media").unwrap();
        std::fs::write(media.join("existing.xmp"), b"retained-sidecar").unwrap();
        config.directory = Arc::from(media.as_path());
        config.state_db = Some(Arc::clone(&db) as Arc<dyn DownloadStore>);
        db.acquire_lock("seed source cursor and debt").unwrap().execute_batch("INSERT INTO metadata(key,value) VALUES('sync_token:PrimarySync','old-cursor'),('pending_sync_token:sparse:OtherZone','retained-debt'); CREATE TABLE unknown_future(payload BLOB); INSERT INTO unknown_future VALUES(X'00FF07');").unwrap();
        Self {
            dir,
            db,
            capture,
            session,
            pass,
            config,
        }
    }
    async fn capture(&self, records: Vec<Value>, cursor: &str) {
        let (_, scope, zone) = self.pass.album.owned_private_scope().unwrap().unwrap();
        let raw=serde_json::to_vec(&json!({"zones":[{"zoneID":zone,"records":records,"syncToken":format!("{cursor}-successor"),"moreComing":false}]})).unwrap();
        self.capture
            .capture(crate::icloud::photos::catalog_observed_page(raw, &scope, cursor).unwrap())
            .await
            .unwrap();
    }
    async fn cycle(&self) -> anyhow::Result<()> {
        admit_retained_work(
            std::slice::from_ref(&self.pass),
            &self.config,
            controls(),
            &CancellationToken::new(),
        )
        .await
    }
    async fn plan(&self) -> WorkPlan {
        let (_, scope, zone) = self.pass.album.catalog_work_scope().unwrap().unwrap();
        let config = self.config.with_pass(&self.pass);
        let config_hash = config_hash(&config, &zone);
        // Independent replay fixture is selected directly from source PK, even
        // after the production scheduler correctly skips an admitted receipt.
        let (id,ordinal):(i64,i64)=self.db.acquire_lock("fixture source PK").unwrap().query_row("SELECT page_id,ordinal FROM provider_catalog_records WHERE kind='asset' ORDER BY page_id,ordinal LIMIT 1",[],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
        let page = self
            .db
            .catalog_source(
                owner(),
                crate::state::db::provider_inbox::CapturedPageId(id),
                16 * 1024 * 1024,
            )
            .await
            .unwrap();
        let current = self
            .pass
            .album
            .confirm_catalog_asset(&page.page.identities[usize::try_from(ordinal).unwrap()].name)
            .await
            .unwrap();
        let mut planner = TaskPlanner::for_download(config.state_db.as_deref())
            .await
            .unwrap();
        let tasks = planner
            .plan_download_asset(&current.asset, &config)
            .await
            .unwrap()
            .tasks;
        WorkPlan {
            source: crate::state::db::provider_work::WorkSource { page, ordinal },
            scope,
            zone,
            config_hash,
            confirmation: Some(current.body),
            master: Some(current.asset.id().to_owned()),
            records: tasks
                .iter()
                .map(|task| pending_record_for_task(&config, &current.asset, task))
                .collect(),
            reason: "",
        }
    }
    fn count(&self, table: &str) -> i64 {
        self.db
            .acquire_lock("fixture count")
            .unwrap()
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }
    fn queue(&self) -> Vec<String> {
        self.db.acquire_lock("independent queue oracle").unwrap().prepare("SELECT printf('%s|%s|%s|%s|%s|%s|%d|%s',id,checksum,status,title,metadata_hash,COALESCE(last_error,''),download_attempts,COALESCE(capture_repair_metadata_hash,'')) FROM assets ORDER BY library,id,version_size").unwrap().query_map([],|r|r.get(0)).unwrap().collect::<Result<_,_>>().unwrap()
    }
    async fn preserved(&self) {
        assert_eq!(
            std::fs::read(self.config.directory.join("existing.jpg")).unwrap(),
            b"existing-media"
        );
        assert_eq!(
            std::fs::read(self.config.directory.join("existing.xmp")).unwrap(),
            b"retained-sidecar"
        );
        assert_eq!(
            self.db
                .get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("old-cursor")
        );
        assert_eq!(
            self.db
                .get_metadata("pending_sync_token:sparse:OtherZone")
                .await
                .unwrap()
                .as_deref(),
            Some("retained-debt")
        );
        assert_eq!(
            self.db
                .acquire_lock("future bytes oracle")
                .unwrap()
                .query_row::<Vec<u8>, _, _>("SELECT payload FROM unknown_future", [], |r| r.get(0))
                .unwrap(),
            [0, 255, 7]
        );
    }
}

#[tokio::test]
async fn queue_projection_current_confirmation_then_two_reopens_have_no_repeat_admission() {
    let mut f = Fixture::new().await;
    f.capture(records("m", OLD, "old"), "old").await;
    f.cycle().await.unwrap();
    assert_eq!(f.count("assets"), 1);
    assert_eq!(f.count("provider_work_obligations"), 1);
    assert_eq!(f.count("provider_work_receipts"), 1);
    assert_eq!(f.count("asset_master_mappings"), 1);
    assert!(f.queue()[0].contains(CURRENT));
    assert!(f.queue()[0].contains("current"));
    let source: Vec<u8> =
        f.db.acquire_lock("raw source oracle")
            .unwrap()
            .query_row("SELECT body FROM provider_shadow_pages", [], |r| r.get(0))
            .unwrap();
    assert!(String::from_utf8(source).unwrap().contains(OLD));
    let confirmation: Vec<u8> =
        f.db.acquire_lock("exact current confirmation oracle")
            .unwrap()
            .query_row("SELECT confirmation FROM provider_work_receipts", [], |r| {
                r.get(0)
            })
            .unwrap();
    assert!(
        String::from_utf8(confirmation)
            .unwrap()
            .contains("\"futureExact\":1.2300e+30")
    );
    let before = f.queue();
    let calls = f.session.calls.load(Ordering::SeqCst);
    assert_eq!(calls, 2);
    for _ in 0..2 {
        let db = Arc::new(
            SqliteStateDb::open_owned(&f.dir.path().join("state.db"), &owner())
                .await
                .unwrap(),
        );
        f.db = db;
        f.capture = ShadowCapture::new(Arc::clone(&f.db), owner(), "com");
        f.pass
            .album
            .set_shadow_capture(f.capture.clone(), Arc::from("private"));
        f.config.state_db = Some(Arc::clone(&f.db) as Arc<dyn DownloadStore>);
        f.cycle().await.unwrap();
        assert_eq!(f.queue(), before);
        assert_eq!(f.session.calls.load(Ordering::SeqCst), calls);
        f.preserved().await;
    }
}

#[tokio::test]
async fn queue_projection_conflicting_retry_and_metadata_are_deferred_without_overwrite() {
    for changed_metadata_only in [false, true] {
        let f = Fixture::new().await;
        f.capture(records("m", OLD, "source"), "old").await;
        let mut plan = f.plan().await;
        let current = plan.records[0].clone();
        let mut older = current.clone();
        if changed_metadata_only {
            let mut meta = older.metadata.as_ref().clone();
            meta.title = Some("newer-metadata".into());
            meta.refresh_hash();
            older.metadata = Arc::new(meta);
        } else {
            older.checksum = OLD.into();
        }
        f.db.upsert_seen(&older).await.unwrap();
        f.db.acquire_lock("seed unfinished retry/publication").unwrap().execute_batch("UPDATE assets SET status='failed',last_error='keep-retry',download_attempts=4,metadata_write_failed_at=7,capture_repair_metadata_hash='prepared',capture_repair_output_checksum='output',capture_repair_output_size=20;").unwrap();
        let baseline = f.queue();
        assert_eq!(
            f.db.project_provider_work(owner(), plan, 512 * 1024 * 1024)
                .await
                .unwrap(),
            WorkAdmission::Deferred
        );
        assert_eq!(f.queue(), baseline);
        assert_eq!(f.count("provider_work_obligations"), 0);
        assert_eq!(f.count("asset_master_mappings"), 0);
        assert_eq!(
            f.db.acquire_lock("durable conflict debt")
                .unwrap()
                .query_row::<String, _, _>("SELECT reason FROM provider_work_receipts", [], |r| r
                    .get(0))
                .unwrap(),
            "queue_generation_conflict"
        );
        plan = f.plan().await;
        assert_eq!(
            f.db.project_provider_work(owner(), plan, 0).await.unwrap(),
            WorkAdmission::Deferred
        );
        assert_eq!(f.queue(), baseline);
        f.preserved().await;
    }
}

#[tokio::test]
async fn queue_projection_receipt_fault_rolls_back_real_queue_mapping_and_scan_writes() {
    let f = Fixture::new().await;
    f.capture(records("m", OLD, "source"), "old").await;
    f.db.acquire_lock("fault after publication writes").unwrap().execute_batch("CREATE TRIGGER work_fault BEFORE INSERT ON provider_work_receipts BEGIN SELECT RAISE(ABORT,'synthetic work receipt fault'); END;").unwrap();
    let error = f.cycle().await.unwrap_err();
    for table in [
        "assets",
        "asset_master_mappings",
        "provider_work_obligations",
        "provider_work_receipts",
        "provider_work_scan",
        "provider_work_retries",
    ] {
        assert_eq!(f.count(table), 0, "{table}");
    }
    assert_eq!(f.count("provider_shadow_pages"), 1);
    assert_eq!(f.count("provider_catalog_pages"), 1);
    assert!(error.to_string().contains("synthetic work receipt fault"));
    f.preserved().await;
    f.db.acquire_lock("remove reversible fault")
        .unwrap()
        .execute_batch("DROP TRIGGER work_fault")
        .unwrap();
    f.cycle().await.unwrap();
    let baseline = f.queue();
    f.cycle().await.unwrap();
    assert_eq!(f.queue(), baseline);
    f.preserved().await;
}

#[tokio::test]
async fn queue_projection_original_receipt_replay_and_shared_admission_never_regress_work() {
    let f = Fixture::new().await;
    f.capture(records("m", OLD, "source"), "old").await;
    let plan = f.plan().await;
    let record = plan.records[0].clone();
    assert_eq!(
        f.db.project_provider_work(owner(), plan, 512 * 1024 * 1024)
            .await
            .unwrap(),
        WorkAdmission::Admitted
    );
    f.db.acquire_lock("seed retry after admitted generation")
        .unwrap()
        .execute_batch("UPDATE assets SET last_error='retained-retry',download_attempts=3;")
        .unwrap();
    let baseline = f.queue();
    let mut stale = record.clone();
    stale.checksum = OLD.into();
    assert!(matches!(
        f.db.upsert_seen(&stale).await,
        Err(crate::state::error::StateError::ProviderWorkConflict)
    ));
    let mut oversized = record.clone();
    oversized.size_bytes = u64::MAX;
    assert!(matches!(
        f.db.upsert_seen(&oversized).await,
        Err(crate::state::error::StateError::ProviderWorkConflict)
    ));
    let mut stale_metadata = record.clone();
    let mut metadata = stale_metadata.metadata.as_ref().clone();
    metadata.title = Some("old metadata".into());
    metadata.refresh_hash();
    stale_metadata.metadata = Arc::new(metadata);
    assert!(matches!(
        f.db.upsert_seen(&stale_metadata).await,
        Err(crate::state::error::StateError::ProviderWorkConflict)
    ));
    assert_eq!(f.queue(), baseline);
    let plan = f.plan().await;
    assert_eq!(
        f.db.project_provider_work(owner(), plan, 0).await.unwrap(),
        WorkAdmission::Admitted
    );
    assert_eq!(f.queue(), baseline);
    // Once the original generation completed, the existing queue can move
    // forward. Its historical admission must still be a no-op on replay.
    let media = f.config.directory.join("completed.jpg");
    std::fs::write(&media, b"completed-generation").unwrap();
    f.db.mark_downloaded(
        "PrimarySync",
        "asset-m",
        record.version_size.as_str(),
        &media,
        "local",
        None,
    )
    .await
    .unwrap();
    f.db.upsert_seen(&stale).await.unwrap();
    let advanced = f.queue();
    let plan = f.plan().await;
    assert_eq!(
        f.db.project_provider_work(owner(), plan, 0).await.unwrap(),
        WorkAdmission::Admitted
    );
    assert_eq!(f.queue(), advanced);
    assert_eq!(std::fs::read(media).unwrap(), b"completed-generation");
    f.preserved().await;
}

#[tokio::test]
async fn queue_projection_scope_identity_source_and_capacity_refusals_preserve_queue() {
    for fault in ["owner", "zone", "source", "mapping", "legacy", "capacity"] {
        let f = Fixture::new().await;
        f.capture(records("m", OLD, "source"), "old").await;
        let mut plan = f.plan().await;
        let mut expected_owner = owner();
        let mut capacity = 512 * 1024 * 1024;
        match fault {
            "owner" => expected_owner = AccountOwner::configured("other@example.invalid", "com"),
            "zone" => plan.zone["ownerRecordName"] = json!("other-owner"),
            "source" => {
                f.db.acquire_lock("mutate after planning")
                    .unwrap()
                    .execute_batch("UPDATE provider_shadow_pages SET successor='wrong-source';")
                    .unwrap();
            }
            "mapping" => {
                f.db.upsert_asset_master_mapping("PrimarySync", "asset-m", "other-master")
                    .await
                    .unwrap();
            }
            "legacy" => {
                let mut old = plan.records[0].clone();
                old.id = "m".into();
                f.db.upsert_seen(&old).await.unwrap();
            }
            "capacity" => capacity = 0,
            _ => panic!("unknown synthetic fault: {fault}"),
        }
        let baseline = f.queue();
        let result =
            f.db.project_provider_work(expected_owner, plan, capacity)
                .await;
        if matches!(fault, "mapping" | "legacy") {
            assert_eq!(result.unwrap(), WorkAdmission::Deferred);
        } else {
            assert!(result.is_err(), "{fault}");
            assert_eq!(f.count("provider_work_receipts"), 0);
        }
        assert_eq!(f.queue(), baseline);
        assert_eq!(f.count("provider_work_obligations"), 0);
        f.preserved().await;
    }
}

#[tokio::test]
async fn queue_projection_lookup_failures_retain_fixed_debt_and_recover_current_generation() {
    for fault in [
        "duplicate",
        "owner",
        "pair",
        "missing",
        "record_error",
        "duplicate_keys",
    ] {
        let f = Fixture::new().await;
        f.capture(records("m", OLD, "source"), "old").await;
        *f.session.corrupt.lock().unwrap() = Some(fault);
        f.cycle().await.unwrap();
        assert_eq!(f.count("assets"), 0);
        assert_eq!(f.count("provider_work_obligations"), 0);
        assert_eq!(
            f.db.acquire_lock("lookup debt")
                .unwrap()
                .query_row::<String, _, _>("SELECT reason FROM provider_work_receipts", [], |r| r
                    .get(0))
                .unwrap(),
            "current_lookup_unresolved"
        );
        *f.session.corrupt.lock().unwrap() = None;
        assert!(
            !has_due_retained_work(std::slice::from_ref(&f.pass), &f.config, controls())
                .await
                .unwrap()
        );
        f.db.acquire_lock("synthetic retry clock")
            .unwrap()
            .execute_batch("UPDATE provider_work_retries SET last_attempt_at=0,next_retry_at=1")
            .unwrap();
        f.cycle().await.unwrap();
        assert_eq!(f.count("assets"), 1);
        assert_eq!(f.count("provider_work_obligations"), 1);
        let calls = f.session.calls.load(Ordering::SeqCst);
        f.cycle().await.unwrap();
        assert_eq!(f.session.calls.load(Ordering::SeqCst), calls);
        f.preserved().await;
    }
}

#[tokio::test]
async fn queue_projection_unsupported_selection_and_read_only_modes_do_not_publish() {
    for fault in [
        "recent",
        "retry_only",
        "repair",
        "album",
        "exclude",
        "dry_run",
        "print",
    ] {
        let mut f = Fixture::new().await;
        f.capture(records("m", OLD, "source"), "old").await;
        let mut controls = controls();
        match fault {
            "recent" => f.config.recent = Some(1),
            "retry_only" => f.config.retry_only = true,
            "repair" => f.config.refresh_metadata = true,
            "album" => f.pass.kind = PassKind::Album,
            "exclude" => {
                f.config.exclude_asset_ids = Arc::new(["other".into()].into_iter().collect())
            }
            "dry_run" => {
                controls =
                    DownloadControls::new(DownloadRunMode::DryRun, DownloadReporting::hidden())
            }
            "print" => {
                controls = DownloadControls::new(
                    DownloadRunMode::PrintFilenames,
                    DownloadReporting::hidden(),
                )
            }
            _ => panic!("unknown synthetic fault: {fault}"),
        }
        admit_retained_work(
            std::slice::from_ref(&f.pass),
            &f.config,
            controls,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(f.count("assets"), 0);
        assert_eq!(f.count("provider_work_receipts"), 0);
        assert_eq!(f.session.calls.load(Ordering::SeqCst), 0);
        assert!(
            !has_due_retained_work(std::slice::from_ref(&f.pass), &f.config, controls)
                .await
                .unwrap()
        );
        f.preserved().await;
    }
}

#[tokio::test]
async fn queue_projection_rejects_stale_planned_metadata_even_with_current_resource() {
    let f = Fixture::new().await;
    f.capture(records("m", OLD, "source"), "old").await;
    let mut plan = f.plan().await;
    let mut metadata = plan.records[0].metadata.as_ref().clone();
    metadata.title = Some("stale planned title".into());
    metadata.refresh_hash();
    plan.records[0].metadata = Arc::new(metadata);
    assert!(matches!(
        f.db.project_provider_work(owner(), plan, 512 * 1024 * 1024)
            .await,
        Err(crate::state::error::StateError::ProviderWorkInvalid)
    ));
    assert_eq!(f.count("assets"), 0);
    assert_eq!(f.count("provider_work_receipts"), 0);
    f.preserved().await;
}

#[tokio::test]
async fn queue_projection_schema30_migration_and_conflict_preserve_unknown_history() {
    for conflict in [false, true] {
        let f = Fixture::new().await;
        f.capture(records("m", OLD, "source"), "old").await;
        f.db.acquire_lock("released schema30 fixture").unwrap().execute_batch("DROP TABLE provider_work_obligations; DROP TABLE provider_work_receipts; DROP TABLE provider_work_scan; PRAGMA user_version=30;").unwrap();
        if conflict {
            f.db.acquire_lock("unknown work schema conflict").unwrap().execute_batch("CREATE TABLE provider_work_receipts(page_id INTEGER,ordinal INTEGER,config_hash TEXT,confirmation_hash TEXT,confirmation BLOB,state TEXT,reason TEXT,scope TEXT,source_hash TEXT,master_record_name TEXT,charged_bytes INTEGER,future_blob BLOB); INSERT INTO provider_work_receipts(future_blob) VALUES(X'01FE');").unwrap();
        }
        for _ in 0..2 {
            let opened = SqliteStateDb::open_owned(&f.dir.path().join("state.db"), &owner()).await;
            if conflict {
                assert!(opened.is_err());
                let conn = rusqlite::Connection::open(f.dir.path().join("state.db")).unwrap();
                assert_eq!(
                    conn.pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))
                        .unwrap(),
                    30
                );
                assert_eq!(
                    conn.query_row::<Vec<u8>, _, _>(
                        "SELECT future_blob FROM provider_work_receipts",
                        [],
                        |r| r.get(0)
                    )
                    .unwrap(),
                    [1, 254]
                );
                assert_eq!(conn.query_row::<i64,_,_>("SELECT count(*) FROM sqlite_schema WHERE name IN ('provider_work_scan','provider_work_obligations')",[],|r|r.get(0)).unwrap(),0);
            } else {
                let opened = opened.unwrap();
                assert_eq!(
                    opened
                        .acquire_lock("version oracle")
                        .unwrap()
                        .pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))
                        .unwrap(),
                    i64::from(crate::state::schema::SCHEMA_VERSION)
                );
            }
            f.preserved().await;
        }
    }
}

#[tokio::test]
async fn queue_projection_cancelled_attempt_leaves_source_and_no_admission() {
    let f = Fixture::new().await;
    f.capture(records("m", OLD, "source"), "old").await;
    let cancel = CancellationToken::new();
    cancel.cancel();
    admit_retained_work(
        std::slice::from_ref(&f.pass),
        &f.config,
        controls(),
        &cancel,
    )
    .await
    .unwrap();
    assert_eq!(f.count("assets"), 0);
    assert_eq!(f.count("provider_work_scan"), 0);
    assert_eq!(f.session.calls.load(Ordering::SeqCst), 0);
    f.cycle().await.unwrap();
    assert_eq!(f.count("assets"), 1);
    f.preserved().await;
}

#[tokio::test]
async fn queue_projection_current_filters_retain_source_then_replan_changed_configuration() {
    let mut f = Fixture::new().await;
    f.capture(records("m", OLD, "source"), "old").await;
    f.config.filename_exclude = Arc::from([glob::Pattern::new("changed.jpg").unwrap()]);
    f.cycle().await.unwrap();
    assert_eq!(f.count("assets"), 0);
    assert_eq!(
        f.db.acquire_lock("filter evidence")
            .unwrap()
            .query_row::<String, _, _>("SELECT reason FROM provider_work_receipts", [], |r| r
                .get(0))
            .unwrap(),
        "currently_filtered"
    );
    f.config.filename_exclude = Arc::from([]);
    f.cycle().await.unwrap();
    assert_eq!(f.count("assets"), 1);
    assert_eq!(f.count("provider_shadow_pages"), 1);
    assert_eq!(
        f.db.acquire_lock("selection generations retained")
            .unwrap()
            .query_row::<i64, _, _>(
                "SELECT count(DISTINCT config_hash) FROM provider_work_receipts",
                [],
                |r| r.get(0)
            )
            .unwrap(),
        2
    );
    let calls = f.session.calls.load(Ordering::SeqCst);
    f.cycle().await.unwrap();
    assert_eq!(f.session.calls.load(Ordering::SeqCst), calls);
    f.preserved().await;
}

#[tokio::test]
async fn queue_projection_hidden_current_asset_retains_source_without_queue_work() {
    let f = Fixture::new().await;
    f.capture(records("m", OLD, "source"), "old").await;
    f.session.records.lock().unwrap()[1]["fields"]["isHidden"] = json!({"value":1,"type":"INT64"});
    f.cycle().await.unwrap();
    assert_eq!(f.count("assets"), 0);
    assert_eq!(
        f.db.acquire_lock("current hidden evidence")
            .unwrap()
            .query_row::<String, _, _>("SELECT reason FROM provider_work_receipts", [], |r| r
                .get(0))
            .unwrap(),
        "current_library_ineligible"
    );
    f.preserved().await;
}

#[tokio::test]
async fn queue_projection_dispatch_fault_holds_cursor_and_does_not_rank_fallback() {
    let mut f = Fixture::new().await;
    f.capture(records("m", OLD, "source"), "old").await;
    f.config.sync_mode = crate::download::SyncMode::Incremental {
        zone_sync_token: "old-cursor".into(),
    };
    f.db.acquire_lock("dispatch fault before receipt").unwrap().execute_batch("CREATE TRIGGER work_fault BEFORE INSERT ON provider_work_receipts BEGIN SELECT RAISE(ABORT,'synthetic dispatch work fault'); END;").unwrap();
    for _ in 0..2 {
        let result = crate::download::download_photos_with_sync(
            &reqwest::Client::new(),
            std::slice::from_ref(&f.pass),
            Arc::new(f.config.clone()),
            controls(),
            CancellationToken::new(),
        )
        .await;
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("synthetic dispatch work fault")
        );
        assert_eq!(f.count("assets"), 0);
        assert_eq!(f.count("provider_work_receipts"), 0);
        f.preserved().await;
    }
    f.db.acquire_lock("restore fixture")
        .unwrap()
        .execute_batch("DROP TRIGGER work_fault")
        .unwrap();
    f.cycle().await.unwrap();
    let baseline = f.queue();
    f.cycle().await.unwrap();
    assert_eq!(f.queue(), baseline);
    f.preserved().await;
}

#[tokio::test]
async fn queue_projection_bounded_scan_does_not_starve_later_sources() {
    let f = Fixture::new().await;
    let mut original = Vec::new();
    let mut current = Vec::new();
    for index in 0..66 {
        let name = format!("m-{index}");
        original.extend(records(&name, OLD, "source"));
        if index != 0 {
            current.extend(records(&name, CURRENT, "current"));
        }
    }
    *f.session.records.lock().unwrap() = current;
    f.capture(original, "many").await;
    f.cycle().await.unwrap();
    assert_eq!(f.count("assets"), 63);
    f.cycle().await.unwrap();
    assert_eq!(f.count("assets"), 65);
    f.cycle().await.unwrap();
    assert_eq!(f.count("assets"), 65);
    assert_eq!(f.db.acquire_lock("early source still unresolved").unwrap().query_row::<i64,_,_>("SELECT count(*) FROM provider_work_receipts WHERE state='deferred' AND reason='current_lookup_unresolved'",[],|r|r.get(0)).unwrap(),1);
    f.preserved().await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "isolated subprocess controlled by the queue work process-death parent"]
async fn queue_projection_process_death_child() {
    assert!(std::env::var_os("KEI_TEST_WORK_COMMIT_PAUSE").is_some());
    let f = Fixture::new().await;
    f.capture(records("m", OLD, "source"), "old").await;
    f.cycle().await.unwrap();
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn queue_projection_process_death_before_receipt_rolls_back_and_recovers_twice() {
    let root = tempfile::tempdir().unwrap();
    let marker = root.path().join("paused");
    let mut child=std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact","download::orchestration::queue_projection::tests::queue_projection_process_death_child","--ignored","--nocapture"])
        .env("TMPDIR",root.path()).env("KEI_TEST_WORK_COMMIT_PAUSE",&marker)
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().unwrap();
    let mut database = None;
    for _ in 0..400 {
        if let Ok(bytes) = std::fs::read(&marker) {
            database = Some(std::path::PathBuf::from(String::from_utf8(bytes).unwrap()));
            break;
        }
        if let Some(status) = child.try_wait().unwrap() {
            panic!("child exited before real write pause: {status}");
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    if database.is_none() {
        child.kill().unwrap();
        child.wait().unwrap();
        panic!("work child did not reach receipt boundary");
    }
    let database = database.unwrap();
    let conn = rusqlite::Connection::open(&database).unwrap();
    assert_eq!(
        conn.query_row::<i64, _, _>("SELECT count(*) FROM provider_shadow_pages", [], |r| r
            .get(0))
            .unwrap(),
        1
    );
    for table in [
        "assets",
        "asset_master_mappings",
        "provider_work_obligations",
        "provider_work_receipts",
        "provider_work_scan",
    ] {
        assert_eq!(
            conn.query_row::<i64, _, _>(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
                .unwrap(),
            0,
            "{table}"
        );
    }
    drop(conn);
    child.kill().unwrap();
    assert!(!child.wait().unwrap().success());
    let mut f = Fixture::new().await;
    f.config.directory = Arc::from(database.parent().unwrap().join("media").as_path());
    for reopen in 0..2 {
        f.db = Arc::new(
            SqliteStateDb::open_owned(&database, &owner())
                .await
                .unwrap(),
        );
        f.capture = ShadowCapture::new(Arc::clone(&f.db), owner(), "com");
        f.pass
            .album
            .set_shadow_capture(f.capture.clone(), Arc::from("private"));
        f.config.state_db = Some(Arc::clone(&f.db) as Arc<dyn DownloadStore>);
        if reopen == 0 {
            assert_eq!(f.count("assets"), 0);
            assert_eq!(f.count("provider_work_obligations"), 0);
        }
        let calls = f.session.calls.load(Ordering::SeqCst);
        f.cycle().await.unwrap();
        assert_eq!(f.count("assets"), 1);
        assert_eq!(f.count("provider_work_obligations"), 1);
        if reopen == 1 {
            assert_eq!(f.session.calls.load(Ordering::SeqCst), calls);
        }
        f.preserved().await;
    }
}

#[tokio::test]
async fn queue_projection_mapping_and_metadata_fences_cover_independent_publication_debt() {
    for independent_path in [false, true] {
        let f = Fixture::new().await;
        f.capture(records("m", OLD, "source"), "old").await;
        let plan = f.plan().await;
        let record = plan.records[0].clone();
        assert_eq!(
            f.db.project_provider_work(owner(), plan, 512 * 1024 * 1024)
                .await
                .unwrap(),
            WorkAdmission::Admitted
        );
        let media = f.config.directory.join("completed.jpg");
        std::fs::write(&media, b"original-published-bytes").unwrap();
        f.db.mark_downloaded(
            "PrimarySync",
            "asset-m",
            record.version_size.as_str(),
            &media,
            "local",
            None,
        )
        .await
        .unwrap();
        if independent_path {
            f.db.acquire_lock("seed independent publication debt").unwrap().execute("INSERT INTO asset_metadata_paths(library,id,version_size,local_path,provider_checksum,local_checksum,source_checksum,metadata_write_failed_at) VALUES ('PrimarySync','asset-m','original','independent-copy.jpg',?1,'copy-local','copy-source',7)",[CURRENT]).unwrap();
        } else {
            f.db.acquire_lock("seed canonical publication debt")
                .unwrap()
                .execute_batch("UPDATE assets SET metadata_write_failed_at=7;")
                .unwrap();
        }
        let baseline = f.queue();
        assert!(matches!(
            f.db.upsert_asset_master_mapping("PrimarySync", "asset-m", "new-master")
                .await,
            Err(crate::state::error::StateError::ProviderWorkConflict)
        ));
        assert_eq!(
            f.db.get_master_record_name_for_asset("PrimarySync", "asset-m")
                .await
                .unwrap()
                .as_deref(),
            Some("m")
        );
        let changed = records("m", CURRENT, "later-metadata");
        let asset = crate::icloud::photos::PhotoAsset::new(changed[0].clone(), changed[1].clone());
        let capture = crate::download::filter::metadata_capture(&asset);
        let result =
            f.db.refresh_downloaded_asset_metadata(
                "PrimarySync",
                "asset-m",
                (&capture, asset.created(), Some(asset.added_date())),
                true,
                false,
                1,
            )
            .await;
        assert!(matches!(
            result,
            Err(crate::state::error::StateError::ProviderWorkConflict)
        ));
        assert_eq!(f.queue(), baseline);
        assert_eq!(std::fs::read(media).unwrap(), b"original-published-bytes");
        f.preserved().await;
    }
}

#[tokio::test]
async fn queue_projection_requires_current_authenticated_pin_and_matching_database_context() {
    let f = Fixture::new().await;
    f.capture(records("m", OLD, "source"), "old").await;
    let plan = f.plan().await;
    assert!(matches!(
        f.db.project_provider_work(
            AccountOwner::configured("queue-stage@example.invalid", "com"),
            plan,
            512 * 1024 * 1024
        )
        .await,
        Err(crate::state::error::StateError::AccountIdentityUnavailable)
    ));
    let plan = f.plan().await;
    let other = AccountOwner::authenticated(
        "queue-stage@example.invalid",
        "com",
        &serde_json::from_value(json!({"dsInfo":{"dsid":"other-provider"}})).unwrap(),
    )
    .unwrap();
    assert!(matches!(
        f.db.project_provider_work(other, plan, 512 * 1024 * 1024)
            .await,
        Err(crate::state::error::StateError::AccountOwnerMismatch)
    ));
    let mut config = f.config.clone();
    let other_db = Arc::new(
        SqliteStateDb::open_owned(&f.dir.path().join("other.db"), &owner())
            .await
            .unwrap(),
    );
    config.state_db = Some(other_db as Arc<dyn DownloadStore>);
    assert!(
        admit_retained_work(
            std::slice::from_ref(&f.pass),
            &config,
            controls(),
            &CancellationToken::new()
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("database context mismatch")
    );
    assert_eq!(f.count("assets"), 0);
    assert_eq!(f.count("provider_work_receipts"), 0);
    f.preserved().await;
}

#[tokio::test]
async fn queue_projection_companion_obligations_commit_all_or_defer_all() {
    for conflict in [false, true] {
        let f = Fixture::new().await;
        f.capture(records("m", OLD, "source"), "old").await;
        {
            let mut current = f.session.records.lock().unwrap();
            current[0]["fields"]["resOriginalVidComplRes"] = json!({"value":{"downloadURL":"https://p01.icloud-content.com/motion.mov","size":2048,"fileChecksum":OLD}});
            current[0]["fields"]["resOriginalVidComplFileType"] =
                json!({"value":"com.apple.quicktime-movie"});
        }
        let plan = f.plan().await;
        assert_eq!(plan.records.len(), 2);
        if conflict {
            let mut existing = plan.records[0].clone();
            existing.checksum = "prior-generation".into();
            f.db.upsert_seen(&existing).await.unwrap();
        }
        let baseline = f.queue();
        let result =
            f.db.project_provider_work(owner(), plan, 512 * 1024 * 1024)
                .await
                .unwrap();
        if conflict {
            assert_eq!(result, WorkAdmission::Deferred);
            assert_eq!(f.queue(), baseline);
            assert_eq!(f.count("provider_work_obligations"), 0);
        } else {
            assert_eq!(result, WorkAdmission::Admitted);
            assert_eq!(f.count("assets"), 2);
            assert_eq!(f.count("provider_work_obligations"), 2);
        }
        f.preserved().await;
    }
}

#[tokio::test]
async fn queue_projection_raw_alignment_preserves_current_provider_metadata_binding() {
    let mut f = Fixture::new().await;
    f.capture(records("m", OLD, "source"), "old").await;
    {
        let mut current = f.session.records.lock().unwrap();
        current[0]["fields"]["filenameEnc"] = json!({"value":"current.dng","type":"STRING"});
        current[0]["fields"]["resOriginalFileType"] = json!({"value":"com.adobe.raw-image"});
        current[0]["fields"]["resOriginalAltRes"] = json!({"value":{"downloadURL":"https://p01.icloud-content.com/current.jpg","size":2048,"fileChecksum":OLD}});
        current[0]["fields"]["resOriginalAltFileType"] = json!({"value":"public.jpeg"});
        current[0]["fields"]["resOriginalAltWidth"] = json!({"value":200});
        current[0]["fields"]["resOriginalAltHeight"] = json!({"value":300});
    }
    f.config.raw_policy = crate::types::RawPolicy::PreferJpeg;
    f.cycle().await.unwrap();
    assert_eq!(
        f.db.acquire_lock("logical original uses confirmed JPEG facts")
            .unwrap()
            .query_row::<(String, i64, i64), _, _>(
                "SELECT checksum,width,height FROM assets WHERE version_size='original'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            )
            .unwrap(),
        (OLD.into(), 200, 300)
    );
    f.preserved().await;
}

#[tokio::test]
async fn queue_projection_completed_receipt_refuses_corrupted_obligation() {
    let f = Fixture::new().await;
    f.capture(records("m", OLD, "source"), "old").await;
    f.cycle().await.unwrap();
    let baseline = f.queue();
    f.db.acquire_lock("receipt evidence corruption")
        .unwrap()
        .execute_batch("UPDATE provider_work_obligations SET checksum='wrong-proof';")
        .unwrap();
    let plan = f.plan().await;
    assert!(matches!(
        f.db.project_provider_work(owner(), plan, 0).await,
        Err(crate::state::error::StateError::ProviderWorkInvalid)
    ));
    assert_eq!(f.queue(), baseline);
    f.preserved().await;
}

#[tokio::test]
async fn queue_projection_derived_plan_budget_rejects_without_partial_admission() {
    let f = Fixture::new().await;
    f.capture(records("m", OLD, "source"), "old").await;
    let mut plan = f.plan().await;
    let mut body = plan.confirmation.take().unwrap();
    assert_eq!(body.pop(), Some(b'}'));
    body.extend_from_slice(b",\"futurePadding\":\"");
    body.resize(16 * 1024 * 1024 - 18, b'x');
    body.extend_from_slice(b"\"}");
    plan.confirmation = Some(body);
    assert!(matches!(
        f.db.project_provider_work(owner(), plan, 512 * 1024 * 1024)
            .await,
        Err(crate::state::error::StateError::ProviderWorkFull)
    ));
    for table in [
        "assets",
        "asset_master_mappings",
        "provider_work_obligations",
        "provider_work_receipts",
        "provider_work_scan",
        "provider_work_retries",
    ] {
        assert_eq!(f.count(table), 0, "{table}");
    }
    f.preserved().await;
}

#[tokio::test]
async fn queue_projection_same_generation_collision_replans_without_stranding_work() {
    let f = Fixture::new().await;
    f.capture(records("m", OLD, "source"), "old").await;
    let current = f.pass.album.confirm_catalog_asset("asset-m").await.unwrap();
    let config = f.config.with_pass(&f.pass);
    let mut planner = TaskPlanner::for_download(config.state_db.as_deref())
        .await
        .unwrap();
    let initial = planner
        .plan_download_asset(&current.asset, &config)
        .await
        .unwrap();
    let original_path = initial.tasks.first().unwrap().download_path.clone();
    f.cycle().await.unwrap();
    std::fs::create_dir_all(original_path.parent().unwrap()).unwrap();
    std::fs::write(&original_path, b"independent-conflicting-file").unwrap();
    for _ in 0..2 {
        let mut planner = TaskPlanner::for_download(config.state_db.as_deref())
            .await
            .unwrap();
        let plan = planner
            .plan_download_asset(&current.asset, &config)
            .await
            .unwrap();
        let task = plan.tasks.first().unwrap();
        assert_ne!(task.download_path, original_path);
        crate::download::planner::upsert_seen_for_task(&*f.db, &config, &current.asset, task)
            .await
            .unwrap();
        assert_eq!(f.count("assets"), 1);
        assert_eq!(f.count("provider_work_obligations"), 1);
        assert_eq!(
            std::fs::read(&original_path).unwrap(),
            b"independent-conflicting-file"
        );
        f.preserved().await;
    }
    let calls = f.session.calls.load(Ordering::SeqCst);
    f.cycle().await.unwrap();
    assert_eq!(f.session.calls.load(Ordering::SeqCst), calls);
}

#[tokio::test]
async fn queue_projection_actual_discovery_qualifies_primary_and_private_shared() {
    let mut f = Fixture::new().await;
    f.session.discovery = true;
    f.session.zone_name = Arc::from("SharedSync-queue");
    for record in f.session.records.lock().unwrap().iter_mut() {
        if let Some(zone) = record.pointer_mut("/fields/masterRef/value/zoneID") {
            *zone = json!({"zoneName":"SharedSync-queue","ownerRecordName":"_defaultOwner"});
        }
    }

    let mut service = crate::icloud::photos::PhotosService::new(
        "https://example.invalid".into(),
        Box::new(f.session.clone()),
        std::collections::HashMap::new(),
        crate::retry::RetryConfig::default(),
    )
    .await
    .unwrap();
    service.set_shadow_capture(f.capture.clone());
    let libraries = service.all_libraries().await.unwrap();
    assert_eq!(
        libraries
            .iter()
            .filter(|library| library.zone_name() == "PrimarySync")
            .count(),
        1
    );
    for library in &libraries {
        let eligible = matches!(library.zone_name(), "PrimarySync" | "SharedSync-queue");
        assert_eq!(
            library.all().catalog_work_scope().unwrap().is_some(),
            eligible,
            "{}",
            library.zone_name()
        );
    }
    f.pass.album = service.get_library("PrimarySync").await.unwrap().all();
    assert!(f.pass.album.catalog_work_scope().unwrap().is_some());
    // No primary source exists, so qualified ownership alone admits nothing.
    f.cycle().await.unwrap();
    assert_eq!(f.session.calls.load(Ordering::SeqCst), 0);
    assert_eq!(f.count("provider_work_receipts"), 0);
    f.pass.album = service.get_library("SharedSync-queue").await.unwrap().all();
    f.config.library = Arc::from("SharedSync-queue");
    let mut source = records("m", OLD, "source");
    for record in &mut source {
        if let Some(zone) = record.pointer_mut("/fields/masterRef/value/zoneID") {
            *zone = json!({"zoneName":"SharedSync-queue","ownerRecordName":"_defaultOwner"});
        }
    }
    f.capture(source, "discovered").await;
    f.cycle().await.unwrap();
    assert_eq!(f.count("assets"), 1);
    assert_eq!(f.count("provider_work_receipts"), 1);
    assert_eq!(f.count("provider_work_obligations"), 1);
    assert_eq!(f.session.calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        f.db.acquire_lock("discovered scope oracle")
            .unwrap()
            .query_row::<String, _, _>("SELECT library FROM assets", [], |row| row.get(0))
            .unwrap(),
        "SharedSync-queue"
    );
    f.cycle().await.unwrap();
    assert_eq!(f.session.calls.load(Ordering::SeqCst), 2);
    f.preserved().await;
}

#[tokio::test]
async fn queue_projection_default_primary_uses_discovered_owner_and_retained_source() {
    let mut f = Fixture::new().await;
    f.session.discovery = true;
    f.capture(records("m", OLD, "retained-old"), "primary-history")
        .await;
    // An ownerless observation remains separate history, never inferred into
    // the qualified scope or counted as admitted work.
    let ownerless_zone = json!({"zoneName":"PrimarySync"});
    let ownerless_scope = f.capture.scope("private", &ownerless_zone).unwrap();
    let ownerless_body = serde_json::to_vec(&json!({"zones":[{"zoneID":ownerless_zone,"records":records("ownerless",OLD,"unresolved-history"),"syncToken":"ownerless-successor","moreComing":false}]})).unwrap();
    f.capture
        .capture(
            crate::icloud::photos::catalog_observed_page(
                ownerless_body.clone(),
                &ownerless_scope,
                "ownerless-before",
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let source_count = f.count("provider_shadow_pages");
    let debt_count = f.count("provider_catalog_debt");
    let mut service = crate::icloud::photos::PhotosService::new(
        "https://example.invalid".into(),
        Box::new(f.session.clone()),
        std::collections::HashMap::new(),
        crate::retry::RetryConfig::default(),
    )
    .await
    .unwrap();
    // Actual startup resolves the default selection before attaching capture.
    let selected = crate::commands::resolve_libraries(
        &crate::selection::LibrarySelector::default(),
        &mut service,
    )
    .await
    .unwrap();
    assert_eq!(selected.len(), 1);
    service.set_shadow_capture(f.capture.clone());
    f.pass.album = service.get_library("PrimarySync").await.unwrap().all();
    assert!(
        f.pass.album.catalog_work_scope().unwrap().is_some(),
        "default primary must use explicit authenticated private discovery evidence"
    );
    // Fault after real queue writes must roll back even on the newly reachable
    // default primary route, preserving the existing checkpoint and debt.
    f.db.acquire_lock("primary receipt fault").unwrap().execute_batch("CREATE TRIGGER primary_receipt_fault BEFORE INSERT ON provider_work_receipts BEGIN SELECT RAISE(ABORT,'primary receipt fault'); END;").unwrap();
    assert!(
        f.cycle()
            .await
            .unwrap_err()
            .to_string()
            .contains("primary receipt fault")
    );
    for table in [
        "assets",
        "asset_master_mappings",
        "provider_work_obligations",
        "provider_work_receipts",
        "provider_work_scan",
        "provider_work_retries",
    ] {
        assert_eq!(f.count(table), 0, "{table}");
    }
    assert_eq!(f.count("provider_shadow_pages"), source_count);
    assert_eq!(f.count("provider_catalog_debt"), debt_count);
    f.preserved().await;
    f.db.acquire_lock("remove synthetic primary fault")
        .unwrap()
        .execute_batch("DROP TRIGGER primary_receipt_fault;")
        .unwrap();
    f.cycle().await.unwrap();
    assert_eq!(f.count("assets"), 1);
    assert_eq!(f.count("provider_work_receipts"), 1);
    assert_eq!(f.count("provider_work_obligations"), 1);
    assert!(f.queue()[0].contains(CURRENT));
    assert!(f.queue()[0].contains("current"));
    let queue = f.queue();
    let calls = f.session.calls.load(Ordering::SeqCst);
    assert_eq!(calls, 4);
    for _ in 0..2 {
        f.db = Arc::new(
            SqliteStateDb::open_owned(&f.dir.path().join("state.db"), &owner())
                .await
                .unwrap(),
        );
        f.capture = ShadowCapture::new(Arc::clone(&f.db), owner(), "com");
        f.config.state_db = Some(Arc::clone(&f.db) as Arc<dyn DownloadStore>);
        // A fresh service requalifies ownership, as process restart does.
        let mut service = crate::icloud::photos::PhotosService::new(
            "https://example.invalid".into(),
            Box::new(f.session.clone()),
            std::collections::HashMap::new(),
            crate::retry::RetryConfig::default(),
        )
        .await
        .unwrap();
        service.set_shadow_capture(f.capture.clone());
        let libraries = crate::commands::resolve_libraries(
            &crate::selection::LibrarySelector::default(),
            &mut service,
        )
        .await
        .unwrap();
        f.pass.album = libraries[0].all();
        f.cycle().await.unwrap();
        assert_eq!(f.queue(), queue);
        assert_eq!(f.session.calls.load(Ordering::SeqCst), calls);
        assert_eq!(f.count("provider_work_receipts"), 1);
        assert_eq!(f.count("provider_shadow_pages"), source_count);
        assert_eq!(f.count("provider_catalog_debt"), debt_count);
        let retained: Vec<u8> =
            f.db.acquire_lock("ownerless history oracle")
                .unwrap()
                .query_row(
                    "SELECT body FROM provider_shadow_pages WHERE scope=?1",
                    [&ownerless_scope],
                    |row| row.get(0),
                )
                .unwrap();
        assert_eq!(retained, ownerless_body);
        f.preserved().await;
    }
}

#[tokio::test]
async fn queue_projection_retry_deadline_survives_reopen_is_bounded_and_is_not_completion() {
    let f = Fixture::new().await;
    f.capture(records("m", OLD, "source"), "old").await;
    *f.session.corrupt.lock().unwrap() = Some("missing");
    for attempt in 1..=7 {
        assert!(
            has_due_retained_work(std::slice::from_ref(&f.pass), &f.config, controls())
                .await
                .unwrap()
        );
        f.cycle().await.unwrap();
        let (attempts, last, next): (u32, i64, i64) =
            f.db.acquire_lock("retry schedule")
                .unwrap()
                .query_row(
                    "SELECT attempts,last_attempt_at,next_retry_at FROM provider_work_retries",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .unwrap();
        assert_eq!(attempts, attempt);
        assert_eq!(
            next - last,
            (3600 * (1_i64 << (attempt - 1).min(5))).min(86400)
        );
        let reopened = SqliteStateDb::open_owned(&f.dir.path().join("state.db"), &owner())
            .await
            .unwrap();
        let (_, scope, zone) = f.pass.album.catalog_work_scope().unwrap().unwrap();
        let hash = config_hash(&f.config.with_pass(&f.pass), &zone);
        assert!(
            !reopened
                .has_due_provider_work(owner(), scope, hash)
                .await
                .unwrap()
        );
        let calls = f.session.calls.load(Ordering::SeqCst);
        f.cycle().await.unwrap();
        assert_eq!(f.session.calls.load(Ordering::SeqCst), calls);
        assert_eq!(f.count("assets"), 0);
        assert_eq!(f.count("provider_work_obligations"), 0);
        f.preserved().await;
        f.db.acquire_lock("synthetic due deadline")
            .unwrap()
            .execute_batch("UPDATE provider_work_retries SET last_attempt_at=0,next_retry_at=1")
            .unwrap();
    }
    *f.session.corrupt.lock().unwrap() = None;
    f.cycle().await.unwrap();
    assert_eq!(f.count("assets"), 1);
    assert_eq!(f.count("provider_work_retries"), 0);
    assert!(
        has_due_retained_work(std::slice::from_ref(&f.pass), &f.config, controls())
            .await
            .unwrap(),
        "admission before dispatch remains local queue work"
    );
}

#[tokio::test]
async fn queue_projection_due_early_retry_is_revisited_behind_scan_and_new_tail() {
    let f = Fixture::new().await;
    f.capture(records("early", OLD, "source"), "early").await;
    *f.session.corrupt.lock().unwrap() = Some("missing");
    f.cycle().await.unwrap();
    *f.session.corrupt.lock().unwrap() = None;
    let mut current = Vec::new();
    let mut original = Vec::new();
    for index in 0..70 {
        current.extend(records(&format!("late-{index}"), CURRENT, "current"));
        original.extend(records(&format!("late-{index}"), OLD, "source"));
    }
    *f.session.records.lock().unwrap() = current;
    f.capture(original, "late").await;
    f.cycle().await.unwrap();
    assert_eq!(f.count("assets"), 64);
    f.db.acquire_lock("due behind historical scan")
        .unwrap()
        .execute_batch("UPDATE provider_work_retries SET last_attempt_at=0,next_retry_at=1")
        .unwrap();
    let (_, scope, zone) = f.pass.album.catalog_work_scope().unwrap().unwrap();
    let source =
        f.db.next_work_source(
            owner(),
            scope,
            config_hash(&f.config.with_pass(&f.pass), &zone),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        source.page.page.identities[usize::try_from(source.ordinal).unwrap()].name,
        "asset-early"
    );
    f.cycle().await.unwrap();
    assert_eq!(f.count("assets"), 70);
    assert_eq!(
        f.db.acquire_lock("due attempt oracle")
            .unwrap()
            .query_row::<u32, _, _>("SELECT attempts FROM provider_work_retries", [], |r| r
                .get(0))
            .unwrap(),
        2
    );
    f.preserved().await;
}

#[tokio::test]
async fn queue_projection_wake_requires_current_scope_config_and_unfinished_generation() {
    let mut f = Fixture::new().await;
    f.capture(records("m", OLD, "source"), "old").await;
    assert!(
        has_due_retained_work(std::slice::from_ref(&f.pass), &f.config, controls())
            .await
            .unwrap()
    );
    f.cycle().await.unwrap();
    assert!(
        has_due_retained_work(std::slice::from_ref(&f.pass), &f.config, controls())
            .await
            .unwrap()
    );
    let (_, scope, zone) = f.pass.album.catalog_work_scope().unwrap().unwrap();
    let hash = config_hash(&f.config.with_pass(&f.pass), &zone);
    assert!(
        !f.db
            .has_due_provider_work(owner(), "different-scope".into(), hash.clone())
            .await
            .unwrap()
    );
    let mut unselected = f.plan().await;
    unselected.config_hash = "f".repeat(64);
    unselected.records.clear();
    unselected.reason = "currently_filtered";
    assert!(matches!(
        f.db.project_provider_work(owner(), unselected, 512 * 1024 * 1024)
            .await
            .unwrap(),
        WorkAdmission::Deferred
    ));
    assert!(
        !f.db
            .has_due_provider_work(owner(), scope.clone(), "f".repeat(64))
            .await
            .unwrap()
    );
    f.db.acquire_lock("synthetic queue completion")
        .unwrap()
        .execute_batch("UPDATE assets SET status='downloaded'")
        .unwrap();
    assert!(
        !has_due_retained_work(std::slice::from_ref(&f.pass), &f.config, controls())
            .await
            .unwrap()
    );
    f.db.acquire_lock("independent unfinished path proof").unwrap().execute_batch("INSERT INTO asset_metadata_paths(library,id,version_size,local_path,provider_checksum,metadata_write_failed_at) SELECT library,id,version_size,'synthetic-path',checksum,1700000000 FROM assets").unwrap();
    assert!(
        !has_due_retained_work(std::slice::from_ref(&f.pass), &f.config, controls())
            .await
            .unwrap(),
        "metadata-only retry remains with the option-aware metadata precheck"
    );
    f.db.acquire_lock("different current queue generation")
        .unwrap()
        .execute_batch("UPDATE assets SET checksum='later-generation',status='pending'")
        .unwrap();
    assert!(
        !has_due_retained_work(std::slice::from_ref(&f.pass), &f.config, controls())
            .await
            .unwrap()
    );
    let other_owner = AccountOwner::authenticated(
        "queue-stage@example.invalid",
        "com",
        &serde_json::from_value(json!({"dsInfo":{"dsid":"wrong-provider"}})).unwrap(),
    )
    .unwrap();
    assert!(
        f.db.has_due_provider_work(other_owner, scope, hash)
            .await
            .is_err()
    );
    f.config.filename_exclude = Arc::from([glob::Pattern::new("changed.jpg").unwrap()]);
    assert!(
        has_due_retained_work(std::slice::from_ref(&f.pass), &f.config, controls())
            .await
            .unwrap(),
        "new config has its own unconsumed source"
    );
}

#[tokio::test]
async fn queue_projection_deferred_retry_and_scan_roll_back_with_receipt_fault() {
    let f = Fixture::new().await;
    f.capture(records("m", OLD, "source"), "old").await;
    *f.session.corrupt.lock().unwrap() = Some("missing");
    f.db.acquire_lock("deferred receipt fault").unwrap().execute_batch("CREATE TRIGGER retry_receipt_fault BEFORE INSERT ON provider_work_receipts BEGIN SELECT RAISE(ABORT,'synthetic deferred receipt fault'); END;").unwrap();
    assert!(
        f.cycle()
            .await
            .unwrap_err()
            .to_string()
            .contains("synthetic deferred receipt fault")
    );
    for table in [
        "provider_work_retries",
        "provider_work_scan",
        "provider_work_receipts",
        "assets",
    ] {
        assert_eq!(f.count(table), 0);
    }
    f.preserved().await;
    f.db.acquire_lock("clear fault")
        .unwrap()
        .execute_batch("DROP TRIGGER retry_receipt_fault")
        .unwrap();
    f.cycle().await.unwrap();
    f.db.acquire_lock("retry due and failed receipt").unwrap().execute_batch("UPDATE provider_work_retries SET last_attempt_at=0,next_retry_at=1; CREATE TRIGGER retry_receipt_fault BEFORE INSERT ON provider_work_receipts BEGIN SELECT RAISE(ABORT,'synthetic deferred receipt fault'); END;").unwrap();
    assert!(f.cycle().await.is_err());
    let retry: (u32, i64, i64) =
        f.db.acquire_lock("rollback previous schedule")
            .unwrap()
            .query_row(
                "SELECT attempts,last_attempt_at,next_retry_at FROM provider_work_retries",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
    assert_eq!(retry, (1, 0, 1));
    assert_eq!(f.count("provider_work_receipts"), 1);
    assert_eq!(f.count("provider_work_obligations"), 0);
    f.preserved().await;
}

#[tokio::test]
async fn queue_projection_schema31_retry_migration_preserves_receipts_and_conflicting_history() {
    for conflict in [false, true] {
        let f = Fixture::new().await;
        f.capture(records("m", OLD, "source"), "old").await;
        *f.session.corrupt.lock().unwrap() = Some("missing");
        f.cycle().await.unwrap();
        f.db.acquire_lock("schema31 retained receipt")
            .unwrap()
            .execute_batch("DROP TABLE provider_work_retries; PRAGMA user_version=31;")
            .unwrap();
        if conflict {
            f.db.acquire_lock("unknown retry schema").unwrap().execute_batch("CREATE TABLE provider_work_retries(page_id INTEGER,ordinal INTEGER,config_hash TEXT,attempts INTEGER,last_attempt_at INTEGER,next_retry_at INTEGER,future_blob BLOB); INSERT INTO provider_work_retries(future_blob) VALUES(X'01FE');").unwrap();
        }
        for _ in 0..2 {
            let opened = SqliteStateDb::open_owned(&f.dir.path().join("state.db"), &owner()).await;
            if conflict {
                assert!(opened.is_err());
                let conn = rusqlite::Connection::open(f.dir.path().join("state.db")).unwrap();
                assert_eq!(
                    conn.pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))
                        .unwrap(),
                    31
                );
                assert_eq!(
                    conn.query_row::<Vec<u8>, _, _>(
                        "SELECT future_blob FROM provider_work_retries",
                        [],
                        |r| r.get(0)
                    )
                    .unwrap(),
                    [1, 254]
                );
            } else {
                let opened = opened.unwrap();
                let (_, scope, zone) = f.pass.album.catalog_work_scope().unwrap().unwrap();
                assert!(
                    opened
                        .has_due_provider_work(
                            owner(),
                            scope,
                            config_hash(&f.config.with_pass(&f.pass), &zone)
                        )
                        .await
                        .unwrap(),
                    "legacy deferred receipt is not silently acknowledged"
                );
            }
            assert_eq!(f.count("provider_work_receipts"), 1);
            assert_eq!(f.count("assets"), 0);
            f.preserved().await;
        }
    }
}

#[tokio::test]
async fn queue_projection_admitted_before_dispatch_reopens_materializes_and_stays_quiet() {
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    let media = b"\xff\xd8\xff\xe0\0\x10JFIF\0\x01\x01\0\0\x01\0\x01\0\0\xff\xd9";
    Mock::given(method("GET"))
        .and(path("/fresh"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(media))
        .expect(1)
        .mount(&server)
        .await;
    let mut f = Fixture::new().await;
    {
        let mut current = f.session.records.lock().unwrap();
        current[0]["fields"]["resOriginalRes"]["value"] = json!({"downloadURL":format!("{}/fresh",server.uri()),"size":media.len(),"fileChecksum":data_encoding::BASE64.encode(&Sha256::digest(media))});
    }
    f.capture(records("m", OLD, "source"), "old").await;
    f.cycle().await.unwrap();
    assert_eq!(f.count("provider_work_receipts"), 1);
    assert_eq!(
        f.db.acquire_lock("pre-dispatch interruption oracle")
            .unwrap()
            .query_row::<String, _, _>("SELECT status FROM assets", [], |r| r.get(0))
            .unwrap(),
        "pending"
    );
    assert_eq!(std::fs::read_dir(&f.config.directory).unwrap().count(), 2);
    f.config.sync_mode = crate::download::SyncMode::Incremental {
        zone_sync_token: "old-cursor".into(),
    };
    f.pass.album = PhotoAlbum::new(
        PhotoAlbumConfig {
            params: Arc::new(std::collections::HashMap::new()),
            service_endpoint: Arc::from("https://example.invalid"),
            name: Arc::from(""),
            list_type: Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
            obj_type: Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
            query_filter: None,
            page_size: 100,
            zone_id: Arc::new(
                json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner","zoneType":"REGULAR_CUSTOM_ZONE"}),
            ),
            retry_config: crate::retry::RetryConfig::default(),
            container_id: None,
            cross_zone_sources: Vec::new(),
        },
        Box::new(DestinationSelectionSession(f.session.clone())),
    );
    let client = reqwest::Client::new();
    let mut quiet_calls = None;
    for reopen in 0..3 {
        let db = Arc::new(
            SqliteStateDb::open_owned(&f.dir.path().join("state.db"), &owner())
                .await
                .unwrap(),
        );
        f.capture = ShadowCapture::new(db.clone(), owner(), "com");
        f.pass
            .album
            .set_shadow_capture(f.capture.clone(), Arc::from("private"));
        f.config.state_db = Some(db.clone() as Arc<dyn DownloadStore>);
        f.db = db;
        assert_eq!(
            has_due_retained_work(std::slice::from_ref(&f.pass), &f.config, controls())
                .await
                .unwrap(),
            reopen == 0
        );
        let result = crate::download::download_photos_with_sync(
            &client,
            std::slice::from_ref(&f.pass),
            Arc::new(f.config.clone()),
            controls(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(matches!(
            result.outcome,
            crate::download::DownloadOutcome::Success
        ));
        assert_eq!(result.stats.downloaded, if reopen == 0 { 1 } else { 0 });
        let (saved, checksum): (String, String) =
            f.db.acquire_lock("download publication oracle")
                .unwrap()
                .query_row(
                    "SELECT local_path,local_checksum FROM assets WHERE status='downloaded'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
        assert_eq!(std::fs::read(saved).unwrap(), media);
        assert_eq!(checksum, format!("{:x}", Sha256::digest(media)));
        assert!(
            !has_due_retained_work(std::slice::from_ref(&f.pass), &f.config, controls())
                .await
                .unwrap()
        );
        let calls = f.session.calls.load(Ordering::SeqCst);
        if let Some(quiet_calls) = quiet_calls {
            assert_eq!(calls, quiet_calls);
        } else {
            quiet_calls = Some(calls);
        }
        f.preserved().await;
    }
}

async fn selection_manifest(
    fixture: &Fixture,
) -> (
    String,
    crate::state::db::provider_selection::SelectionManifest,
) {
    let id: String = fixture
        .db
        .acquire_lock("shadow generation identity")
        .unwrap()
        .query_row(
            "SELECT id FROM provider_selection_generations ORDER BY id LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let manifest = fixture
        .db
        .replay_selection_shadow(owner(), id.clone())
        .await
        .unwrap();
    (id, manifest)
}

#[tokio::test]
async fn selection_shadow_current_authority_original_bytes_and_two_reopens_preserve_progress() {
    let fixture = Fixture::new().await;
    fixture
        .capture(records("m", OLD, "historical"), "old-page")
        .await;
    fixture.cycle().await.unwrap();
    let (id, manifest) = selection_manifest(&fixture).await;
    assert_eq!(manifest.profile["coverage"], "confirmed_sources_only");
    assert_eq!(manifest.decisions.len(), 1);
    let decision = &manifest.decisions[0];
    assert_eq!(decision.outcome, SelectionOutcome::Selected);
    assert_eq!(decision.child, "asset-m");
    assert_eq!(decision.master.as_deref(), Some("m"));
    let body = decision.confirmation.as_ref().unwrap();
    assert!(
        std::str::from_utf8(body)
            .unwrap()
            .contains("\"futureExact\":1.2300e+30")
    );
    assert_eq!(decision.destinations.len(), 1);
    assert_eq!(decision.destinations[0].checksum, CURRENT);
    assert!(
        decision.destinations[0]
            .path
            .to_path()
            .starts_with(&fixture.config.directory)
    );
    assert_eq!(fixture.count("provider_selection_sources"), 1);
    assert_eq!(fixture.count("provider_selection_destinations"), 1);
    let pending: String = fixture
        .db
        .acquire_lock("selection is not byte completion")
        .unwrap()
        .query_row("SELECT status FROM assets WHERE id='asset-m'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(pending, "pending");
    fixture.preserved().await;
    let calls = fixture.session.calls.load(Ordering::SeqCst);
    fixture.cycle().await.unwrap();
    assert_eq!(fixture.session.calls.load(Ordering::SeqCst), calls);
    assert_eq!(fixture.count("provider_selection_generations"), 1);
    let database = fixture.dir.path().join("state.db");
    let media = fixture.config.directory.to_path_buf();
    let Fixture {
        dir,
        db,
        capture,
        session,
        pass,
        config,
    } = fixture;
    drop(config);
    drop(pass);
    drop(capture);
    drop(db);
    drop(session);
    for _ in 0..2 {
        let reopened = SqliteStateDb::open_owned(&database, &owner())
            .await
            .unwrap();
        assert_eq!(
            reopened
                .replay_selection_shadow(owner(), id.clone())
                .await
                .unwrap(),
            manifest
        );
        assert_eq!(
            reopened
                .get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("old-cursor")
        );
        assert_eq!(
            std::fs::read(media.join("existing.jpg")).unwrap(),
            b"existing-media"
        );
        drop(reopened);
    }
    drop(dir);
}

#[tokio::test]
async fn selection_shadow_multiple_destinations_capacity_and_atomic_failure_do_not_admit_work() {
    let fixture = Fixture::new().await;
    fixture
        .capture(records("m", OLD, "historical"), "source")
        .await;
    fixture.cycle().await.unwrap();
    let (id, mut manifest) = selection_manifest(&fixture).await;
    let original_queue = fixture.queue();
    assert_eq!(
        fixture
            .db
            .capture_selection_shadow(owner(), manifest.clone(), 0)
            .await
            .unwrap(),
        id
    );
    let mut second = manifest.decisions[0].clone();
    manifest.decisions[0].pass_key = "album-A".to_owned();
    second.pass_key = "album-B".to_owned();
    second.destinations[0].path =
        SelectionPath::from_path(&fixture.config.directory.join("B/photo.jpg"));
    manifest.decisions.push(second);
    assert!(matches!(
        fixture
            .db
            .capture_selection_shadow(owner(), manifest.clone(), 0)
            .await,
        Err(crate::state::error::StateError::ProviderSelectionFull)
    ));
    assert_eq!(fixture.count("provider_selection_generations"), 1);
    fixture.db.acquire_lock("fault second destination").unwrap().execute_batch(
        "CREATE TRIGGER selection_fault BEFORE INSERT ON provider_selection_destinations WHEN NEW.pass_key='album-B' BEGIN SELECT RAISE(ABORT,'synthetic destination failure'); END;"
    ).unwrap();
    assert!(
        fixture
            .db
            .capture_selection_shadow(owner(), manifest.clone(), MAX_SELECTION_BYTES)
            .await
            .is_err()
    );
    for (table, count) in [
        ("provider_selection_generations", 1),
        ("provider_selection_sources", 1),
        ("provider_selection_decisions", 1),
        ("provider_selection_destinations", 1),
    ] {
        assert_eq!(
            fixture.count(table),
            count,
            "partial shadow insert into {table}"
        );
    }
    fixture
        .db
        .acquire_lock("restore destination writes")
        .unwrap()
        .execute_batch("DROP TRIGGER selection_fault;")
        .unwrap();
    let generation = fixture
        .db
        .capture_selection_shadow(owner(), manifest.clone(), MAX_SELECTION_BYTES)
        .await
        .unwrap();
    assert_eq!(
        fixture
            .db
            .replay_selection_shadow(owner(), generation)
            .await
            .unwrap(),
        manifest
    );
    assert_eq!(fixture.count("provider_selection_destinations"), 3);
    assert_eq!(fixture.queue(), original_queue);
    assert_eq!(fixture.count("provider_work_receipts"), 1);
    fixture.preserved().await;
}

#[tokio::test]
async fn selection_shadow_replay_rejects_corrupt_rows_sources_confirmations_and_foreign_owner() {
    let fixture = Fixture::new().await;
    fixture
        .capture(records("m", OLD, "historical"), "source")
        .await;
    fixture.cycle().await.unwrap();
    let (id, manifest) = selection_manifest(&fixture).await;
    let original_queue = fixture.queue();
    let wrong = AccountOwner::authenticated(
        "other@example.invalid",
        "com",
        &serde_json::from_value(json!({"dsInfo":{"dsid":"queue-provider"}})).unwrap(),
    )
    .unwrap();
    assert!(matches!(
        fixture.db.replay_selection_shadow(wrong, id.clone()).await,
        Err(crate::state::error::StateError::AccountOwnerMismatch)
    ));
    for mutation in [
        "UPDATE provider_selection_destinations SET checksum='wrong'",
        "DELETE FROM provider_selection_destinations",
        "UPDATE provider_selection_sources SET body_hash='wrong'",
        "UPDATE provider_selection_generations SET config_hash='wrong'",
        "UPDATE provider_shadow_pages SET body=X'7B7D'",
    ] {
        let probe = Fixture::new().await;
        probe
            .capture(records("m", OLD, "historical"), "corruption-source")
            .await;
        probe.cycle().await.unwrap();
        let (probe_id, healthy) = selection_manifest(&probe).await;
        assert_eq!(
            probe
                .db
                .replay_selection_shadow(owner(), probe_id.clone())
                .await
                .unwrap(),
            healthy
        );
        let queue = probe.queue();
        probe
            .db
            .acquire_lock("committed shadow corruption probe")
            .unwrap()
            .execute_batch(mutation)
            .unwrap();
        let error = probe
            .db
            .replay_selection_shadow(owner(), probe_id)
            .await
            .expect_err("corrupted evidence cannot replay");
        if mutation.contains("provider_shadow_pages") {
            assert!(
                matches!(
                    error,
                    crate::state::error::StateError::ProviderCatalogInvalid
                ),
                "wrong validation error: {error:?}"
            );
        } else {
            assert!(
                matches!(
                    error,
                    crate::state::error::StateError::ProviderSelectionInvalid
                ),
                "wrong validation error: {error:?}"
            );
        }
        assert_eq!(probe.queue(), queue);
        probe.preserved().await;
    }
    assert_eq!(
        fixture
            .db
            .replay_selection_shadow(owner(), id)
            .await
            .unwrap(),
        manifest
    );
    let mut wrong = manifest.clone();
    wrong.decisions[0].confirmation = Some(b"{\"records\":[]}".to_vec());
    assert!(matches!(
        fixture
            .db
            .capture_selection_shadow(owner(), wrong, MAX_SELECTION_BYTES)
            .await,
        Err(crate::state::error::StateError::ProviderSelectionInvalid)
    ));
    let mut wrong = manifest;
    wrong.scope = wrong.scope.replace("PrimarySync", "OtherZone");
    assert!(matches!(
        fixture
            .db
            .capture_selection_shadow(owner(), wrong, MAX_SELECTION_BYTES)
            .await,
        Err(crate::state::error::StateError::ProviderSelectionInvalid)
    ));
    assert_eq!(fixture.queue(), original_queue);
    fixture.preserved().await;
}

#[tokio::test]
async fn selection_shadow_failure_precedes_queue_admission_then_recovers_quietly() {
    let fixture = Fixture::new().await;
    fixture
        .capture(records("m", OLD, "historical"), "source")
        .await;
    fixture.db.acquire_lock("shadow manifest fault").unwrap().execute_batch(
        "CREATE TRIGGER selection_manifest_fault BEFORE INSERT ON provider_selection_generations BEGIN SELECT RAISE(ABORT,'synthetic selection failure'); END;"
    ).unwrap();
    assert!(fixture.cycle().await.is_err());
    assert_eq!(fixture.count("provider_selection_generations"), 0);
    assert_eq!(fixture.count("assets"), 0);
    assert_eq!(fixture.count("provider_work_receipts"), 0);
    assert_eq!(fixture.count("provider_catalog_records"), 2);
    fixture.preserved().await;
    fixture
        .db
        .acquire_lock("remove shadow fault")
        .unwrap()
        .execute_batch("DROP TRIGGER selection_manifest_fault;")
        .unwrap();
    fixture.cycle().await.unwrap();
    assert_eq!(fixture.count("assets"), 1);
    assert_eq!(fixture.count("provider_work_receipts"), 1);
    let calls = fixture.session.calls.load(Ordering::SeqCst);
    fixture.cycle().await.unwrap();
    fixture.cycle().await.unwrap();
    assert_eq!(fixture.session.calls.load(Ordering::SeqCst), calls);
    assert_eq!(fixture.count("provider_selection_generations"), 1);
    fixture.preserved().await;
}

#[tokio::test]
async fn selection_shadow_schema32_migration_preserves_live_wal_work_and_unknown_collision() {
    let fixture = Fixture::new().await;
    fixture
        .capture(records("m", OLD, "historical"), "source")
        .await;
    fixture.cycle().await.unwrap();
    let queue = fixture.queue();
    let work: Vec<u8> = fixture
        .db
        .acquire_lock("retained work bytes")
        .unwrap()
        .query_row(
            "SELECT confirmation FROM provider_work_receipts LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    fixture.db.acquire_lock("synthetic schema32 and unknown table").unwrap().execute_batch(
        "PRAGMA wal_autocheckpoint=0; DROP TABLE provider_selection_destinations; DROP TABLE provider_selection_decisions; DROP TABLE provider_selection_sources; DROP TABLE provider_selection_generations; PRAGMA user_version=32; CREATE TABLE provider_selection_generations(payload BLOB); INSERT INTO provider_selection_generations VALUES(X'00FF09');"
    ).unwrap();
    let database = fixture.dir.path().join("state.db");
    assert!(
        SqliteStateDb::open_owned(&database, &owner())
            .await
            .is_err()
    );
    {
        let conn = fixture
            .db
            .acquire_lock("migration rollback oracle")
            .unwrap();
        assert_eq!(
            conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            32
        );
        assert_eq!(
            conn.query_row(
                "SELECT payload FROM provider_selection_generations",
                [],
                |r| r.get::<_, Vec<u8>>(0)
            )
            .unwrap(),
            [0, 255, 9]
        );
        assert_eq!(conn.query_row("SELECT count(*) FROM sqlite_schema WHERE type='table' AND name IN ('provider_selection_sources','provider_selection_decisions','provider_selection_destinations')", [], |r|r.get::<_,i64>(0)).unwrap(), 0);
        conn.execute_batch(
            "ALTER TABLE provider_selection_generations RENAME TO retained_future_selection;",
        )
        .unwrap();
    }
    let migrated = SqliteStateDb::open_owned(&database, &owner())
        .await
        .unwrap();
    assert_eq!(fixture.queue(), queue);
    assert_eq!(
        fixture
            .db
            .acquire_lock("work preserved after migration")
            .unwrap()
            .query_row(
                "SELECT confirmation FROM provider_work_receipts LIMIT 1",
                [],
                |r| r.get::<_, Vec<u8>>(0)
            )
            .unwrap(),
        work
    );
    assert_eq!(
        migrated
            .acquire_lock("future table preserved")
            .unwrap()
            .query_row("SELECT payload FROM retained_future_selection", [], |r| r
                .get::<_, Vec<
                u8,
            >>(
                0
            ))
            .unwrap(),
        [0, 255, 9]
    );
    assert_eq!(fixture.count("provider_selection_generations"), 0);
    fixture.preserved().await;
    drop(migrated);
    fixture.cycle().await.unwrap(); // Previously admitted work is not recreated.
    assert_eq!(fixture.count("provider_selection_generations"), 0);
    assert_eq!(fixture.queue(), queue);
}

#[tokio::test]
async fn selection_shadow_relative_download_root_keeps_existing_admission_contract() {
    let mut fixture = Fixture::new().await;
    let current = std::env::current_dir().unwrap();
    let relative = tempfile::tempdir_in(&current).unwrap();
    let root = relative.path().join("media");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("existing.jpg"), b"existing-media").unwrap();
    std::fs::write(root.join("existing.xmp"), b"retained-sidecar").unwrap();
    let absolute = std::path::absolute(&root).unwrap();
    fixture.config.directory = Arc::from(absolute.strip_prefix(current).unwrap());
    assert!(!fixture.config.directory.is_absolute());
    fixture
        .capture(records("m", OLD, "historical"), "relative-source")
        .await;
    fixture.cycle().await.unwrap();
    let (_, manifest) = selection_manifest(&fixture).await;
    assert!(
        manifest.decisions[0].destinations[0]
            .path
            .to_path()
            .starts_with(&absolute)
    );
    fixture.preserved().await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn selection_shadow_non_utf8_download_root_preserves_native_path() {
    use std::os::unix::ffi::OsStringExt;
    let mut fixture = Fixture::new().await;
    let root = fixture
        .dir
        .path()
        .join(std::ffi::OsString::from_vec(b"native-root-\xff".to_vec()));
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("existing.jpg"), b"existing-media").unwrap();
    std::fs::write(root.join("existing.xmp"), b"retained-sidecar").unwrap();
    fixture.config.directory = Arc::from(root.as_path());
    assert!(fixture.config.directory.to_str().is_none());
    fixture
        .capture(records("m", OLD, "historical"), "native-source")
        .await;
    fixture.cycle().await.unwrap();
    let (generation, manifest) = selection_manifest(&fixture).await;
    let path = manifest.decisions[0].destinations[0].path.to_path();
    assert!(path.starts_with(&root));
    assert!(path.to_str().is_none());
    let reopened = SqliteStateDb::open_owned(&fixture.dir.path().join("state.db"), &owner())
        .await
        .unwrap();
    assert_eq!(
        reopened
            .replay_selection_shadow(owner(), generation)
            .await
            .unwrap(),
        manifest
    );
    assert_eq!(fixture.count("assets"), 1);
    fixture.preserved().await;
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn selection_shadow_native_path_storage_reopens_without_filesystem_creation() {
    let fixture = Fixture::new().await;
    fixture
        .capture(records("m", OLD, "historical"), "native-storage-source")
        .await;
    fixture.cycle().await.unwrap();
    let (_, mut manifest) = selection_manifest(&fixture).await;
    let component = {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            std::ffi::OsString::from_vec(b"native-root-\xff".to_vec())
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStringExt;
            std::ffi::OsString::from_wide(&[0x006e, 0x0061, 0x0074, 0xd800])
        }
    };
    // Native encoding is a storage contract even when the current filesystem
    // cannot create this name. This fixture deliberately never creates it.
    let destination = fixture.dir.path().join(component).join("selected.jpg");
    assert!(destination.is_absolute());
    assert!(destination.to_str().is_none());
    manifest.decisions[0].destinations[0].path = SelectionPath::from_path(&destination);
    let queue = fixture.queue();
    let generation = fixture
        .db
        .capture_selection_shadow(owner(), manifest.clone(), MAX_SELECTION_BYTES)
        .await
        .unwrap();
    assert_eq!(fixture.queue(), queue);
    fixture.preserved().await;
    let database = fixture.dir.path().join("state.db");
    let Fixture {
        dir,
        db,
        capture,
        session,
        pass,
        config,
    } = fixture;
    drop(config);
    drop(pass);
    drop(capture);
    drop(db);
    drop(session);
    for _ in 0..2 {
        let reopened = SqliteStateDb::open_owned(&database, &owner())
            .await
            .unwrap();
        let replay = reopened
            .replay_selection_shadow(owner(), generation.clone())
            .await
            .unwrap();
        assert_eq!(replay, manifest);
        assert_eq!(
            replay.decisions[0].destinations[0].path.to_path(),
            destination
        );
        assert_eq!(
            reopened
                .get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("old-cursor")
        );
        drop(reopened);
    }
    drop(dir);
}

#[derive(Clone)]
struct DestinationSelectionSession(CurrentSession);

#[async_trait::async_trait]
impl PhotosSession for DestinationSelectionSession {
    async fn post(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        let request: Value = serde_json::from_str(&body)?;
        if url.contains("/internal/records/query/batch?") {
            let batch = request["batch"]
                .as_array()
                .unwrap()
                .iter()
                .map(|_| json!({"records":[{"fields":{"itemCount":{"value":1}}}]}))
                .collect::<Vec<_>>();
            return Ok(json!({"batch":batch}));
        }
        if url.contains("/records/query?") {
            self.0.rank_calls.fetch_add(1, Ordering::SeqCst);
            let offset = request["query"]["filterBy"]
                .as_array()
                .unwrap()
                .iter()
                .find(|f| f["fieldName"] == "startRank")
                .unwrap()["fieldValue"]["value"]
                .as_u64()
                .unwrap();
            let records = if offset == 0 {
                self.0.records.lock().unwrap().clone()
            } else {
                Vec::new()
            };
            return Ok(json!({"records":records,"syncToken":"destination-rank-proof"}));
        }
        self.0.post(url, body, headers).await
    }
    async fn post_changes_body(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<Vec<u8>> {
        let value = self.post(url, body, headers).await?;
        let mut bytes = serde_json::to_vec(&value)?;
        bytes.pop();
        bytes.extend_from_slice(b",\"rankFutureExact\":1.2300e+30}");
        Ok(bytes)
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

fn destination_pass(f: &Fixture, name: &str) -> AlbumPass {
    destination_pass_with(&f.session, &f.capture, name)
}

fn destination_pass_with(
    session: &CurrentSession,
    capture: &ShadowCapture,
    name: &str,
) -> AlbumPass {
    let mut album = PhotoAlbum::new(
        PhotoAlbumConfig {
            params: Arc::new(std::collections::HashMap::new()),
            service_endpoint: Arc::from("https://example.invalid"),
            name: Arc::from(name),
            list_type: Arc::from("CPLContainerRelationLiveByAssetDate"),
            obj_type: Arc::from(format!("CPLContainerRelationNotDeletedByAssetDate:{name}")),
            query_filter: Some(Arc::new(
                json!([{"fieldName":"parentId","comparator":"EQUALS","fieldValue":{"type":"STRING","value":name}}]),
            )),
            page_size: 2,
            zone_id: Arc::new(
                json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner","zoneType":"REGULAR_CUSTOM_ZONE"}),
            ),
            retry_config: crate::retry::RetryConfig::default(),
            container_id: Some(Arc::from(name)),
            cross_zone_sources: Vec::new(),
        },
        Box::new(DestinationSelectionSession(session.clone())),
    );
    album.set_shadow_capture(capture.clone(), Arc::from("private"));
    AlbumPass {
        kind: PassKind::Album,
        album,
        exclude_ids: Arc::new(FxHashSet::default()),
    }
}

#[tokio::test]
async fn private_destination_reopen_materializes_missing_copy() {
    Box::pin(destination_reopen_fixture(false, false, false)).await;
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn private_destination_reopen_finishes_exact_sidecar_receipts() {
    Box::pin(destination_reopen_fixture(false, true, false)).await;
}

#[cfg(all(target_os = "linux", feature = "xmp"))]
#[tokio::test]
async fn private_destination_reopen_finishes_native_sidecar_receipts() {
    Box::pin(destination_reopen_fixture(true, true, false)).await;
}

async fn destination_reopen_fixture(native: bool, metadata: bool, backlog: bool) {
    let (trace, _trace_guard) = crate::test_helpers::TracingCapture::install();
    use crate::download::pipeline::{StreamRuntime, stream_and_download_from_stream};
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    let media = b"\xff\xd8\xff\xe0\0\x10JFIF\0\x01\x01\0\0\x01\0\x01\0\0\xff\xd9";
    Mock::given(method("GET"))
        .and(path("/destination"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(media))
        .expect(2)
        .mount(&server)
        .await;
    let mut f = Fixture::new().await;
    f.config.folder_structure_albums = Arc::from("{album}");
    #[cfg(unix)]
    if native {
        use std::os::unix::ffi::OsStringExt;
        let root = f
            .dir
            .path()
            .join(std::ffi::OsString::from_vec(b"native-\xff".to_vec()));
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("existing.jpg"), b"existing-media").unwrap();
        std::fs::write(root.join("existing.xmp"), b"retained-sidecar").unwrap();
        f.config.directory = Arc::from(root.as_path());
    }
    #[cfg(not(unix))]
    assert!(!native);
    #[cfg(feature = "xmp")]
    {
        f.config.metadata.xmp_sidecar = metadata;
    }
    #[cfg(not(feature = "xmp"))]
    assert!(!metadata);
    {
        let mut current = f.session.records.lock().unwrap();
        current[0]["fields"]["resOriginalRes"]["value"] = json!({"downloadURL":format!("{}/destination",server.uri()),"size":media.len(),"fileChecksum":data_encoding::BASE64.encode(&Sha256::digest(media))});
    }
    f.capture(records("m", OLD, "historical"), "old").await;
    let passes = vec![destination_pass(&f, "A"), destination_pass(&f, "B")];
    let asset = passes[0]
        .album
        .confirm_catalog_asset("asset-m")
        .await
        .unwrap()
        .asset;
    if native {
        let initial = crate::download::download_photos_with_sync(
            &reqwest::Client::new(),
            &passes[..1],
            Arc::new(f.config.clone()),
            controls(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(initial.stats.downloaded, 1);
        if metadata {
            let before = f
                .db
                .acquire_lock("initial native metadata proof")
                .unwrap()
                .query_row::<i64, _, _>(
                    "SELECT count(*) FROM provider_active_destinations WHERE verified_metadata=1",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            let offset =
                f.db.get_metadata("active_selection_metadata_retry_offset")
                    .await
                    .unwrap();
            assert_eq!(
                before, 1,
                "initial native metadata must finish before later grouping; offset={offset:?}; result={initial:?}"
            );
        }
    } else {
        let initial = stream_and_download_from_stream(
            &reqwest::Client::new(),
            futures_util::stream::iter([Ok(asset)]),
            &Arc::new(f.config.with_pass(&passes[0])),
            controls(),
            1,
            CancellationToken::new(),
            StreamRuntime::new(None, None),
        )
        .await
        .unwrap();
        assert_eq!(initial.downloaded, 1);
    }
    for pass in &passes {
        crate::download::orchestration::test_support::seed_complete_album_snapshot(
            f.db.as_ref(),
            pass.album.container_id().unwrap(),
            pass.album.name.as_ref(),
            &[("asset-m", "m")],
        )
        .await;
    }
    let a = f.config.directory.join("A/changed.JPG");
    assert_eq!(std::fs::read(&a).unwrap(), media);
    let original_time = std::fs::metadata(&a).unwrap().modified().unwrap();
    let backlog_tail = f.config.directory.join("backlog-tail.JPG");
    if backlog {
        std::fs::write(&backlog_tail, media).unwrap();
        let conn =
            f.db.acquire_lock("synthetic populated failed metadata backlog")
                .unwrap();
        let columns = conn
            .prepare("SELECT name FROM pragma_table_info('assets') ORDER BY cid")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let quoted = columns
            .iter()
            .map(|name| format!("\"{name}\""))
            .collect::<Vec<_>>();
        let selected = columns
            .iter()
            .map(|name| match name.as_str() {
                "id" => "?1".into(),
                "library" => "'MetadataBacklog'".into(),
                "local_path" => "?2".into(),
                "metadata_write_failed_at" => "1".into(),
                _ => format!("\"{name}\""),
            })
            .collect::<Vec<String>>();
        let sql = format!(
            "INSERT INTO assets ({}) SELECT {} FROM assets WHERE library='PrimarySync' AND id='asset-m' AND version_size='original'",
            quoted.join(","),
            selected.join(",")
        );
        for index in 0..502 {
            let path = if index == 501 {
                backlog_tail.clone()
            } else {
                f.config
                    .directory
                    .join(format!("missing-backlog-{index:04}.JPG"))
            };
            assert_eq!(
                conn.execute(
                    &sql,
                    rusqlite::params![format!("backlog-{index:04}"), path.to_str().unwrap()]
                )
                .unwrap(),
                1
            );
        }
    }
    f.config.sync_mode = crate::download::SyncMode::Incremental {
        zone_sync_token: "old-cursor".into(),
    };
    drop(passes);
    f.config.state_db = None;
    drop(f.capture);
    drop(f.pass);
    drop(f.db);
    let reopened = Arc::new(
        SqliteStateDb::open_owned(&f.dir.path().join("state.db"), &owner())
            .await
            .unwrap(),
    );
    f.db = reopened;
    f.capture = ShadowCapture::new(f.db.clone(), owner(), "com");
    f.config.state_db = Some(f.db.clone() as Arc<dyn DownloadStore>);
    f.pass = destination_pass_with(&f.session, &f.capture, "A");
    let passes = vec![destination_pass(&f, "A"), destination_pass(&f, "B")];
    let result = crate::download::download_photos_with_sync(
        &reqwest::Client::new(),
        &passes,
        Arc::new(f.config.clone()),
        controls(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        result.stats.downloaded, 1,
        "canonical A completion must not suppress selected B after quiet reopen"
    );
    let b = f.config.directory.join("B").join(a.file_name().unwrap());
    assert_eq!(std::fs::read(&b).unwrap(), media);
    assert_eq!(
        std::fs::metadata(&a).unwrap().modified().unwrap(),
        original_time
    );
    assert_eq!(std::fs::read(&a).unwrap(), media);
    let requests = f.session.calls.load(Ordering::SeqCst);
    let roots =
        f.db.acquire_lock("sealed active roots")
            .unwrap()
            .query_row::<i64, _, _>(
                "SELECT count(*) FROM provider_active_generations WHERE sealed=1",
                [],
                |r| r.get(0),
            )
            .unwrap();
    assert_eq!(roots, if native { 2 } else { 1 });
    let destinations=f.db.acquire_lock("independent verified paths").unwrap().query_row::<i64,_,_>("SELECT count(*) FROM provider_active_destinations WHERE admitted=1 AND verified_media=1",[],|r|r.get(0)).unwrap();
    let proof_rows = f.db.acquire_lock("destination proof diagnostic").unwrap().prepare("SELECT pass_key,path,verified_media,source_checksum,local_checksum FROM provider_active_destinations").unwrap().query_map([],|r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,i64>(2)?,r.get::<_,Option<String>>(3)?,r.get::<_,Option<String>>(4)?))).unwrap().collect::<Result<Vec<_>,_>>().unwrap();
    let path_rows = f.db.acquire_lock("legacy receipt diagnostic").unwrap().prepare("SELECT local_path,source_checksum,local_checksum FROM asset_metadata_paths WHERE id='asset-m'").unwrap().query_map([],|r| Ok((r.get::<_,String>(0)?,r.get::<_,Option<String>>(1)?,r.get::<_,Option<String>>(2)?))).unwrap().collect::<Result<Vec<_>,_>>().unwrap();
    assert_eq!(
        destinations,
        if native { 3 } else { 2 },
        "active: {proof_rows:?}, legacy: {path_rows:?}"
    );
    if metadata {
        let complete=f.db.acquire_lock("configured metadata proof").unwrap().query_row::<i64,_,_>("SELECT count(*) FROM provider_active_destinations WHERE verified_media=1 AND verified_metadata=1",[],|r|r.get(0)).unwrap();
        let diagnostic_rows = {
            let conn = f.db.acquire_lock("metadata completion diagnostic").unwrap();
            let mut statement=conn.prepare("SELECT generation,pass_key,path,verified_metadata FROM provider_active_destinations").unwrap();
            statement
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, bool>(3)?,
                    ))
                })
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        };
        let offset =
            f.db.get_metadata("active_selection_metadata_retry_offset")
                .await
                .unwrap();
        assert_eq!(
            complete,
            if native { 3 } else { 2 },
            "actual configured rewrite must finish both exact destinations; result: {result:?}; rows: {diagnostic_rows:?}; offset: {offset:?}; events: {:?}",
            trace
                .events()
                .iter()
                .filter(|event| event.level <= tracing::Level::WARN)
                .collect::<Vec<_>>()
        );
        for media_path in [&a, &b] {
            assert!(
                std::fs::read(media_path.with_file_name(format!(
                    "{}.xmp",
                    media_path.file_name().unwrap().to_str().unwrap()
                )))
                .unwrap()
                .windows(1)
                .any(|byte| byte == b"<")
            );
        }
    }
    drop(passes);
    for _ in 0..2 {
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
        let passes = vec![destination_pass(&f, "A"), destination_pass(&f, "B")];
        let quiet = crate::download::download_photos_with_sync(
            &reqwest::Client::new(),
            &passes,
            Arc::new(f.config.clone()),
            controls(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(quiet.stats.downloaded, 0);
        assert_eq!(
            f.session.calls.load(Ordering::SeqCst),
            requests,
            "quiet replay must not repeat healthy current lookup"
        );
        assert_eq!(
            f.db.acquire_lock("no repeated root")
                .unwrap()
                .query_row::<i64, _, _>(
                    "SELECT count(*) FROM provider_active_generations",
                    [],
                    |r| r.get(0)
                )
                .unwrap(),
            roots
        );
        assert_eq!(
            std::fs::metadata(&a).unwrap().modified().unwrap(),
            original_time
        );
        assert_eq!(std::fs::read(&b).unwrap(), media);
        if backlog {
            assert!(
                std::fs::read_to_string(backlog_tail.with_file_name("backlog-tail.JPG.xmp"))
                    .unwrap()
                    .contains("current")
            );
            let conn =
                f.db.acquire_lock("failed prefix survives healthy tail")
                    .unwrap();
            assert_eq!(conn.query_row::<i64,_,_>("SELECT count(*) FROM assets WHERE library='MetadataBacklog' AND metadata_write_failed_at IS NOT NULL",[],|r|r.get(0)).unwrap(),501);
        }
        f.preserved().await;
    }
    f.preserved().await;
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn private_metadata_shared_native_path_completes_compatible_pass_aliases() {
    Box::pin(private_metadata_recovery_fixture(
        true,
        false,
        false,
        false,
        MetadataStateFault::None,
    ))
    .await;
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn private_metadata_interrupted_root_recovers_without_sealing_old_coverage() {
    Box::pin(private_metadata_recovery_fixture(
        false,
        true,
        false,
        false,
        MetadataStateFault::None,
    ))
    .await;
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn private_metadata_completed_history_does_not_hide_explicit_refresh() {
    Box::pin(private_metadata_recovery_fixture(
        false,
        false,
        true,
        false,
        MetadataStateFault::None,
    ))
    .await;
}

#[cfg(feature = "xmp")]
#[derive(Clone, Copy, PartialEq, Eq)]
enum MetadataStateFault {
    None,
    AfterWrite,
    AfterWriteAndSourceDrift,
}

#[cfg(feature = "xmp")]
async fn private_metadata_recovery_fixture(
    shared: bool,
    interrupted: bool,
    refresh: bool,
    native: bool,
    finalization_fault: MetadataStateFault,
) {
    let (trace, _trace_guard) = crate::test_helpers::TracingCapture::install();
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    // XMPFiles requires a UTF-8 filename. The existing HEIF byte writer
    // supports native paths, so this control exercises its actual publication.
    let media: &[u8] = if native {
        include_bytes!("../../../../tests/data/sample.heic")
    } else {
        b"\xff\xd8\xff\xe0\0\x10JFIF\0\x01\x01\0\0\x01\0\x01\0\x01\0\0\xff\xd9"
    };
    let filename = if native {
        "changed.HEIC"
    } else {
        "changed.JPG"
    };
    Mock::given(method("GET"))
        .and(path("/metadata-history"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(media))
        .expect(if shared { 1 } else { 2 })
        .mount(&server)
        .await;
    let mut f = Fixture::new().await;
    #[cfg(unix)]
    if native {
        use std::os::unix::ffi::OsStringExt;
        let root = f.dir.path().join(std::ffi::OsString::from_vec(
            b"native-refresh-\xff".to_vec(),
        ));
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("existing.jpg"), b"existing-media").unwrap();
        std::fs::write(root.join("existing.xmp"), b"retained-sidecar").unwrap();
        f.config.directory = Arc::from(root.as_path());
    }
    #[cfg(not(unix))]
    assert!(!native);
    f.config.folder_structure_albums = Arc::from(if shared { "copies" } else { "{album}" });
    f.config.metadata.embed_xmp = shared;
    f.config.metadata.xmp_sidecar = !shared;
    f.session.records.lock().unwrap()[0]["fields"]["resOriginalRes"]["value"] = json!({"downloadURL":format!("{}/metadata-history",server.uri()),"size":media.len(),"fileChecksum":data_encoding::BASE64.encode(&Sha256::digest(media))});
    f.session.records.lock().unwrap()[0]["fields"]["filenameEnc"] =
        json!({"value": filename, "type": "STRING"});
    if native {
        let mut current = f.session.records.lock().unwrap();
        current[0]["fields"]["resOriginalFileType"] = json!({"value": "public.heic"});
        current[0]["fields"]["itemType"] = json!({"value": "public.heic"});
    }
    f.capture(records("m", OLD, "retained-history"), "history")
        .await;
    let passes = vec![destination_pass(&f, "A"), destination_pass(&f, "B")];
    for pass in &passes {
        crate::download::orchestration::test_support::seed_complete_album_snapshot(
            f.db.as_ref(),
            pass.album.container_id().unwrap(),
            pass.album.name.as_ref(),
            &[("asset-m", "m")],
        )
        .await;
    }
    if native && finalization_fault != MetadataStateFault::None {
        // Establish the original physical media before the explicit metadata
        // generation, avoiding concurrently planned native download aliases in
        // the late-writer fault itself.
        f.config.metadata.embed_xmp = false;
        let primed = crate::download::download_photos_with_sync(
            &reqwest::Client::new(),
            std::slice::from_ref(&passes[0]),
            Arc::new(f.config.clone()),
            controls(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(
            matches!(primed.outcome, crate::download::DownloadOutcome::Success),
            "{primed:?}"
        );
        assert_eq!(primed.stats.downloaded, 1);
        f.config.metadata.embed_xmp = true;
    }
    if finalization_fault != MetadataStateFault::None {
        f.db.acquire_lock("interrupt metadata completion after actual embedded write").unwrap().execute_batch("CREATE TRIGGER interrupt_metadata_finish BEFORE UPDATE OF verified_metadata ON provider_active_destinations WHEN NEW.verified_metadata=1 BEGIN SELECT RAISE(ABORT,'synthetic metadata finalization failure'); END;").unwrap();
    }
    if interrupted {
        f.db.acquire_lock("interrupt coverage seal after actual publication").unwrap().execute_batch("CREATE TRIGGER interrupt_selection_seal BEFORE UPDATE OF sealed ON provider_active_generations WHEN NEW.sealed=1 BEGIN SELECT RAISE(ABORT,'synthetic selection seal interruption'); END;").unwrap();
    }
    let first = crate::download::download_photos_with_sync(
        &reqwest::Client::new(),
        &passes,
        Arc::new(f.config.clone()),
        controls(),
        CancellationToken::new(),
    )
    .await;
    if interrupted {
        assert!(
            first
                .unwrap_err()
                .to_string()
                .contains("synthetic selection seal interruption")
        );
        assert_eq!(f.db.acquire_lock("interrupted independent media").unwrap().query_row::<i64,_,_>("SELECT count(*) FROM provider_active_destinations WHERE verified_media=1 AND verified_metadata=0",[],|r|r.get(0)).unwrap(),2);
        assert_eq!(
            f.db.acquire_lock("old coverage remains unsealed")
                .unwrap()
                .query_row::<i64, _, _>(
                    "SELECT count(*) FROM provider_active_generations WHERE sealed=0",
                    [],
                    |r| r.get(0)
                )
                .unwrap(),
            1
        );
        f.db.acquire_lock("remove synthetic interruption")
            .unwrap()
            .execute_batch("DROP TRIGGER interrupt_selection_seal")
            .unwrap();
        // A new retained unknown source dirties selection without granting any
        // absence authority or replacing the selected resource.
        f.capture(vec![json!({"recordName":"new-unresolved-observation","recordType":"FutureOpaque","future":{"payload":"keep"}})],"changed-basis").await;
    } else if finalization_fault != MetadataStateFault::None {
        let first = first.unwrap();
        assert_eq!(first.stats.downloaded, usize::from(!native));
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
        assert!(
            first.stats.sync_token_blocked_reason.is_some(),
            "unfinished exact receipts must hold publication: {first:?}"
        );
        assert_eq!(f.db.acquire_lock("failed finalization leaves exact debt").unwrap().query_row::<i64,_,_>("SELECT count(*) FROM provider_active_destinations WHERE admitted=1 AND verified_media=1 AND verified_metadata=0",[],|r|r.get(0)).unwrap(),2);
        assert_ne!(
            std::fs::read(f.config.directory.join(format!("copies/{filename}"))).unwrap(),
            media,
            "the embedded writer must actually have landed bytes before the state fault"
        );
        let written_path = f.config.directory.join(format!("copies/{filename}"));
        let own_output = std::fs::read(&written_path).unwrap();
        let (generation,key,child,prepared):(String,String,String,String)=f.db.acquire_lock("prepared output independently persisted").unwrap().query_row("SELECT generation,pass_key,child,prepared_checksum FROM provider_active_destinations WHERE prepared_checksum IS NOT NULL",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap();
        assert_eq!(prepared, format!("{:x}", Sha256::digest(&own_output)));
        f.db.acquire_lock("negative corrupted prepared output proof").unwrap().execute("UPDATE provider_active_destinations SET prepared_checksum=?4 WHERE generation=?1 AND pass_key=?2 AND child=?3",rusqlite::params![generation,key,child,"0".repeat(64)]).unwrap();
        assert!(
            matches!(
                f.db.pending_selection_metadata(owner(), generation.clone(), 250)
                    .await,
                Err(crate::state::error::StateError::ProviderSelectionInvalid)
            ),
            "a normalized output checksum without matching intent/input proof cannot authorize recovery"
        );
        f.db.acquire_lock("restore exact synthetic proof").unwrap().execute("UPDATE provider_active_destinations SET prepared_checksum=?4 WHERE generation=?1 AND pass_key=?2 AND child=?3",rusqlite::params![generation,key,child,prepared]).unwrap();
        std::fs::write(&written_path, b"unrelated synthetic damage").unwrap();
        let negative = crate::download::metadata_rewrite::run_pending_budget(
            f.db.as_ref(),
            crate::download::pipeline::MetadataFlags::from(&f.config),
            crate::download::metadata_rewrite::CaptureTimestampRepair::Preserve,
            f.config.temp_suffix.clone(),
            &CancellationToken::new(),
            None,
            (0, 500),
        )
        .await;
        assert_eq!(
            negative.applied, 0,
            "arbitrary bytes cannot be blessed by an operation-owned prepared receipt"
        );
        assert_eq!(
            std::fs::read(&written_path).unwrap(),
            b"unrelated synthetic damage"
        );
        assert_eq!(f.db.acquire_lock("negative cannot acknowledge aliases").unwrap().query_row::<i64,_,_>("SELECT count(*) FROM provider_active_destinations WHERE admitted=1 AND verified_metadata=0",[],|r|r.get(0)).unwrap(),2);
        std::fs::write(&written_path, &own_output).unwrap();
        f.db.acquire_lock("remove only metadata state fault")
            .unwrap()
            .execute_batch("DROP TRIGGER interrupt_metadata_finish")
            .unwrap();
    } else {
        let first = first.unwrap();
        let proof_rows=f.db.acquire_lock("actual shared writer evidence").unwrap().prepare("SELECT path,local_checksum,source_checksum,verified_metadata FROM provider_active_destinations").unwrap().query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,Option<String>>(1)?,r.get::<_,Option<String>>(2)?,r.get::<_,bool>(3)?))).unwrap().collect::<Result<Vec<_>,_>>().unwrap();
        let actual_bytes = std::fs::read(f.config.directory.join(if shared {
            format!("copies/{filename}")
        } else {
            format!("A/{filename}")
        }))
        .unwrap();
        assert!(
            matches!(first.outcome, crate::download::DownloadOutcome::Success),
            "{first:?}; proofs: {proof_rows:?}; actualSHA: {:x}; warnings: {:?}",
            Sha256::digest(&actual_bytes),
            trace
                .events()
                .iter()
                .filter(|e| e.level <= tracing::Level::WARN)
                .collect::<Vec<_>>()
        );
        assert_eq!(first.stats.downloaded, if shared { 1 } else { 2 });
    }
    // The TEXT projection collides with this distinct real UTF-8 sibling.
    // Its matching old resource bytes cannot authorize writes under the native
    // configuration, nor can the native receipt settle its legacy markers.
    let native_collision = if native
        && finalization_fault == MetadataStateFault::AfterWriteAndSourceDrift
    {
        let mirror = std::path::PathBuf::from(f.config.directory.to_string_lossy().into_owned())
            .join(format!("copies/{filename}"));
        std::fs::create_dir_all(mirror.parent().unwrap()).unwrap();
        std::fs::write(&mirror, media).unwrap();
        let sidecar = mirror.with_file_name(format!("{filename}.xmp"));
        std::fs::write(&sidecar, b"unrelated-sibling-sidecar").unwrap();
        let legacy =
            f.db.acquire_lock("retain ambiguous legacy sibling debt")
                .unwrap()
                .query_row::<(Option<String>, Option<i64>), _, _>(
                    "SELECT local_checksum,metadata_write_failed_at FROM assets WHERE id='asset-m'",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
        assert!(legacy.1.is_some());
        Some((mirror, sidecar, legacy))
    } else {
        None
    };
    let media_paths: Vec<_> = if shared {
        vec![f.config.directory.join(format!("copies/{filename}"))]
    } else {
        vec![
            f.config.directory.join(format!("A/{filename}")),
            f.config.directory.join(format!("B/{filename}")),
        ]
    };
    for media_path in &media_paths {
        assert!(media_path.is_file());
    }
    if shared {
        assert_ne!(
            std::fs::read(&media_paths[0]).unwrap(),
            media,
            "embedded metadata must actually change media bytes"
        );
    }
    if finalization_fault == MetadataStateFault::AfterWriteAndSourceDrift {
        f.capture(vec![json!({"recordName":"independent-source-drift-after-own-output","recordType":"FutureOpaque","unknown":{"keep":"all"}})],"prepared-output-source-drift").await;
    }
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
    let passes = vec![destination_pass(&f, "A"), destination_pass(&f, "B")];
    f.config.sync_mode = crate::download::SyncMode::Incremental {
        zone_sync_token: "old-cursor".into(),
    };
    if refresh {
        f.session.records.lock().unwrap()[1]["fields"]["captionEnc"] =
            json!({"value":"new-explicit-title","type":"STRING"});
        f.config.refresh_metadata = true;
        f.config.sync_mode = crate::download::SyncMode::Full;
    }
    let recovered = crate::download::download_photos_with_sync(
        &reqwest::Client::new(),
        &passes,
        Arc::new(f.config.clone()),
        controls(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(
        matches!(recovered.outcome, crate::download::DownloadOutcome::Success),
        "{recovered:?}; warnings: {:?}",
        trace
            .events()
            .iter()
            .filter(|e| e.level <= tracing::Level::WARN)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        recovered.stats.downloaded, 0,
        "healthy completed physical paths must not redownload"
    );
    {
        let conn =
            f.db.acquire_lock("all exact compatible metadata obligations finished")
                .unwrap();
        assert_eq!(conn.query_row::<i64,_,_>("SELECT count(*) FROM provider_active_destinations WHERE admitted=1 AND (verified_media=0 OR verified_metadata=0)",[],|r|r.get(0)).unwrap(),0);
        if interrupted {
            assert_eq!(
                conn.query_row::<i64, _, _>(
                    "SELECT count(*) FROM provider_active_generations WHERE sealed=0",
                    [],
                    |r| r.get(0)
                )
                .unwrap(),
                1,
                "writer completion must not seal interrupted rank coverage"
            );
        }
    }
    if refresh {
        // The existing explicit command deliberately queues during enumeration
        // and owns this separate bounded refresh tail after all passes finish.
        let failed = crate::download::orchestration::maintenance::drain_pending_metadata_rewrites(
            f.db.as_ref(),
            &f.config.metadata,
            crate::download::metadata_rewrite::CaptureTimestampRepair::Preserve,
            &["PrimarySync"],
            f.config.temp_suffix.clone(),
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(failed, 0);
        for media_path in &media_paths {
            let sidecar = media_path.with_file_name(format!(
                "{}.xmp",
                media_path.file_name().unwrap().to_str().unwrap()
            ));
            assert!(
                std::fs::read_to_string(sidecar)
                    .unwrap()
                    .contains("new-explicit-title"),
                "ordinary explicit refresh must reach every existing physical writer; stored title: {:?}; actual: {}; events: {:?}",
                f.db.acquire_lock("refresh metadata evidence")
                    .unwrap()
                    .query_row::<Option<String>, _, _>(
                        "SELECT title FROM assets WHERE id='asset-m'",
                        [],
                        |r| r.get(0)
                    )
                    .unwrap(),
                std::fs::read_to_string(media_path.with_file_name(format!(
                    "{}.xmp",
                    media_path.file_name().unwrap().to_str().unwrap()
                )))
                .unwrap(),
                trace
                    .events()
                    .iter()
                    .filter(|e| e.level <= tracing::Level::WARN)
                    .collect::<Vec<_>>()
            );
        }
    }
    let times: Vec<_> = media_paths
        .iter()
        .map(|p| std::fs::metadata(p).unwrap().modified().unwrap())
        .collect();
    let calls = f.session.calls.load(Ordering::SeqCst);
    f.config.refresh_metadata = false;
    f.config.sync_mode = crate::download::SyncMode::Incremental {
        zone_sync_token: "quiet-zone-cursor".into(),
    };
    let quiet = crate::download::download_photos_with_sync(
        &reqwest::Client::new(),
        &passes,
        Arc::new(f.config.clone()),
        controls(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(quiet.stats.downloaded, 0);
    if !refresh {
        assert_eq!(f.session.calls.load(Ordering::SeqCst), calls);
    }
    for (media_path, time) in media_paths.iter().zip(times) {
        assert_eq!(
            std::fs::metadata(media_path).unwrap().modified().unwrap(),
            time
        );
    }
    if let Some((mirror, sidecar, legacy)) = native_collision {
        assert_eq!(std::fs::read(&mirror).unwrap(), media);
        assert_eq!(
            std::fs::read(sidecar).unwrap(),
            b"unrelated-sibling-sidecar"
        );
        let retained =
            f.db.acquire_lock("native receipt cannot settle TEXT sibling")
                .unwrap()
                .query_row::<(Option<String>, Option<i64>), _, _>(
                    "SELECT local_checksum,metadata_write_failed_at FROM assets WHERE id='asset-m'",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
        assert_eq!(
            retained, legacy,
            "native receipt cannot change the checksum or clear the UTF-8 sibling's legacy marker"
        );
    }
    f.preserved().await;
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn private_unresolved_identity_retains_retry_and_does_not_block_healthy_metadata() {
    Box::pin(private_unresolved_recovery_fixture(false, false, false)).await;
}

#[cfg(feature = "xmp")]
async fn private_unresolved_recovery_fixture(drift: bool, native: bool, incompatible: bool) {
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    let media = b"\xff\xd8\xff\xe0\0\x10JFIF\0\x01\x01\0\0\x01\0\x01\0\0\xff\xd9";
    Mock::given(method("GET"))
        .and(path("/unresolved-control"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(media))
        .expect(if incompatible { 1 } else { 2 })
        .mount(&server)
        .await;
    let mut f = Fixture::new().await;
    #[cfg(unix)]
    if native {
        use std::os::unix::ffi::OsStringExt;
        let root = f.dir.path().join(std::ffi::OsString::from_vec(
            b"unresolved-native-\xff".to_vec(),
        ));
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("existing.jpg"), b"existing-media").unwrap();
        std::fs::write(root.join("existing.xmp"), b"retained-sidecar").unwrap();
        f.config.directory = Arc::from(root.as_path());
    }
    #[cfg(not(unix))]
    assert!(!native);
    f.config.folder_structure_albums = Arc::from("{album}");
    f.config.metadata.xmp_sidecar = true;
    let mut observed = records("m", CURRENT, "healthy-title");
    observed.extend(records("c", CURRENT, "later-title"));
    for pair in observed.chunks_mut(2) {
        pair[0]["fields"]["resOriginalRes"]["value"] = json!({"downloadURL":format!("{}/unresolved-control",server.uri()),"size":media.len(),"fileChecksum":data_encoding::BASE64.encode(&Sha256::digest(media))});
    }
    observed[2]["fields"]["filenameEnc"] = json!({"value":"later.JPG","type":"STRING"});
    *f.session.records.lock().unwrap() = observed;
    *f.session.corrupt.lock().unwrap() = Some("missing-c");
    f.capture(records("m", OLD, "historical"), "unresolved-history")
        .await;
    f.pass = destination_pass_with(&f.session, &f.capture, "A");
    crate::download::orchestration::test_support::seed_complete_album_snapshot(
        f.db.as_ref(),
        "A",
        "A",
        &[("asset-m", "m"), ("asset-c", "c")],
    )
    .await;
    let first = crate::download::download_photos_with_sync(
        &reqwest::Client::new(),
        std::slice::from_ref(&f.pass),
        Arc::new(f.config.clone()),
        controls(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(first.stats.downloaded, 1);
    assert!(first.checkpoint.identity_incomplete);
    assert!(first.checkpoint.sync_token_blocked);
    let healthy = f.config.directory.join("A/changed.JPG");
    let sidecar = healthy.with_file_name("changed.JPG.xmp");
    assert!(
        std::fs::read_to_string(&sidecar)
            .unwrap()
            .contains("healthy-title")
    );
    let media_time = std::fs::metadata(&healthy).unwrap().modified().unwrap();
    let sidecar_time = std::fs::metadata(&sidecar).unwrap().modified().unwrap();
    let retry: (String, i64, i64) = f.db.acquire_lock("unresolved durable retry").unwrap().query_row(
        "SELECT generation,attempts,next_retry_at FROM provider_active_decisions WHERE child='asset-c' AND admission='deferred'", [], |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)),
    ).unwrap();
    assert_eq!(retry.1, 1);
    assert!(retry.2 > chrono::Utc::now().timestamp());
    let manifests =
        f.db.acquire_lock("unresolved current proof")
            .unwrap()
            .query_row::<Vec<u8>, _, _>(
                "SELECT manifest FROM provider_active_decisions WHERE child='asset-c'",
                [],
                |r| r.get(0),
            )
            .unwrap();
    let unresolved: crate::state::db::provider_generations::ActiveDecision =
        serde_json::from_slice(&manifests).unwrap();
    assert!(unresolved.decision.confirmation.is_none());
    assert!(!unresolved.sources.is_empty());
    assert_eq!(f.db.acquire_lock("healthy writer proof before coverage").unwrap().query_row::<i64,_,_>(
        "SELECT count(*) FROM provider_active_destinations WHERE child='asset-m' AND verified_metadata=1",[],|r|r.get(0),
    ).unwrap(),1);
    let calls = f.session.calls.load(Ordering::SeqCst);
    let rank_calls = f.session.rank_calls.load(Ordering::SeqCst);
    for _ in 0..2 {
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
        f.config.sync_mode = crate::download::SyncMode::Incremental {
            zone_sync_token: "old-cursor".into(),
        };
        let quiet = crate::download::download_photos_with_sync(
            &reqwest::Client::new(),
            std::slice::from_ref(&f.pass),
            Arc::new(f.config.clone()),
            controls(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(quiet.stats.downloaded, 0);
        assert!(quiet.checkpoint.sync_token_blocked);
        assert_eq!(
            f.session.calls.load(Ordering::SeqCst),
            calls,
            "neither healthy nor not-due identity repeats current lookup"
        );
        assert_eq!(
            f.session.rank_calls.load(Ordering::SeqCst),
            rank_calls,
            "not-due unresolved root must not recapture rank pages"
        );
        assert_eq!(
            std::fs::metadata(&healthy).unwrap().modified().unwrap(),
            media_time
        );
        assert_eq!(
            std::fs::metadata(&sidecar).unwrap().modified().unwrap(),
            sidecar_time
        );
        let actual:(String,i64,i64)=f.db.acquire_lock("unchanged retry deadline").unwrap().query_row(
            "SELECT generation,attempts,next_retry_at FROM provider_active_decisions WHERE child='asset-c'",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)),
        ).unwrap();
        assert_eq!(
            actual, retry,
            "quiet skip cannot continually extend retry backoff"
        );
        f.preserved().await;
    }
    // A due failed lookup with a changed raw rank body must back off again,
    // retaining the first decision's source links and every newer raw page.
    let before_pages =
        f.db.acquire_lock("retained rank history before retry")
            .unwrap()
            .query_row::<i64, _, _>(
                "SELECT count(*) FROM provider_selection_rank_pages",
                [],
                |r| r.get(0),
            )
            .unwrap();
    f.session.records.lock().unwrap()[2]["futureRetryPayload"] =
        json!({"unknown":"retain unchanged intent"});
    f.db.make_selection_retry_due(owner(), "asset-c".into())
        .await
        .unwrap();
    let still_missing = crate::download::download_photos_with_sync(
        &reqwest::Client::new(),
        std::slice::from_ref(&f.pass),
        Arc::new(f.config.clone()),
        controls(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(still_missing.stats.downloaded, 0);
    assert_eq!(
        still_missing.checkpoint.state_write_failures, 0,
        "changed rank bytes must not conflict with frozen unresolved links"
    );
    let (attempts,next,stored):(i64,i64,Vec<u8>)=f.db.acquire_lock("second failure backs off").unwrap().query_row(
        "SELECT attempts,next_retry_at,manifest FROM provider_active_decisions WHERE child='asset-c'",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)),
    ).unwrap();
    assert_eq!(attempts, 2);
    assert!(next >= chrono::Utc::now().timestamp() + 7190);
    assert_eq!(stored, manifests);
    assert!(
        f.db.acquire_lock("changed raw rank remains retained")
            .unwrap()
            .query_row::<i64, _, _>(
                "SELECT count(*) FROM provider_selection_rank_pages",
                [],
                |r| r.get(0)
            )
            .unwrap()
            > before_pages
    );
    let calls = f.session.calls.load(Ordering::SeqCst);
    let quiet = crate::download::download_photos_with_sync(
        &reqwest::Client::new(),
        std::slice::from_ref(&f.pass),
        Arc::new(f.config.clone()),
        controls(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(quiet.stats.downloaded, 0);
    assert_eq!(f.session.calls.load(Ordering::SeqCst), calls);
    if drift {
        f.capture(vec![json!({"recordName":"new-retained-unknown","recordType":"FutureOpaque","future":"preserve"})],"changed-source-basis").await;
        f.db.acquire_lock("synthetic real canonical grouping dependency drift").unwrap().execute_batch("INSERT INTO asset_people(library,asset_id,person_name) VALUES('PrimarySync','asset-m','New Person')").unwrap();
    }
    if incompatible {
        let newer = b"\xff\xd8\xff\xe0\0\x10JFIF\0\x01\x01\0\0\x01\0\x01\0\0\xff\xd9newer";
        Mock::given(method("GET"))
            .and(path("/incompatible-version"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(newer))
            .expect(1)
            .mount(&server)
            .await;
        let mut records = f.session.records.lock().unwrap();
        records[2]["fields"]["filenameEnc"] = json!({"value":"new-version.JPG","type":"STRING"});
        records[2]["fields"]["resOriginalRes"]["value"] = json!({"downloadURL":format!("{}/incompatible-version",server.uri()),"size":newer.len(),"fileChecksum":data_encoding::BASE64.encode(&Sha256::digest(newer))});
    }
    *f.session.corrupt.lock().unwrap() = None;
    f.db.make_selection_retry_due(owner(), "asset-c".into())
        .await
        .unwrap();
    let recovered = crate::download::download_photos_with_sync(
        &reqwest::Client::new(),
        std::slice::from_ref(&f.pass),
        Arc::new(f.config.clone()),
        controls(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        recovered.stats.downloaded, 1,
        "due unresolved identity must become reachable actual queue work: {recovered:?}"
    );
    if incompatible {
        assert!(recovered.checkpoint.sync_token_blocked);
        assert!(f.config.directory.join("A/new-version.JPG").is_file());
        assert!(!f.config.directory.join("A/later.JPG").exists());
        let old: (String, i64, i64, Vec<u8>) = f.db.acquire_lock("incompatible old identity stays debt").unwrap().query_row(
            "SELECT admission,attempts,next_retry_at,manifest FROM provider_active_decisions WHERE generation=?1 AND child='asset-c'", [&retry.0], |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)),
        ).unwrap();
        assert_eq!(old.0, "deferred");
        assert_eq!(old.1, 3);
        assert!(old.2 > chrono::Utc::now().timestamp());
        assert_eq!(old.3, manifests);
        let roots = {
            let conn =
                f.db.acquire_lock("independent old and current coverage")
                    .unwrap();
            assert!(
                !conn
                    .query_row::<bool, _, _>(
                        "SELECT sealed FROM provider_active_generations WHERE id=?1",
                        [&retry.0],
                        |r| r.get(0)
                    )
                    .unwrap()
            );
            assert_eq!(
                conn.query_row::<i64, _, _>(
                    "SELECT count(*) FROM provider_active_generations WHERE sealed=1",
                    [],
                    |r| r.get(0)
                )
                .unwrap(),
                1
            );
            conn.prepare("SELECT id,sealed,seal_hash FROM provider_active_generations ORDER BY id")
                .unwrap()
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, bool>(1)?,
                        r.get::<_, Option<String>>(2)?,
                    ))
                })
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        };
        let calls = f.session.calls.load(Ordering::SeqCst);
        let ranks = f.session.rank_calls.load(Ordering::SeqCst);
        for _ in 0..2 {
            f = reopen_destination_fixture(f).await;
            let quiet = crate::download::download_photos_with_sync(
                &reqwest::Client::new(),
                std::slice::from_ref(&f.pass),
                Arc::new(f.config.clone()),
                controls(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
            assert!(quiet.checkpoint.sync_token_blocked);
            assert_eq!(quiet.stats.downloaded, 0);
            let rows = {
                let conn = f.db.acquire_lock("quiet incompatible diagnostic").unwrap();
                conn.prepare("SELECT generation,child,admission,reason,attempts,next_retry_at,(SELECT group_concat(verified_media || ':' || verified_metadata) FROM provider_active_destinations w WHERE w.generation=d.generation AND w.child=d.child) FROM provider_active_decisions d ORDER BY generation,child").unwrap()
                    .query_map([], |r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,i64>(4)?,r.get::<_,i64>(5)?,r.get::<_,Option<String>>(6)?))).unwrap().collect::<Result<Vec<_>,_>>().unwrap()
            };
            assert_eq!(
                f.session.calls.load(Ordering::SeqCst),
                calls,
                "quiet={quiet:?}; retained={rows:?}"
            );
            assert_eq!(
                f.session.rank_calls.load(Ordering::SeqCst),
                ranks,
                "quiet={quiet:?}; retained={rows:?}"
            );
            assert!(!f.config.directory.join("A/later.JPG").exists());
            let actual = f
                .db
                .acquire_lock("unchanged independent coverage receipts")
                .unwrap()
                .prepare("SELECT id,sealed,seal_hash FROM provider_active_generations ORDER BY id")
                .unwrap()
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, bool>(1)?,
                        r.get::<_, Option<String>>(2)?,
                    ))
                })
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            assert_eq!(actual, roots);
            f.preserved().await;
        }
        return;
    }
    assert_eq!(
        std::fs::read(f.config.directory.join("A/later.JPG")).unwrap(),
        media
    );
    assert!(
        std::fs::read_to_string(f.config.directory.join("A/later.JPG.xmp"))
            .unwrap()
            .contains("later-title")
    );
    assert_eq!(f.db.acquire_lock("atomic unresolved upgrade").unwrap().query_row::<i64,_,_>(
        "SELECT count(*) FROM provider_active_decisions WHERE child='asset-c' AND admission='admitted'",[],|r|r.get(0),
    ).unwrap(),if drift {2} else {1});
    assert_eq!(
        std::fs::metadata(&healthy).unwrap().modified().unwrap(),
        media_time
    );
    if drift {
        assert!(
            std::fs::read_to_string(&sidecar)
                .unwrap()
                .contains("New Person")
        );
        assert_eq!(f.db.acquire_lock("old unresolved identity is resolved without sealing history").unwrap().query_row::<i64,_,_>("SELECT count(*) FROM provider_active_decisions WHERE child='asset-c' AND admission='deferred'",[],|r|r.get(0)).unwrap(),0);
        assert!(
            f.db.acquire_lock("old coverage stays unsealed")
                .unwrap()
                .query_row::<i64, _, _>(
                    "SELECT count(*) FROM provider_active_generations WHERE sealed=0",
                    [],
                    |r| r.get(0)
                )
                .unwrap()
                > 0
        );
    } else {
        assert_eq!(
            std::fs::metadata(&sidecar).unwrap().modified().unwrap(),
            sidecar_time
        );
    }
    let calls = f.session.calls.load(Ordering::SeqCst);
    let rank_calls = f.session.rank_calls.load(Ordering::SeqCst);
    let quiet = crate::download::download_photos_with_sync(
        &reqwest::Client::new(),
        std::slice::from_ref(&f.pass),
        Arc::new(f.config.clone()),
        controls(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(quiet.stats.downloaded, 0);
    assert_eq!(f.session.calls.load(Ordering::SeqCst), calls);
    assert_eq!(f.session.rank_calls.load(Ordering::SeqCst), rank_calls);
    f.preserved().await;
}

#[tokio::test]
async fn private_selection_schema33_migration_preserves_live_wal_work_and_unknown_collision() {
    let fixture = Fixture::new().await;
    fixture
        .capture(records("m", OLD, "historical"), "source")
        .await;
    fixture.cycle().await.unwrap();
    let queue = fixture.queue();
    let work: Vec<u8> = fixture
        .db
        .acquire_lock("retained work bytes")
        .unwrap()
        .query_row(
            "SELECT confirmation FROM provider_work_receipts LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    fixture.db.acquire_lock("synthetic schema33 and unknown table").unwrap().execute_batch(
        "PRAGMA wal_autocheckpoint=0; DROP TABLE provider_active_sources; DROP TABLE provider_active_destinations; DROP TABLE provider_active_decisions; DROP TABLE provider_selection_rank_records; DROP TABLE provider_selection_rank_pages; DROP TABLE provider_active_generations; PRAGMA user_version=33; CREATE TABLE provider_active_generations(payload BLOB); INSERT INTO provider_active_generations VALUES(X'00FF09');"
    ).unwrap();
    let database = fixture.dir.path().join("state.db");
    assert!(
        SqliteStateDb::open_owned(&database, &owner())
            .await
            .is_err()
    );
    {
        let conn = fixture
            .db
            .acquire_lock("migration rollback oracle")
            .unwrap();
        assert_eq!(
            conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            33
        );
        assert_eq!(
            conn.query_row("SELECT payload FROM provider_active_generations", [], |r| r
                .get::<_, Vec<u8>>(0))
                .unwrap(),
            [0, 255, 9]
        );
        assert_eq!(conn.query_row("SELECT count(*) FROM sqlite_schema WHERE type='table' AND name IN ('provider_active_sources','provider_active_decisions','provider_active_destinations','provider_selection_rank_records','provider_selection_rank_pages')", [], |r|r.get::<_,i64>(0)).unwrap(), 0);
        conn.execute_batch(
            "ALTER TABLE provider_active_generations RENAME TO retained_future_selection;",
        )
        .unwrap();
    }
    let migrated = SqliteStateDb::open_owned(&database, &owner())
        .await
        .unwrap();
    assert_eq!(fixture.queue(), queue);
    assert_eq!(
        fixture
            .db
            .acquire_lock("work preserved after migration")
            .unwrap()
            .query_row(
                "SELECT confirmation FROM provider_work_receipts LIMIT 1",
                [],
                |r| r.get::<_, Vec<u8>>(0)
            )
            .unwrap(),
        work
    );
    assert_eq!(
        migrated
            .acquire_lock("future table preserved")
            .unwrap()
            .query_row("SELECT payload FROM retained_future_selection", [], |r| r
                .get::<_, Vec<
                u8,
            >>(
                0
            ))
            .unwrap(),
        [0, 255, 9]
    );
    assert_eq!(fixture.count("provider_active_generations"), 0);
    fixture.preserved().await;
    drop(migrated);
    fixture.cycle().await.unwrap(); // Previously admitted work is not recreated.
    assert_eq!(fixture.count("provider_active_generations"), 0);
    assert_eq!(fixture.queue(), queue);
}

async fn active_pending_fixture() -> (
    Fixture,
    crate::state::db::provider_generations::ActiveGeneration,
    crate::state::db::provider_generations::ActiveDecision,
    Vec<crate::state::AssetRecord>,
) {
    use crate::state::db::provider_generations::{
        ActiveDecision, GenerationSpec, MAX_GENERATION_BYTES,
    };
    let mut f = Fixture::new().await;
    f.capture(records("m", OLD, "retained"), "active-proofs")
        .await;
    f.pass = destination_pass_with(&f.session, &f.capture, "A");
    let (_, scope, zone) = f.pass.album.private_selection_scope().unwrap().unwrap();
    let key = crate::download::orchestration::generation::pass_key(&f.pass);
    let root=f.db.begin_selection_generation(owner(),GenerationSpec {
        format:1,scope:scope.clone(),zone:zone.clone(),config_hash:"a".repeat(64),
        basis:f.db.selection_basis(owner(),scope).await.unwrap(),
        profile:json!({"coverage":"observed_selection_window","passes":[{"key":key,"scope":f.pass.album.selection_scope()}]}),
        metadata_enabled:false,metadata_flags:0,
    },MAX_GENERATION_BYTES).await.unwrap();
    let album = f.pass.album.clone().with_rank_capture(&root.id, &key);
    assert_eq!(album.photos(None).await.unwrap().len(), 1);
    let current = f.pass.album.confirm_catalog_asset("asset-m").await.unwrap();
    let effective = f.config.with_pass(&f.pass);
    let mut planner = TaskPlanner::for_download(effective.state_db.as_deref())
        .await
        .unwrap();
    let plan = planner
        .plan_download_asset(&current.asset, &effective)
        .await
        .unwrap();
    let records = plan
        .tasks
        .iter()
        .map(|task| pending_record_for_task(&effective, &current.asset, task))
        .collect::<Vec<_>>();
    let manifest = ActiveDecision {
        decision: crate::state::db::provider_selection::SelectionDecision {
            pass_key: key.clone(),
            child: "asset-m".into(),
            master: Some("m".into()),
            confirmation: Some(current.body),
            outcome: SelectionOutcome::Selected,
            reason: String::new(),
            destinations: crate::download::filter::derive_expected_paths(
                &current.asset,
                &effective,
            )
            .into_iter()
            .map(
                |expected| crate::state::db::provider_selection::SelectionDestination {
                    version_size: expected.version_size.as_str().into(),
                    path: SelectionPath::from_path(&std::path::absolute(expected.path).unwrap()),
                    checksum: expected.checksum.into(),
                    size: expected.size,
                    metadata_hash: crate::download::filter::metadata_for_selected_version(
                        &current.asset,
                        &effective,
                        expected.version_size,
                    )
                    .metadata_hash
                    .clone()
                    .unwrap(),
                },
            )
            .collect(),
        },
        sources: f
            .db
            .selection_rank_sources(owner(), root.id.clone(), key, "asset-m".into())
            .await
            .unwrap(),
        state_id: "asset-m".into(),
    };
    assert!(
        f.db.project_selection_decision(
            owner(),
            root.id.clone(),
            manifest.clone(),
            records.clone(),
            MAX_GENERATION_BYTES
        )
        .await
        .unwrap()
        .admitted
    );
    (f, root, manifest, records)
}

#[tokio::test]
async fn private_historical_replay_windows_round_robin_and_persist_past_64_after_reopen() {
    use crate::state::db::provider_generations::{
        ActiveDecision, GenerationSpec, MAX_GENERATION_BYTES,
    };
    let mut f = Fixture::new().await;
    f.pass = destination_pass(&f, "A");
    *f.session.records.lock().unwrap() = (0..72)
        .flat_map(|n| records(&format!("history-{n:03}"), CURRENT, "retained"))
        .collect();
    let (_, scope, zone) = f.pass.album.private_selection_scope().unwrap().unwrap();
    let key = crate::download::orchestration::generation::pass_key(&f.pass);
    let config_hash = "a".repeat(64);
    let mut roots = Vec::new();
    for epoch in 0..2 {
        f.capture(vec![json!({"recordName":format!("future-{epoch}"),"recordType":"FutureOpaque","payload":[0,255,9]})], &format!("history-{epoch}")).await;
        let root = f.db.begin_selection_generation(owner(), GenerationSpec {
            format: 1, scope: scope.clone(), zone: zone.clone(), config_hash: config_hash.clone(),
            basis: f.db.selection_basis(owner(), scope.clone()).await.unwrap(),
            profile: json!({"coverage":"observed_selection_window","passes":[{"key":key,"scope":f.pass.album.selection_scope()}]}),
            metadata_enabled: false, metadata_flags: 0,
        }, MAX_GENERATION_BYTES).await.unwrap();
        let album = f.pass.album.clone().with_rank_capture(&root.id, &key);
        assert_eq!(album.photos(None).await.unwrap().len(), 72);
        for n in 0..72 {
            let child = format!("asset-history-{n:03}");
            let decision = ActiveDecision {
                sources: f
                    .db
                    .selection_rank_sources(owner(), root.id.clone(), key.clone(), child.clone())
                    .await
                    .unwrap(),
                state_id: child.clone(),
                decision: crate::state::db::provider_selection::SelectionDecision {
                    pass_key: key.clone(),
                    child,
                    master: None,
                    confirmation: None,
                    outcome: SelectionOutcome::Deferred,
                    reason: "current_lookup_unresolved".into(),
                    destinations: Vec::new(),
                },
            };
            assert!(
                !f.db
                    .project_selection_decision(
                        owner(),
                        root.id.clone(),
                        decision,
                        Vec::new(),
                        MAX_GENERATION_BYTES
                    )
                    .await
                    .unwrap()
                    .admitted
            );
        }
        roots.push(root);
    }
    // Production deferred admission sets a real retry deadline. Advance only
    // the synthetic clock via its authenticated test owner before scheduling.
    for n in 0..72 {
        f.db.make_selection_retry_due(owner(), format!("asset-history-{n:03}"))
            .await
            .unwrap();
    }
    assert_ne!(roots[0].id, roots[1].id);
    let queue = f.queue();
    let raw: Vec<Vec<u8>> = {
        let conn =
            f.db.acquire_lock("retain original historical pages")
                .unwrap();
        let mut query = conn
            .prepare("SELECT body FROM provider_selection_rank_pages ORDER BY id")
            .unwrap();
        query
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    // Scheduling persists only continuation. It cannot consume debt, invent a
    // provider cursor, or require the failed prefix to resolve before the tail.
    for wave in 0..2 {
        for expected_root in &roots {
            f = reopen_destination_fixture(f).await;
            let selected =
                f.db.retained_selection_root(
                    owner(),
                    scope.clone(),
                    config_hash.clone(),
                    "independent-current-root".into(),
                )
                .await
                .unwrap()
                .unwrap();
            assert_eq!(selected.id, expected_root.id, "round-robin wave {wave}");
            let decisions =
                f.db.retained_selection_decisions(owner(), selected.id)
                    .await
                    .unwrap();
            let expected: Vec<_> = if wave == 0 { 0..64 } else { 64..72 }
                .map(|n| format!("asset-history-{n:03}"))
                .collect();
            assert_eq!(
                decisions
                    .iter()
                    .map(|d| d.decision.child.clone())
                    .collect::<Vec<_>>(),
                expected
            );
            assert!(
                decisions
                    .iter()
                    .all(|d| d.decision.outcome == SelectionOutcome::Deferred)
            );
            assert_eq!(f.queue(), queue);
            f.preserved().await;
        }
    }
    // An early failed prefix is not completion and backs off independently.
    // The due tail remains reachable after wrapping the saved cursor.
    for n in 0..64 {
        f.db.defer_selection_confirmation(
            owner(),
            roots[0].id.clone(),
            key.clone(),
            format!("asset-history-{n:03}"),
        )
        .await
        .unwrap();
    }
    f = reopen_destination_fixture(f).await;
    let tail =
        f.db.retained_selection_decisions(owner(), roots[0].id.clone())
            .await
            .unwrap();
    assert_eq!(
        tail.iter()
            .map(|d| d.decision.child.clone())
            .collect::<Vec<_>>(),
        (64..72)
            .map(|n| format!("asset-history-{n:03}"))
            .collect::<Vec<_>>()
    );
    let conn =
        f.db.acquire_lock("retained scheduling is not consumption")
            .unwrap();
    assert_eq!(
        conn.query_row::<i64, _, _>(
            "SELECT count(*) FROM provider_active_decisions WHERE admission='deferred'",
            [],
            |r| r.get(0)
        )
        .unwrap(),
        144
    );
    let mut query = conn
        .prepare("SELECT body FROM provider_selection_rank_pages ORDER BY id")
        .unwrap();
    assert_eq!(
        query
            .query_map([], |r| r.get::<_, Vec<u8>>(0))
            .unwrap()
            .map(Result::unwrap)
            .collect::<Vec<_>>(),
        raw
    );
}

#[tokio::test]
async fn private_duplicate_rank_source_links_decode_each_original_page_once() {
    use crate::state::db::provider_generations::{
        MAX_GENERATION_BYTES, inspect_selection_decode_cost, inspect_selection_validation_budget,
    };
    let (f, root, mut manifest, _) = active_pending_fixture().await;
    let key = manifest.decision.pass_key.clone();
    let request:Vec<u8>=f.db.acquire_lock("original request for repeated identity proof").unwrap().query_row("SELECT request FROM provider_selection_rank_pages WHERE generation=?1 ORDER BY id LIMIT 1",[&root.id],|r|r.get(0)).unwrap();
    let mut request: Value = serde_json::from_slice(&request).unwrap();
    request["resultsLimit"] = json!(400);
    let child = f.session.records.lock().unwrap()[1].clone();
    let raw=serde_json::to_vec(&json!({"records":vec![child;128],"syncToken":"repeated-observation-only","unknown":{"retain":[0,255,9]}})).unwrap();
    f.db.capture_selection_rank_page(
        owner(),
        root.id.clone(),
        key.clone(),
        request,
        raw.clone(),
        MAX_GENERATION_BYTES,
    )
    .await
    .unwrap();
    manifest.sources =
        f.db.selection_rank_sources(owner(), root.id.clone(), key, "asset-m".into())
            .await
            .unwrap();
    assert_eq!(
        manifest.sources.len(),
        129,
        "every duplicate source ordinal remains retained"
    );
    let expected_bytes =
        f.db.acquire_lock("unique source byte oracle")
            .unwrap()
            .query_row::<i64, _, _>(
                "SELECT sum(length(body)) FROM provider_selection_rank_pages WHERE id IN (SELECT DISTINCT page_id FROM provider_selection_rank_records WHERE record_name='asset-m') AND generation=?1",
                [&root.id],
                |r| r.get(0),
            )
            .unwrap();
    let db = f.db.clone();
    let measured_root = root.clone();
    let measured_manifest = manifest.clone();
    let cost = tokio::task::spawn_blocking(move || {
        let conn = db.acquire_lock("measure actual source validation in one owning operation")?;
        inspect_selection_decode_cost(&conn, &measured_root, &measured_manifest)
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        cost,
        (2, usize::try_from(expected_bytes).unwrap()),
        "bounded source links cannot multiply original raw-page decoding"
    );
    {
        let conn =
            f.db.acquire_lock("bounded original-page validation negative controls")
                .unwrap();
        let original_bytes: i64 = conn.query_row("SELECT sum(length(request)+length(body)) FROM provider_selection_rank_pages WHERE id IN (SELECT DISTINCT page_id FROM provider_selection_rank_records WHERE record_name='asset-m') AND generation=?1", [&root.id], |r| r.get(0)).unwrap();
        let capacity = usize::try_from(original_bytes).unwrap();
        let (fit, decoded) = inspect_selection_validation_budget(&conn, &root, &manifest, capacity);
        assert!(fit.is_ok());
        assert_eq!(decoded, cost);
        let (full, decoded) =
            inspect_selection_validation_budget(&conn, &root, &manifest, capacity - 1);
        assert!(matches!(
            full,
            Err(crate::state::error::StateError::ProviderSelectionFull)
        ));
        assert_eq!(
            decoded.0, 1,
            "refuse the next raw page before decoding or accepting its source links"
        );
        let (full, decoded) = inspect_selection_validation_budget(&conn, &root, &manifest, 0);
        assert!(matches!(
            full,
            Err(crate::state::error::StateError::ProviderSelectionFull)
        ));
        assert_eq!(decoded, (0, 0));
        let page: String = conn.query_row("SELECT r.page_id FROM provider_selection_rank_records r JOIN provider_selection_rank_pages p ON p.id=r.page_id WHERE p.generation=?1 AND r.record_name='m' LIMIT 1", [&root.id], |r| r.get(0)).unwrap();
        let original: Option<String> = conn.query_row("SELECT record_type FROM provider_selection_rank_records WHERE page_id=?1 AND ordinal=0", [&page], |r| r.get(0)).unwrap();
        conn.execute("UPDATE provider_selection_rank_records SET record_type='CorruptAfterPriorOperation' WHERE page_id=?1 AND ordinal=0", [&page]).unwrap();
        assert!(
            matches!(
                inspect_selection_decode_cost(&conn, &root, &manifest),
                Err(crate::state::error::StateError::ProviderSelectionInvalid)
            ),
            "operation-local reuse cannot hide committed normalized-index corruption"
        );
        conn.execute("UPDATE provider_selection_rank_records SET record_type=?2 WHERE page_id=?1 AND ordinal=0", rusqlite::params![page, original]).unwrap();
        assert_eq!(
            inspect_selection_decode_cost(&conn, &root, &manifest).unwrap(),
            cost
        );
        let original_body: Vec<u8> = conn
            .query_row(
                "SELECT body FROM provider_selection_rank_pages WHERE id=?1",
                [&page],
                |r| r.get(0),
            )
            .unwrap();
        let mut corrupted = original_body.clone();
        corrupted.push(b' ');
        conn.execute(
            "UPDATE provider_selection_rank_pages SET body=?2 WHERE id=?1",
            rusqlite::params![page, corrupted],
        )
        .unwrap();
        assert!(matches!(
            inspect_selection_decode_cost(&conn, &root, &manifest),
            Err(crate::state::error::StateError::ProviderSelectionInvalid)
        ));
        conn.execute(
            "UPDATE provider_selection_rank_pages SET body=?2 WHERE id=?1",
            rusqlite::params![page, original_body],
        )
        .unwrap();
        conn.execute("INSERT INTO provider_selection_rank_records(page_id,ordinal,record_name,record_type,deleted) VALUES(?1,999,'extra-unreferenced','Future',0)",[&page]).unwrap();
        assert!(matches!(
            inspect_selection_decode_cost(&conn, &root, &manifest),
            Err(crate::state::error::StateError::ProviderSelectionInvalid)
        ));
        conn.execute(
            "DELETE FROM provider_selection_rank_records WHERE page_id=?1 AND ordinal=999",
            [&page],
        )
        .unwrap();
        let record:(String,Option<String>,bool)=conn.query_row("SELECT record_name,record_type,deleted FROM provider_selection_rank_records WHERE page_id=?1 AND ordinal=0",[&page],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
        conn.execute(
            "DELETE FROM provider_selection_rank_records WHERE page_id=?1 AND ordinal=0",
            [&page],
        )
        .unwrap();
        assert!(matches!(
            inspect_selection_decode_cost(&conn, &root, &manifest),
            Err(crate::state::error::StateError::ProviderSelectionInvalid)
        ));
        conn.execute("INSERT INTO provider_selection_rank_records(page_id,ordinal,record_name,record_type,deleted) VALUES(?1,0,?2,?3,?4)",rusqlite::params![page,record.0,record.1,record.2]).unwrap();
        assert_eq!(
            inspect_selection_decode_cost(&conn, &root, &manifest).unwrap(),
            cost
        );
    }
    assert_eq!(f.count("provider_selection_rank_records"), 130);
    assert!(
        f.db.acquire_lock("original repeated bytes remain retained")
            .unwrap()
            .query_row::<bool, _, _>(
                "SELECT EXISTS(SELECT 1 FROM provider_selection_rank_pages WHERE body=?1)",
                [raw],
                |r| r.get(0)
            )
            .unwrap()
    );
    f.preserved().await;
}

#[tokio::test]
async fn private_quota_valid_large_source_declaration_keeps_same_root_healthy_tail_reachable() {
    use crate::state::db::provider_generations::{
        ActiveDecision, MAX_GENERATION_BYTES, inspect_selection_decode_cost,
    };
    let (mut f, root, original, _) = active_pending_fixture().await;
    let key = original.decision.pass_key.clone();
    let request:Vec<u8>=f.db.acquire_lock("large original source request").unwrap().query_row("SELECT request FROM provider_selection_rank_pages WHERE generation=?1 ORDER BY id LIMIT 1",[&root.id],|r|r.get(0)).unwrap();
    let request: Value = serde_json::from_slice(&request).unwrap();
    let child_name = "asset-000-large";
    let mut child = f.session.records.lock().unwrap()[1].clone();
    child["recordName"] = json!(child_name);
    let padding = "x".repeat(12 * 1024 * 1024);
    for nonce in 0..6 {
        let raw = serde_json::to_vec(
            &json!({"records":[child],"future":{"nonce":nonce,"padding":padding}}),
        )
        .unwrap();
        f.db.capture_selection_rank_page(
            owner(),
            root.id.clone(),
            key.clone(),
            request.clone(),
            raw,
            MAX_GENERATION_BYTES,
        )
        .await
        .unwrap();
    }
    drop(padding);
    let manifest = ActiveDecision {
        sources: f
            .db
            .selection_rank_sources(owner(), root.id.clone(), key.clone(), child_name.into())
            .await
            .unwrap(),
        state_id: child_name.into(),
        decision: crate::state::db::provider_selection::SelectionDecision {
            pass_key: key.clone(),
            child: child_name.into(),
            master: None,
            confirmation: None,
            outcome: SelectionOutcome::Deferred,
            reason: "current_lookup_unresolved".into(),
            destinations: Vec::new(),
        },
    };
    assert_eq!(manifest.sources.len(), 6);
    assert!(
        !f.db
            .project_selection_decision(
                owner(),
                root.id.clone(),
                manifest.clone(),
                Vec::new(),
                MAX_GENERATION_BYTES
            )
            .await
            .unwrap()
            .admitted
    );
    let cost = {
        let conn =
            f.db.acquire_lock("compact validation releases large raw pages")
                .unwrap();
        inspect_selection_decode_cost(&conn, &root, &manifest).unwrap()
    };
    assert_eq!(cost.0, 6);
    assert!(cost.1 > 64 * 1024 * 1024);
    assert!(
        matches!(
            f.db.observed_selection_asset(owner(), root.id.clone(), key.clone(), child_name.into())
                .await,
            Err(crate::state::error::StateError::ProviderSelectionFull)
        ),
        "parsed reconstruction may defer without making the original declaration invalid"
    );
    f.db.defer_selection_confirmation(owner(), root.id.clone(), key.clone(), child_name.into())
        .await
        .unwrap();
    f.db.make_selection_retry_due(owner(), child_name.into())
        .await
        .unwrap();
    let queue = f.queue();
    for _ in 0..2 {
        f = reopen_destination_fixture(f).await;
        let scheduled =
            f.db.retained_selection_decisions(owner(), root.id.clone())
                .await
                .unwrap();
        assert_eq!(
            scheduled
                .iter()
                .map(|d| d.decision.child.as_str())
                .collect::<Vec<_>>(),
            [child_name, "asset-m"],
            "a valid large early declaration cannot strand the same-root pending tail"
        );
        assert_eq!(f.queue(), queue);
        {
            let conn =
                f.db.acquire_lock("all original bytes and unresolved debt remain retained")
                    .unwrap();
            assert_eq!(conn.query_row::<i64,_,_>("SELECT count(*) FROM provider_selection_rank_pages WHERE generation=?1 AND length(body)>12000000",[&root.id],|r|r.get(0)).unwrap(),6);
            assert_eq!(conn.query_row::<i64,_,_>("SELECT count(*) FROM provider_active_decisions WHERE generation=?1 AND child=?2 AND admission='deferred'",rusqlite::params![root.id,child_name],|r|r.get(0)).unwrap(),1);
        }
        f.preserved().await;
    }
}

#[tokio::test]
async fn private_selection_committed_corruption_refuses_pending_publication_proof() {
    for mutation in [
        "UPDATE provider_active_destinations SET admitted=0",
        "UPDATE provider_active_decisions SET admission='excluded'",
        "UPDATE provider_active_decisions SET reason='wrong'",
        "UPDATE provider_active_decisions SET next_retry_at=7",
        "UPDATE provider_active_destinations SET created_at=created_at+1",
        "UPDATE provider_active_destinations SET added_at=added_at+1",
        "UPDATE provider_active_destinations SET checksum='wrong'",
        "UPDATE provider_active_destinations SET compat_path='wrong'",
        "UPDATE provider_active_destinations SET verified_metadata=1",
        "UPDATE provider_active_destinations SET grouping_hash='wrong'",
        "UPDATE provider_active_destinations SET progress_hash='wrong'",
        "UPDATE provider_active_generations SET checkpoint_ready=1",
        "UPDATE provider_active_generations SET specification=X'7B7D'",
        "UPDATE provider_selection_rank_pages SET body=X'7B7D'",
        "UPDATE provider_selection_rank_records SET record_name='wrong'",
        "UPDATE provider_active_decisions SET manifest=X'7B7D'",
        "DELETE FROM provider_active_sources",
    ] {
        let (f, root, manifest, _) = Box::pin(active_pending_fixture()).await;
        f.db.seal_selection_generation(owner(), root.id.clone(), false, None)
            .await
            .unwrap();
        assert!(
            f.db.current_selection_generation(
                owner(),
                root.spec.scope.clone(),
                root.spec.config_hash.clone()
            )
            .await
            .unwrap()
            .is_some()
        );
        assert_eq!(
            f.db.selection_decision(
                owner(),
                root.id.clone(),
                manifest.decision.pass_key.clone(),
                manifest.decision.child.clone()
            )
            .await
            .unwrap(),
            Some(manifest)
        );
        let queue = f.queue();
        f.db.acquire_lock("committed active proof mutation")
            .unwrap()
            .execute_batch(mutation)
            .unwrap();
        let error =
            f.db.current_selection_generation(owner(), root.spec.scope, root.spec.config_hash)
                .await
                .expect_err(mutation);
        assert!(
            matches!(
                error,
                crate::state::error::StateError::ProviderSelectionInvalid
            ),
            "{mutation}: {error:?}"
        );
        assert_eq!(f.queue(), queue);
        f.preserved().await;
    }
}

#[tokio::test]
async fn private_selection_projection_capacity_and_final_write_fault_are_atomic() {
    let (f, root, manifest, records) = Box::pin(active_pending_fixture()).await;
    let queue = f.queue();
    // Idempotent exact replay remains usable even at zero remaining capacity.
    assert!(
        f.db.project_selection_decision(
            owner(),
            root.id.clone(),
            manifest.clone(),
            records.clone(),
            0
        )
        .await
        .unwrap()
        .admitted
    );
    let mut next = manifest.clone();
    next.decision.pass_key = "different-pass".into();
    assert!(matches!(
        f.db.project_selection_decision(owner(), root.id.clone(), next, records.clone(), 0)
            .await,
        Err(crate::state::error::StateError::ProviderSelectionInvalid)
    ));
    assert_eq!(f.queue(), queue);
    let (probe, unsealed, decision, records) = Box::pin(active_pending_fixture()).await;
    // Restore only this synthetic fixture's pending unit to exercise the real
    // complete projection transaction with existing source and owner evidence.
    probe.db.acquire_lock("synthetic pending unit reset").unwrap().execute_batch("DELETE FROM provider_active_destinations; DELETE FROM provider_active_sources; DELETE FROM provider_active_decisions; DELETE FROM asset_master_mappings; DELETE FROM asset_albums; DELETE FROM assets;").unwrap();
    let baseline = probe.queue();
    assert!(matches!(
        probe
            .db
            .project_selection_decision(
                owner(),
                unsealed.id.clone(),
                decision.clone(),
                records.clone(),
                0
            )
            .await,
        Err(crate::state::error::StateError::ProviderSelectionFull)
    ));
    assert_eq!(probe.queue(), baseline);
    assert_eq!(probe.count("provider_active_decisions"), 0);
    probe.db.acquire_lock("fail last consumption write").unwrap().execute_batch("CREATE TRIGGER active_consumption_fault BEFORE UPDATE OF admission ON provider_active_decisions BEGIN SELECT RAISE(ABORT,'synthetic last consumption fault'); END;").unwrap();
    assert!(
        probe
            .db
            .project_selection_decision(
                owner(),
                unsealed.id.clone(),
                decision.clone(),
                records.clone(),
                512 * 1024 * 1024
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("synthetic last consumption fault")
    );
    for table in [
        "assets",
        "asset_master_mappings",
        "asset_albums",
        "provider_active_decisions",
        "provider_active_sources",
        "provider_active_destinations",
    ] {
        assert_eq!(probe.count(table), 0, "{table}");
    }
    probe.preserved().await;
    probe
        .db
        .acquire_lock("remove synthetic final write fault")
        .unwrap()
        .execute_batch("DROP TRIGGER active_consumption_fault")
        .unwrap();
    assert!(
        probe
            .db
            .project_selection_decision(owner(), unsealed.id, decision, records, 512 * 1024 * 1024)
            .await
            .unwrap()
            .admitted
    );
    f.preserved().await;
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn private_unresolved_old_root_recovers_matching_facts_after_source_and_grouping_drift() {
    Box::pin(private_unresolved_recovery_fixture(true, false, false)).await;
}

#[cfg(all(target_os = "linux", feature = "xmp"))]
#[tokio::test]
async fn private_unresolved_old_native_root_recovers_after_source_and_grouping_drift() {
    Box::pin(private_unresolved_recovery_fixture(true, true, false)).await;
}

async fn reopen_destination_fixture(mut f: Fixture) -> Fixture {
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
    f.config.sync_mode = crate::download::SyncMode::Incremental {
        zone_sync_token: "old-cursor".into(),
    };
    f
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn private_incompatible_old_identity_keeps_original_debt_and_backs_off_after_reopen() {
    Box::pin(private_unresolved_recovery_fixture(true, false, true)).await;
}

#[tokio::test]
async fn private_invalidated_old_root_cannot_hide_corrupted_destination_or_header() {
    for mutation in [
        "UPDATE provider_active_destinations SET admitted=0",
        "UPDATE provider_active_generations SET scope='{}'",
        "UPDATE provider_active_generations SET config_hash='wrong'",
        "UPDATE provider_active_generations SET replay_after='[\"missing\",\"child\"]'",
    ] {
        let (f, root, _, _) = Box::pin(active_pending_fixture()).await;
        f.capture(
            vec![json!({"recordName":"later-unknown","recordType":"FutureOpaque"})],
            "invalidate-old-root",
        )
        .await;
        assert!(
            f.db.current_selection_generation(
                owner(),
                root.spec.scope.clone(),
                root.spec.config_hash.clone()
            )
            .await
            .unwrap()
            .is_none()
        );
        assert!(
            f.db.unfinished_selection_debt(owner(), root.spec.scope.clone())
                .await
                .unwrap()
        );
        let before = f.queue();
        f.db.acquire_lock("committed historical corruption")
            .unwrap()
            .execute_batch(mutation)
            .unwrap();
        let error =
            f.db.unfinished_selection_debt(owner(), root.spec.scope.clone())
                .await
                .expect_err(mutation);
        assert!(
            matches!(
                error,
                crate::state::error::StateError::ProviderSelectionInvalid
            ),
            "{mutation}: {error:?}"
        );
        assert_eq!(f.queue(), before);
        f.preserved().await;
    }
}

#[tokio::test]
async fn private_metadata_retry_schedule_is_bounded_and_never_publication_proof() {
    let (f, root, _, _) = Box::pin(active_pending_fixture()).await;
    assert_eq!(
        f.db.selection_metadata_retry_offset(owner()).await.unwrap(),
        0
    );
    f.db.set_selection_metadata_retry_offset(owner(), 501)
        .await
        .unwrap();
    assert_eq!(
        f.db.selection_metadata_retry_offset(owner()).await.unwrap(),
        501
    );
    assert!(
        f.db.unfinished_selection_debt(owner(), root.spec.scope)
            .await
            .unwrap()
    );
    for value in [
        "-1",
        "9223372036854775808",
        "no-offset",
        "000000000000000000000",
    ] {
        f.db.acquire_lock("corrupt retry schedule")
            .unwrap()
            .execute(
                "UPDATE metadata SET value=?1 WHERE key='active_selection_metadata_retry_offset'",
                [value],
            )
            .unwrap();
        assert!(matches!(
            f.db.selection_metadata_retry_offset(owner()).await,
            Err(crate::state::error::StateError::ProviderSelectionInvalid)
        ));
    }
    f.preserved().await;
}

#[cfg(all(target_os = "linux", feature = "xmp"))]
#[tokio::test]
async fn private_completed_native_publications_receive_explicit_current_metadata_without_redownload()
 {
    Box::pin(private_metadata_recovery_fixture(
        false,
        false,
        true,
        true,
        MetadataStateFault::None,
    ))
    .await;
}

#[tokio::test]
async fn private_smart_selector_preserves_existing_dispatch_and_retained_debt_gate() {
    let (mut f, root, _, _) = Box::pin(active_pending_fixture()).await;
    f.pass.kind = PassKind::SmartFolder;
    f.config.media = crate::config::MediaSelection {
        photos: false,
        videos: true,
        live_photos: false,
    };
    assert!(
        crate::download::orchestration::generation::context(
            std::slice::from_ref(&f.pass),
            &f.config,
            controls()
        )
        .await
        .unwrap()
        .is_none()
    );
    let result = crate::download::download_photos_with_sync(
        &reqwest::Client::new(),
        std::slice::from_ref(&f.pass),
        Arc::new(f.config.clone()),
        controls(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(result.stats.downloaded, 0);
    assert!(result.full_enumeration_ran);
    assert!(result.checkpoint.sync_token_blocked);
    assert!(result.sync_token.is_none());
    assert_eq!(
        result.stats.sync_token_blocked_reason,
        Some("selection_generation_deferred")
    );
    assert_eq!(f.count("provider_active_generations"), 1);
    assert!(
        f.db.unfinished_selection_debt(owner(), root.spec.scope)
            .await
            .unwrap()
    );
    f.preserved().await;
    let mut healthy = Fixture::new().await;
    healthy.pass = destination_pass_with(&healthy.session, &healthy.capture, "A");
    healthy.pass.kind = PassKind::SmartFolder;
    healthy.config.media = crate::config::MediaSelection {
        photos: false,
        videos: true,
        live_photos: false,
    };
    let healthy_result = crate::download::download_photos_with_sync(
        &reqwest::Client::new(),
        std::slice::from_ref(&healthy.pass),
        Arc::new(healthy.config.clone()),
        controls(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(healthy_result.stats.downloaded, 0);
    assert!(!healthy_result.checkpoint.sync_token_blocked);
    assert!(healthy_result.sync_token.is_some());
    assert_eq!(healthy.count("provider_active_generations"), 0);
    healthy.preserved().await;
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn private_current_destination_and_retained_tail_progress_past_501_failed_writers() {
    Box::pin(destination_reopen_fixture(false, true, true)).await;
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn private_prepared_metadata_option_drift_retains_old_flags_and_allows_healthy_current_writers()
 {
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    let media = b"\xff\xd8\xff\xe0\0\x10JFIF\0\x01\x01\0\0\x01\0\x01\0\0\xff\xd9";
    let mut healthy = media.to_vec();
    healthy.extend_from_slice(b"healthy-independent-child");
    for (name, bytes) in [
        ("prepared", media.as_slice()),
        ("healthy", healthy.as_slice()),
    ] {
        Mock::given(method("GET"))
            .and(path(format!("/{name}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes))
            .expect(1)
            .mount(&server)
            .await;
    }
    let mut f = Fixture::new().await;
    f.config.folder_structure_albums = Arc::from("copies");
    f.config.metadata.embed_xmp = true;
    f.config.metadata.xmp_sidecar = true;
    f.session.records.lock().unwrap()[0]["fields"]["resOriginalRes"]["value"] = json!({"downloadURL":format!("{}/prepared",server.uri()),"size":media.len(),"fileChecksum":data_encoding::BASE64.encode(&Sha256::digest(media))});
    f.db.acquire_lock("interrupt after authorized actual prepared output").unwrap().execute_batch("CREATE TRIGGER option_drift_finalization_fault BEFORE UPDATE OF verified_metadata ON provider_active_destinations WHEN NEW.verified_metadata=1 BEGIN SELECT RAISE(ABORT,'synthetic option drift metadata finalization'); END;").unwrap();
    let first = crate::download::download_photos_with_sync(
        &reqwest::Client::new(),
        &[destination_pass(&f, "A"), destination_pass(&f, "B")],
        Arc::new(f.config.clone()),
        controls(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(first.stats.downloaded, 1, "{first:?}");
    assert!(first.sync_token.is_none());
    let old =
        f.db.acquire_lock("prepared original writer proof")
            .unwrap()
            .query_row::<String, _, _>("SELECT id FROM provider_active_generations", [], |r| {
                r.get(0)
            })
            .unwrap();
    assert!(f.db.acquire_lock("actual prepared output precedes receipt failure").unwrap().query_row::<i64,_,_>("SELECT count(*) FROM provider_active_destinations WHERE prepared_checksum IS NOT NULL AND verified_metadata=0",[],|r|r.get(0)).unwrap()>0);
    let path = f.config.directory.join("copies/changed.JPG");
    let sidecar = path.with_file_name("changed.JPG.xmp");
    let original = (
        std::fs::read(&path).unwrap(),
        std::fs::metadata(&path).unwrap().modified().unwrap(),
        std::fs::read(&sidecar).unwrap(),
        std::fs::metadata(&sidecar).unwrap().modified().unwrap(),
    );
    assert_ne!(
        original.0, media,
        "the explicit original embed option must have reached actual publication"
    );
    f.db.acquire_lock("remove only synthetic finalization fault")
        .unwrap()
        .execute_batch("DROP TRIGGER option_drift_finalization_fault;")
        .unwrap();
    f.config.metadata.embed_xmp = false;
    let mut new = records("c", CURRENT, "Healthy current sidecar");
    new[0]["fields"]["filenameEnc"]["value"] = json!("healthy.jpg");
    new[0]["fields"]["resOriginalRes"]["value"] = json!({"downloadURL":format!("{}/healthy",server.uri()),"size":healthy.len(),"fileChecksum":data_encoding::BASE64.encode(&Sha256::digest(&healthy))});
    f.session.records.lock().unwrap().extend(new);
    f.capture(vec![json!({"recordName":"future-option-drift","recordType":"FutureOpaque","payload":[0,255,9]})],"option-drift-source").await;
    f = reopen_destination_fixture(f).await;
    let current = crate::download::download_photos_with_sync(
        &reqwest::Client::new(),
        &[destination_pass(&f, "A"), destination_pass(&f, "B")],
        Arc::new(f.config.clone()),
        controls(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(current.stats.downloaded, 1, "{current:?}");
    assert!(
        current.sync_token.is_none(),
        "disabled historical writer remains unfinished debt"
    );
    assert_eq!(f.db.acquire_lock("disabled frozen embed cannot be acknowledged by sidecar only").unwrap().query_row::<i64,_,_>("SELECT count(*) FROM provider_active_destinations WHERE generation=?1 AND verified_metadata=0",[&old],|r|r.get(0)).unwrap(),2);
    assert_eq!(
        std::fs::read(&path).unwrap(),
        original.0,
        "disabled embed preserves prepared media bytes"
    );
    assert_eq!(
        std::fs::metadata(&path).unwrap().modified().unwrap(),
        original.1,
        "disabled embed preserves media timestamp"
    );
    assert_eq!(
        std::fs::read(&sidecar).unwrap(),
        original.2,
        "identical sidecar preserves all original packet bytes"
    );
    assert_eq!(
        std::fs::metadata(&sidecar).unwrap().modified().unwrap(),
        original.3,
        "identical sidecar does not repeat publication"
    );
    let healthy_path = f.config.directory.join("copies/healthy.JPG");
    assert_eq!(std::fs::read(&healthy_path).unwrap(), healthy);
    assert!(
        std::fs::read_to_string(healthy_path.with_file_name("healthy.JPG.xmp"))
            .unwrap()
            .contains("Healthy current sidecar")
    );
    assert_eq!(f.db.acquire_lock("independent current sidecar work finishes").unwrap().query_row::<i64,_,_>("SELECT count(*) FROM provider_active_destinations WHERE child='asset-c' AND verified_metadata=1",[],|r|r.get(0)).unwrap(),2);
    f.config.metadata.embed_xmp = true;
    f = reopen_destination_fixture(f).await;
    let restored = crate::download::download_photos_with_sync(
        &reqwest::Client::new(),
        &[destination_pass(&f, "A"), destination_pass(&f, "B")],
        Arc::new(f.config.clone()),
        controls(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(restored.stats.downloaded, 0, "{restored:?}");
    assert_eq!(
        f.db.acquire_lock("authorized original debt recovers after options restored")
            .unwrap()
            .query_row::<i64, _, _>(
                "SELECT count(*) FROM provider_active_destinations WHERE verified_metadata=0",
                [],
                |r| r.get(0)
            )
            .unwrap(),
        0
    );
    let calls = f.session.calls.load(Ordering::SeqCst);
    let ranks = f.session.rank_calls.load(Ordering::SeqCst);
    let requests = server.received_requests().await.unwrap().len();
    let final_bytes = std::fs::read(&path).unwrap();
    let final_time = std::fs::metadata(&path).unwrap().modified().unwrap();
    for _ in 0..2 {
        f = reopen_destination_fixture(f).await;
        let quiet = crate::download::download_photos_with_sync(
            &reqwest::Client::new(),
            &[destination_pass(&f, "A"), destination_pass(&f, "B")],
            Arc::new(f.config.clone()),
            controls(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(quiet.stats.downloaded, 0);
        assert_eq!(f.session.calls.load(Ordering::SeqCst), calls);
        assert_eq!(f.session.rank_calls.load(Ordering::SeqCst), ranks);
        assert_eq!(server.received_requests().await.unwrap().len(), requests);
        assert_eq!(std::fs::read(&path).unwrap(), final_bytes);
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            final_time
        );
        f.preserved().await;
    }
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn private_300_current_writers_and_600_aliases_finish_through_reseeds_without_repeated_writes()
 {
    Box::pin(current_writers_reseed_fixture(300)).await;
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn private_unchanged_sidecar_packets_survive_reseeds_and_two_quiet_reopens() {
    Box::pin(current_writers_reseed_fixture(3)).await;
}

#[cfg(feature = "xmp")]
async fn current_writers_reseed_fixture(writers: usize) {
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    let media = b"\xff\xd8\xff\xe0\0\x10JFIF\0\x01\x01\0\0\x01\0\x01\0\0\xff\xd9";
    Mock::given(method("GET"))
        .and(path("/many-owned-current"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(media))
        .expect(u64::try_from(writers).unwrap())
        .mount(&server)
        .await;
    let mut f = Fixture::new().await;
    f.config.folder_structure_albums = Arc::from("copies");
    f.config.metadata.xmp_sidecar = true;
    *f.session.records.lock().unwrap()=(0..writers).flat_map(|n| {
        let mut pair=records(&format!("writer-{n:03}"),CURRENT,"Current metadata");
        pair[0]["fields"]["filenameEnc"]["value"]=json!(format!("writer-{n:03}.jpg"));
        pair[0]["fields"]["resOriginalRes"]["value"]=json!({"downloadURL":format!("{}/many-owned-current",server.uri()),"size":media.len(),"fileChecksum":data_encoding::BASE64.encode(&Sha256::digest(media))});pair
    }).collect();
    let mut stable = None;
    for cycle in 0..5 {
        if cycle > 0 {
            f = reopen_destination_fixture(f).await;
        }
        if cycle < 3 {
            f.capture(vec![json!({"recordName":format!("future-writer-{cycle}"),"recordType":"FutureOpaque","payload":[0,255,9]})],&format!("writer-basis-{cycle}")).await;
        }
        let before_calls = f.session.calls.load(Ordering::SeqCst);
        let before_ranks = f.session.rank_calls.load(Ordering::SeqCst);
        let result = crate::download::download_photos_with_sync(
            &reqwest::Client::new(),
            &[destination_pass(&f, "A"), destination_pass(&f, "B")],
            Arc::new(f.config.clone()),
            controls(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            result.stats.downloaded,
            if cycle == 0 { writers } else { 0 },
            "cycle{cycle}: {result:?}"
        );
        {
            let conn =
                f.db.acquire_lock("physical writer versus pass obligation oracle")
                    .unwrap();
            assert_eq!(conn.query_row::<(i64,i64),_,_>("SELECT count(DISTINCT path),count(*) FROM provider_active_destinations WHERE verified_media=1 AND verified_metadata=1",[],|r|Ok((r.get(0)?,r.get(1)?))).unwrap(),(i64::try_from(writers).unwrap(),i64::try_from(2*writers*(cycle.min(2)+1)).unwrap()));
            assert_eq!(
                conn.query_row::<i64, _, _>(
                    "SELECT count(*) FROM provider_active_destinations WHERE verified_metadata=0",
                    [],
                    |r| r.get(0)
                )
                .unwrap(),
                0,
                "current lane and remaining budget cannot strand the 51st writer"
            );
        }
        let files: Vec<_> = (0..writers)
            .map(|n| {
                let path = f
                    .config
                    .directory
                    .join("copies")
                    .join(format!("writer-{n:03}.JPG"));
                let sidecar = path.with_file_name(format!("writer-{n:03}.JPG.xmp"));
                assert_eq!(std::fs::read(&path).unwrap(), media);
                assert!(
                    std::fs::read_to_string(&sidecar)
                        .unwrap()
                        .contains("Current metadata")
                );
                (
                    std::fs::metadata(path).unwrap().modified().unwrap(),
                    std::fs::metadata(sidecar).unwrap().modified().unwrap(),
                )
            })
            .collect();
        if let Some(stable) = &stable {
            assert_eq!(
                stable, &files,
                "unchanged current receipts must survive source reseeds without repeated physical writes"
            );
        } else {
            stable = Some(files);
        }
        if cycle >= 3 {
            assert_eq!(f.session.calls.load(Ordering::SeqCst), before_calls);
            assert_eq!(f.session.rank_calls.load(Ordering::SeqCst), before_ranks);
        }
        f.preserved().await;
    }
}

#[tokio::test]
async fn private_multi_destination_raw_edited_alternative_and_companion_failure_reopen_quietly() {
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let jpeg = b"\xff\xd8\xff\xe0\0\x10JFIF\0\x01\x01\0\0\x01\0\x01\0\0\xff\xd9";
    let raw = include_bytes!("../../../../tests/data/media/pattern.dng");
    let movie = include_bytes!("../../../../tests/data/media/pattern.mov");
    let mut edited = jpeg.to_vec();
    edited.extend_from_slice(b"edited");
    for policy in [
        crate::types::RawPolicy::AsIs,
        crate::types::RawPolicy::PreferRaw,
        crate::types::RawPolicy::PreferJpeg,
    ] {
        let server = MockServer::start().await;
        for (name, bytes) in [
            ("r", jpeg.as_slice()),
            ("raw", raw.as_slice()),
            ("edited", edited.as_slice()),
            ("l", jpeg.as_slice()),
        ] {
            Mock::given(method("GET"))
                .and(path(format!("/{name}")))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes))
                .expect(2)
                .mount(&server)
                .await;
        }
        let fail = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let failure = fail.clone();
        Mock::given(method("GET"))
            .and(path("/movie"))
            .respond_with(move |_: &wiremock::Request| {
                if failure.load(Ordering::SeqCst) {
                    ResponseTemplate::new(500)
                } else {
                    ResponseTemplate::new(200).set_body_bytes(movie.as_slice())
                }
            })
            .expect(4..=6)
            .mount(&server)
            .await;
        let mut f = Fixture::new().await;
        f.config.folder_structure_albums = Arc::from("{album}");
        f.config.raw_policy = policy;
        f.config.alternative = true;
        f.config.edited = true;
        f.config.retry.max_retries = 0;
        let resource = |name: &str, bytes: &[u8]| json!({"value":{"downloadURL":format!("{}/{name}",server.uri()),"size":bytes.len(),"fileChecksum":data_encoding::BASE64.encode(&Sha256::digest(bytes))}});
        let mut r = records("r", CURRENT, "RAW pair");
        r[0]["fields"]["filenameEnc"]["value"] = json!("r.jpg");
        r[0]["fields"]["resOriginalRes"] = resource("r", jpeg);
        r[0]["fields"]["resOriginalAltRes"] = resource("raw", raw);
        r[0]["fields"]["resOriginalAltFileType"] = json!({"value":"com.adobe.raw-image"});
        r[1]["fields"]["resJPEGFullRes"] = resource("edited", &edited);
        r[1]["fields"]["resJPEGFullFileType"] = json!({"value":"public.jpeg"});
        let mut l = records("l", CURRENT, "Live companion");
        l[0]["fields"]["filenameEnc"]["value"] = json!("l.jpg");
        l[0]["fields"]["resOriginalRes"] = resource("l", jpeg);
        l[0]["fields"]["resOriginalVidComplRes"] = resource("movie", movie);
        l[0]["fields"]["resOriginalVidComplFileType"] =
            json!({"value":"com.apple.quicktime-movie"});
        *f.session.records.lock().unwrap() = [r, l].concat();
        f.capture(vec![json!({"recordName":"future-renditions","recordType":"FutureOpaque","payload":[0,255,9]})],"rendition-observation").await;
        let first = crate::download::download_photos_with_sync(
            &reqwest::Client::new(),
            &[destination_pass(&f, "A"), destination_pass(&f, "B")],
            Arc::new(f.config.clone()),
            controls(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(first.stats.downloaded, 8, "{policy:?}: {first:?}");
        assert!(first.checkpoint.sync_token_blocked);
        assert!(first.sync_token.is_none());
        assert_eq!(
            f.db.acquire_lock("failed companions retain both physical obligations")
                .unwrap()
                .query_row::<i64, _, _>(
                    "SELECT count(*) FROM provider_active_destinations WHERE verified_media=0",
                    [],
                    |r| r.get(0)
                )
                .unwrap(),
            2
        );
        f.preserved().await;
        fail.store(false, Ordering::SeqCst);
        f = reopen_destination_fixture(f).await;
        let recovered = crate::download::download_photos_with_sync(
            &reqwest::Client::new(),
            &[destination_pass(&f, "A"), destination_pass(&f, "B")],
            Arc::new(f.config.clone()),
            controls(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(recovered.stats.downloaded, 2, "{policy:?}: {recovered:?}");
        let names = if policy == crate::types::RawPolicy::PreferRaw {
            vec![("r.DNG", raw.as_slice()), ("r_alt.JPG", jpeg.as_slice())]
        } else {
            vec![("r.JPG", jpeg.as_slice()), ("r_RAW.DNG", raw.as_slice())]
        };
        for album in ["A", "B"] {
            for (name, bytes) in names.iter().copied().chain([
                ("r_edited.JPG", edited.as_slice()),
                ("l.JPG", jpeg.as_slice()),
                ("l.MOV", movie.as_slice()),
            ]) {
                let path = f.config.directory.join(album).join(name);
                assert_eq!(
                    std::fs::read(&path).unwrap(),
                    bytes,
                    "{policy:?}: {}",
                    path.display()
                );
            }
        }
        assert_eq!(f.db.acquire_lock("asset versus physical rendition oracle").unwrap().query_row::<(i64,i64),_,_>("SELECT count(DISTINCT child),count(*) FROM provider_active_destinations WHERE verified_media=1 AND verified_metadata=1",[],|r|Ok((r.get(0)?,r.get(1)?))).unwrap(),(2,10));
        let lookups = f.session.calls.load(Ordering::SeqCst);
        let ranks = f.session.rank_calls.load(Ordering::SeqCst);
        for _ in 0..2 {
            f = reopen_destination_fixture(f).await;
            let quiet = crate::download::download_photos_with_sync(
                &reqwest::Client::new(),
                &[destination_pass(&f, "A"), destination_pass(&f, "B")],
                Arc::new(f.config.clone()),
                controls(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
            assert_eq!(quiet.stats.downloaded, 0, "{policy:?}: {quiet:?}");
            assert_eq!(f.session.calls.load(Ordering::SeqCst), lookups);
            assert_eq!(f.session.rank_calls.load(Ordering::SeqCst), ranks);
            f.preserved().await;
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn private_selected_paths_preserve_regular_leaf_and_parent_symlink_conflicts_then_recover() {
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::symlink;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    for kind in ["regular", "leaf", "parent"] {
        let server = MockServer::start().await;
        let media = b"\xff\xd8\xff\xe0\0\x10JFIF\0\x01\x01\0\0\x01\0\x01\0\0\xff\xd9";
        Mock::given(method("GET"))
            .and(path("/confined-current"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(media))
            .expect(2)
            .mount(&server)
            .await;
        let mut f = Fixture::new().await;
        f.config.folder_structure_albums = Arc::from("{album}");
        f.config.retry.max_retries = 0;
        f.session.records.lock().unwrap()[0]["fields"]["filenameEnc"]["value"] =
            json!("confined.JPG");
        f.session.records.lock().unwrap()[0]["fields"]["resOriginalRes"]["value"] = json!({"downloadURL":format!("{}/confined-current",server.uri()),"size":media.len(),"fileChecksum":data_encoding::BASE64.encode(&Sha256::digest(media))});
        f.capture(vec![json!({"recordName":"future-confined","recordType":"FutureOpaque","payload":[0,255,9]})],"confined-old-source").await;
        let outside = f.dir.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        let outside_media = outside.join("confined.JPG");
        std::fs::write(&outside_media, b"outside-owned-media").unwrap();
        let outside_sidecar = outside.join("confined.xmp");
        std::fs::write(&outside_sidecar, b"outside-owned-sidecar").unwrap();
        let album = f.config.directory.join("A");
        let conflict = album.join("confined.JPG");
        if kind == "parent" {
            symlink(&outside, &album).unwrap();
        } else {
            std::fs::create_dir(&album).unwrap();
            if kind == "leaf" {
                symlink(&outside_media, &conflict).unwrap();
            } else {
                std::fs::write(&conflict, b"different-existing-media").unwrap();
            }
        }
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
        assert_eq!(
            first.stats.downloaded,
            if kind == "parent" { 1 } else { 2 },
            "{kind}: {first:?}"
        );
        assert_eq!(
            std::fs::read(&outside_media).unwrap(),
            b"outside-owned-media"
        );
        assert_eq!(
            std::fs::read(&outside_sidecar).unwrap(),
            b"outside-owned-sidecar"
        );
        if kind == "regular" {
            assert_eq!(
                std::fs::read(&conflict).unwrap(),
                b"different-existing-media"
            );
        }
        if kind == "leaf" {
            assert!(std::fs::symlink_metadata(&conflict).unwrap().is_symlink());
        }
        if kind == "parent" {
            assert!(first.checkpoint.sync_token_blocked);
            assert!(first.sync_token.is_none());
            assert_eq!(
                f.db.acquire_lock("parent symlink cannot receive proof")
                    .unwrap()
                    .query_row::<i64, _, _>(
                        "SELECT count(*) FROM provider_active_destinations WHERE verified_media=0",
                        [],
                        |r| r.get(0)
                    )
                    .unwrap(),
                1
            );
            // Remove only the owned fixture link. No existing target is changed.
            std::fs::remove_file(&album).unwrap();
            std::fs::create_dir(&album).unwrap();
            drop(passes);
            f = reopen_destination_fixture(f).await;
            let recovered = crate::download::download_photos_with_sync(
                &reqwest::Client::new(),
                &[destination_pass(&f, "A"), destination_pass(&f, "B")],
                Arc::new(f.config.clone()),
                controls(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
            assert_eq!(recovered.stats.downloaded, 1, "{recovered:?}");
        } else {
            drop(passes);
        }
        let verified: Vec<(String, String)> = {
            let conn =
                f.db.acquire_lock("every destination has actual confined bytes")
                    .unwrap();
            let mut query=conn.prepare("SELECT path,local_checksum FROM provider_active_destinations WHERE verified_media=1 ORDER BY path").unwrap();
            query
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .map(Result::unwrap)
                .collect()
        };
        assert_eq!(verified.len(), 2);
        for (path, checksum) in &verified {
            let path: SelectionPath = serde_json::from_str(path).unwrap();
            let path = path.to_path();
            assert_eq!(std::fs::read(&path).unwrap(), media);
            assert_eq!(
                *checksum,
                data_encoding::HEXLOWER.encode(&Sha256::digest(media))
            );
            assert!(!std::fs::symlink_metadata(&path).unwrap().is_symlink());
            assert!(path.starts_with(&f.config.directory));
            if kind != "parent" {
                assert_ne!(
                    path, conflict,
                    "ordinary collision cannot overwrite the conflicting entry"
                );
            }
        }
        let lookups = f.session.calls.load(Ordering::SeqCst);
        let ranks = f.session.rank_calls.load(Ordering::SeqCst);
        for _ in 0..2 {
            f = reopen_destination_fixture(f).await;
            let quiet = crate::download::download_photos_with_sync(
                &reqwest::Client::new(),
                &[destination_pass(&f, "A"), destination_pass(&f, "B")],
                Arc::new(f.config.clone()),
                controls(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
            assert_eq!(quiet.stats.downloaded, 0, "{kind}: {quiet:?}");
            assert_eq!(f.session.calls.load(Ordering::SeqCst), lookups);
            assert_eq!(f.session.rank_calls.load(Ordering::SeqCst), ranks);
            f.preserved().await;
        }
        assert_eq!(
            std::fs::read(&outside_media).unwrap(),
            b"outside-owned-media"
        );
        assert_eq!(
            std::fs::read(&outside_sidecar).unwrap(),
            b"outside-owned-sidecar"
        );
    }
}

#[tokio::test]
async fn private_replay_raw_cross_pass_aliases_refresh_only_original_failed_resource() {
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    let raw = include_bytes!("../../../../tests/data/media/pattern.dng");
    let jpeg = b"\xff\xd8\xff\xe0\0\x10JFIF\0\x01\x01\0\0\x01\0\x01\0\0\xff\xd9";
    let mut f = Fixture::new().await;
    f.config.folder_structure_albums = Arc::from("copies");
    f.config.raw_policy = crate::types::RawPolicy::PreferRaw;
    f.config.retry.max_retries = 0;
    {
        let mut records = f.session.records.lock().unwrap();
        records[0]["fields"]["resOriginalRes"]["value"] = json!({"downloadURL":format!("{}/unused-jpeg",server.uri()),"size":jpeg.len(),"fileChecksum":data_encoding::BASE64.encode(&Sha256::digest(jpeg))});
        records[0]["fields"]["resOriginalAltRes"] = json!({"value":{"downloadURL":format!("{}/blocked-raw",server.uri()),"size":raw.len(),"fileChecksum":data_encoding::BASE64.encode(&Sha256::digest(raw))}});
        records[0]["fields"]["resOriginalAltFileType"] = json!({"value":"com.adobe.raw-image"});
    }
    let mode = Arc::new(std::sync::atomic::AtomicU8::new(0));
    let active_mode = mode.clone();
    let records = f.session.records.clone();
    let fresh = format!("{}/fresh-original-raw", server.uri());
    Mock::given(method("GET"))
        .and(path("/blocked-raw"))
        .respond_with(move |_: &wiremock::Request| {
            if active_mode.load(Ordering::SeqCst) == 0 {
                ResponseTemplate::new(500)
            } else {
                records.lock().unwrap()[0]["fields"]["resOriginalAltRes"]["value"]["downloadURL"] =
                    json!(fresh);
                ResponseTemplate::new(410)
            }
        })
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/fresh-original-raw"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(raw))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/unused-jpeg"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    let first = crate::download::download_photos_with_sync(
        &reqwest::Client::new(),
        &[destination_pass(&f, "A"), destination_pass(&f, "B")],
        Arc::new(f.config.clone()),
        controls(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(first.stats.downloaded, 0, "{first:?}");
    assert!(first.sync_token.is_none());
    let roots = f.count("provider_active_generations");
    assert_eq!(roots, 1);
    assert_eq!(
        f.db.acquire_lock("both original aliases remain independently unfinished")
            .unwrap()
            .query_row::<i64, _, _>(
                "SELECT count(*) FROM provider_active_destinations WHERE verified_media=0",
                [],
                |r| r.get(0)
            )
            .unwrap(),
        2
    );
    let rank_calls = f.session.rank_calls.load(Ordering::SeqCst);
    mode.store(1, Ordering::SeqCst);
    f = reopen_destination_fixture(f).await;
    let recovered = crate::download::download_photos_with_sync(
        &reqwest::Client::new(),
        &[destination_pass(&f, "A"), destination_pass(&f, "B")],
        Arc::new(f.config.clone()),
        controls(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(recovered.stats.downloaded, 1, "{recovered:?}");
    assert_eq!(
        f.session.rank_calls.load(Ordering::SeqCst),
        rank_calls,
        "sealed pending root replays without a new rank inventory"
    );
    assert_eq!(f.count("provider_active_generations"), roots);
    let physical = f.config.directory.join("copies/changed.DNG");
    assert_eq!(std::fs::read(&physical).unwrap(), raw);
    assert_eq!(f.db.acquire_lock("RAW provider swap cannot retire a JPEG or only one alias").unwrap().query_row::<i64,_,_>("SELECT count(*) FROM provider_active_destinations WHERE version_size='original' AND verified_media=1 AND verified_metadata=1",[],|r|r.get(0)).unwrap(),2);
    let modified = std::fs::metadata(&physical).unwrap().modified().unwrap();
    let requests = server.received_requests().await.unwrap().len();
    let lookups = f.session.calls.load(Ordering::SeqCst);
    for _ in 0..2 {
        f = reopen_destination_fixture(f).await;
        let quiet = crate::download::download_photos_with_sync(
            &reqwest::Client::new(),
            &[destination_pass(&f, "A"), destination_pass(&f, "B")],
            Arc::new(f.config.clone()),
            controls(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(quiet.stats.downloaded, 0, "{quiet:?}");
        assert_eq!(f.session.rank_calls.load(Ordering::SeqCst), rank_calls);
        assert_eq!(f.session.calls.load(Ordering::SeqCst), lookups);
        assert_eq!(server.received_requests().await.unwrap().len(), requests);
        assert_eq!(
            std::fs::metadata(&physical).unwrap().modified().unwrap(),
            modified
        );
        f.preserved().await;
    }
}

#[tokio::test]
async fn private_expired_url_refresh_preserves_frozen_resource_identity_then_recovers_after_reopen()
{
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    for drift in ["none", "checksum", "master", "metadata"] {
        let server = MockServer::start().await;
        let media = b"\xff\xd8\xff\xe0\0\x10JFIF\0\x01\x01\0\0\x01\0\x01\0\0\xff\xd9";
        let mut f = Fixture::new().await;
        f.pass = destination_pass(&f, "A");
        f.config.retry.max_retries = 0;
        #[cfg(feature = "xmp")]
        {
            f.config.metadata.xmp_sidecar = true;
        }
        let good = format!("{}/fresh-current", server.uri());
        f.session.records.lock().unwrap()[0]["fields"]["resOriginalRes"]["value"] = json!({"downloadURL":format!("{}/expired-current",server.uri()),"size":media.len(),"fileChecksum":data_encoding::BASE64.encode(&Sha256::digest(media))});
        let current = f.session.records.clone();
        let lookup_fault = f.session.corrupt.clone();
        let replacement = good.clone();
        let mutate = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let mutation = mutate.clone();
        Mock::given(method("GET"))
            .and(path("/expired-current"))
            .respond_with(move |_: &wiremock::Request| {
                if mutation.swap(false, Ordering::SeqCst) {
                    let mut records = current.lock().unwrap();
                    records[0]["fields"]["resOriginalRes"]["value"]["downloadURL"] =
                        json!(replacement);
                    match drift {
                        "checksum" => {
                            records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
                                json!(CURRENT)
                        }
                        "master" => *lookup_fault.lock().unwrap() = Some("pair"),
                        "metadata" => {
                            records[1]["fields"]["captionEnc"]["value"] =
                                json!("different current title")
                        }
                        _ => {}
                    }
                }
                ResponseTemplate::new(410)
            })
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/fresh-current"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(media))
            .expect(1)
            .mount(&server)
            .await;
        f.capture(vec![json!({"recordName":"future-refresh","recordType":"FutureOpaque","payload":[0,255,9]})],"refresh-original-source").await;
        let first = crate::download::download_photos_with_sync(
            &reqwest::Client::new(),
            std::slice::from_ref(&f.pass),
            Arc::new(f.config.clone()),
            controls(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        if matches!(drift, "checksum" | "master") {
            assert_eq!(first.stats.downloaded, 0, "{first:?}");
            assert!(first.sync_token.is_none());
            assert_eq!(
                f.db.acquire_lock("resource drift cannot obtain active publication proof")
                    .unwrap()
                    .query_row::<i64, _, _>(
                        "SELECT count(*) FROM provider_active_destinations WHERE verified_media=1",
                        [],
                        |r| r.get(0)
                    )
                    .unwrap(),
                0
            );
            assert!(!f.config.directory.join("A/changed.JPG").exists());
            f.preserved().await;
            f.session.records.lock().unwrap()[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
                json!(data_encoding::BASE64.encode(&Sha256::digest(media)));
            *f.session.corrupt.lock().unwrap() = None;
            f = reopen_destination_fixture(f).await;
            let recovered = crate::download::download_photos_with_sync(
                &reqwest::Client::new(),
                std::slice::from_ref(&f.pass),
                Arc::new(f.config.clone()),
                controls(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
            assert_eq!(recovered.stats.downloaded, 1, "{recovered:?}");
        } else {
            assert_eq!(first.stats.downloaded, 1, "{first:?}");
        }
        let published = f.config.directory.join("A/changed.JPG");
        assert_eq!(std::fs::read(&published).unwrap(), media);
        #[cfg(feature = "xmp")]
        if drift == "metadata" {
            let packet =
                std::fs::read_to_string(published.with_file_name("changed.JPG.xmp")).unwrap();
            assert!(packet.contains("current"));
            assert!(
                !packet.contains("different current title"),
                "URL refresh cannot replace the selected task's frozen metadata"
            );
        }
        let checksum = f
            .db
            .acquire_lock("refreshed URL is transient, exact SHA is receipt")
            .unwrap()
            .query_row::<String, _, _>(
                "SELECT local_checksum FROM provider_active_destinations WHERE verified_media=1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            checksum,
            data_encoding::HEXLOWER.encode(&Sha256::digest(media))
        );
        let requests = server.received_requests().await.unwrap().len();
        let calls = f.session.calls.load(Ordering::SeqCst);
        let ranks = f.session.rank_calls.load(Ordering::SeqCst);
        let modified = std::fs::metadata(&published).unwrap().modified().unwrap();
        for _ in 0..2 {
            f = reopen_destination_fixture(f).await;
            let quiet = crate::download::download_photos_with_sync(
                &reqwest::Client::new(),
                std::slice::from_ref(&f.pass),
                Arc::new(f.config.clone()),
                controls(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
            assert_eq!(quiet.stats.downloaded, 0);
            assert_eq!(f.session.calls.load(Ordering::SeqCst), calls);
            assert_eq!(f.session.rank_calls.load(Ordering::SeqCst), ranks);
            assert_eq!(server.received_requests().await.unwrap().len(), requests);
            assert_eq!(
                std::fs::metadata(&published).unwrap().modified().unwrap(),
                modified
            );
            f.preserved().await;
        }
    }
}

#[tokio::test]
async fn private_historical_first_session_failure_preserves_original_intent_and_recovers_current_root()
 {
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    let media = b"\xff\xd8\xff\xe0\0\x10JFIF\0\x01\x01\0\0\x01\0\x01\0\0\xff\xd9";
    let fail = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let failure = fail.clone();
    Mock::given(method("GET"))
        .and(path("/historical-owned"))
        .respond_with(move |_: &wiremock::Request| {
            if failure.load(Ordering::SeqCst) {
                ResponseTemplate::new(500)
            } else {
                ResponseTemplate::new(200).set_body_bytes(media)
            }
        })
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/historical-owned"))
        .respond_with(ResponseTemplate::new(403))
        .expect(1)
        .mount(&server)
        .await;
    let mut f = Fixture::new().await;
    f.pass = destination_pass(&f, "A");
    f.config.retry.max_retries = 0;
    f.session.records.lock().unwrap()[0]["fields"]["resOriginalRes"]["value"] = json!({"downloadURL":format!("{}/historical-owned",server.uri()),"size":media.len(),"fileChecksum":data_encoding::BASE64.encode(&Sha256::digest(media))});
    let first = crate::download::download_photos_with_sync(
        &reqwest::Client::new(),
        std::slice::from_ref(&f.pass),
        Arc::new(f.config.clone()),
        controls(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(first.stats.downloaded, 0);
    assert_eq!(first.stats.failed, 1);
    assert!(first.sync_token.is_none());
    let old =
        f.db.acquire_lock("retain selected historical intent")
            .unwrap()
            .query_row::<(String, Vec<u8>), _, _>(
                "SELECT generation,manifest FROM provider_active_decisions",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
    f.capture(vec![json!({"recordName":"future-historical-session","recordType":"FutureOpaque","payload":[0,255,9]})],"historical-session-drift").await;
    *f.session.corrupt.lock().unwrap() = Some("http403");
    f = reopen_destination_fixture(f).await;
    let calls = f.session.calls.load(Ordering::SeqCst);
    let ranks = f.session.rank_calls.load(Ordering::SeqCst);
    let refused = crate::download::download_photos_with_sync(
        &reqwest::Client::new(),
        std::slice::from_ref(&f.pass),
        Arc::new(f.config.clone()),
        controls(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(
        matches!(
            refused.outcome,
            crate::download::DownloadOutcome::SessionExpired { .. }
        ),
        "{refused:?}"
    );
    assert_eq!(f.session.calls.load(Ordering::SeqCst), calls + 1);
    assert_eq!(
        f.session.rank_calls.load(Ordering::SeqCst),
        ranks,
        "historical-first auth must stop before fresh inventory"
    );
    assert_eq!(refused.stats.downloaded, 0);
    assert!(refused.sync_token.is_none());
    assert_eq!(f.count("provider_active_decisions"), 1);
    {
        let conn =
            f.db.acquire_lock("auth cannot rewrite historical proof as backoff")
                .unwrap();
        assert_eq!(conn.query_row::<(Vec<u8>,i64,i64),_,_>("SELECT manifest,attempts,next_retry_at FROM provider_active_decisions WHERE generation=?1",[&old.0],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap(),(old.1.clone(),0,0));
    }
    f.preserved().await;
    *f.session.corrupt.lock().unwrap() = None;
    fail.store(false, Ordering::SeqCst);
    f = reopen_destination_fixture(f).await;
    let recovered = crate::download::download_photos_with_sync(
        &reqwest::Client::new(),
        std::slice::from_ref(&f.pass),
        Arc::new(f.config.clone()),
        controls(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(recovered.stats.downloaded, 1, "{recovered:?}");
    assert_eq!(
        std::fs::read(f.config.directory.join("A/changed.JPG")).unwrap(),
        media
    );
    assert_eq!(f.db.acquire_lock("one physical publication completes compatible roots").unwrap().query_row::<i64,_,_>("SELECT count(*) FROM provider_active_destinations WHERE verified_media=1 AND verified_metadata=1",[],|r|r.get(0)).unwrap(),2);
    let calls = f.session.calls.load(Ordering::SeqCst);
    let ranks = f.session.rank_calls.load(Ordering::SeqCst);
    let requests = server.received_requests().await.unwrap().len();
    for _ in 0..2 {
        f = reopen_destination_fixture(f).await;
        let quiet = crate::download::download_photos_with_sync(
            &reqwest::Client::new(),
            std::slice::from_ref(&f.pass),
            Arc::new(f.config.clone()),
            controls(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(quiet.stats.downloaded, 0);
        assert_eq!(f.session.calls.load(Ordering::SeqCst), calls);
        assert_eq!(f.session.rank_calls.load(Ordering::SeqCst), ranks);
        assert_eq!(server.received_requests().await.unwrap().len(), requests);
        f.preserved().await;
    }
}

#[tokio::test]
async fn private_actual_lookup_session_errors_stop_the_current_producer_without_deferred_ack() {
    Box::pin(private_lookup_session_recovery(false)).await;
}

#[tokio::test]
async fn private_actual_lookup_session_errors_stop_album_b_and_unfiled_then_recover_all_destinations()
 {
    Box::pin(private_lookup_session_recovery(true)).await;
}

fn session_recovery_passes(f: &Fixture, multi: bool) -> Vec<AlbumPass> {
    let mut passes = vec![destination_pass(f, "A")];
    if multi {
        passes.push(destination_pass(f, "B"));
        let mut album = PhotoAlbum::new(
            PhotoAlbumConfig {
                params: Arc::new(std::collections::HashMap::new()),
                service_endpoint: Arc::from("https://example.invalid"),
                name: Arc::from(""),
                list_type: Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
                obj_type: Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
                query_filter: None,
                page_size: 2,
                zone_id: Arc::new(
                    json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner","zoneType":"REGULAR_CUSTOM_ZONE"}),
                ),
                retry_config: crate::retry::RetryConfig::default(),
                container_id: None,
                cross_zone_sources: Vec::new(),
            },
            Box::new(DestinationSelectionSession(f.session.clone())),
        );
        album.set_shadow_capture(f.capture.clone(), Arc::from("private"));
        passes.push(AlbumPass {
            kind: PassKind::Unfiled,
            album,
            exclude_ids: Arc::new(
                ["m", "asset-m", "c", "asset-c"]
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
            ),
        });
    }
    passes
}

async fn private_lookup_session_recovery(multi: bool) {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    for (status, fault) in [(401, "http401"), (403, "http403"), (421, "http421")] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/session-fault"))
            .respond_with(ResponseTemplate::new(status))
            .mount(&server)
            .await;
        let mut f = Fixture::new().await;
        let mut observed = records("m", CURRENT, "current");
        observed.extend(records("c", CURRENT, "second"));
        observed.extend(records("d", CURRENT, "third"));
        for pair in observed.chunks_mut(2) {
            pair[0]["fields"]["resOriginalRes"]["value"]["downloadURL"] =
                json!(format!("{}/session-fault", server.uri()));
        }
        *f.session.records.lock().unwrap() = observed;
        *f.session.corrupt.lock().unwrap() = Some(fault);
        f.pass = destination_pass_with(&f.session, &f.capture, "A");
        f.config.folder_structure = String::new();
        let passes = session_recovery_passes(&f, multi);
        let before = f.queue();
        let result = crate::download::download_photos_with_sync(
            &reqwest::Client::new(),
            &passes,
            Arc::new(f.config.clone()),
            controls(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(
            matches!(
                result.outcome,
                crate::download::DownloadOutcome::SessionExpired { .. }
            ),
            "status={status}: {result:?}"
        );
        assert_eq!(result.stats.downloaded, 0);
        assert_eq!(
            f.session.calls.load(Ordering::SeqCst),
            1,
            "status={status}: stop after the first canonical session failure"
        );
        assert_eq!(f.queue(), before);
        assert_eq!(
            f.count("provider_active_decisions"),
            0,
            "session recovery must not become non-session backoff"
        );
        assert_eq!(f.count("provider_active_destinations"), 0);
        assert_eq!(
            f.db.acquire_lock("auth cannot seal coverage")
                .unwrap()
                .query_row::<i64, _, _>(
                    "SELECT count(*) FROM provider_active_generations WHERE sealed=1",
                    [],
                    |r| r.get(0)
                )
                .unwrap(),
            0
        );
        assert!(result.checkpoint.sync_token_blocked);
        assert!(result.sync_token.is_none());
        f.preserved().await;
        // Remove only the session fault and supply current scoped resource facts.
        // Every child has a distinct source checksum/path, independently of rank.
        *f.session.corrupt.lock().unwrap() = None;
        let header = b"\xff\xd8\xff\xe0\0\x10JFIF\0\x01\x01\0\0\x01\0\x01\0\0\xff\xd9";
        let mut payloads = Vec::new();
        for (index, name) in ["m", "c", "d"].into_iter().enumerate() {
            use sha2::{Digest, Sha256};
            let mut bytes = header.to_vec();
            bytes.extend_from_slice(name.as_bytes());
            Mock::given(method("GET"))
                .and(path(format!("/recovered-{name}")))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.clone()))
                .expect(if multi {
                    if name == "d" { 3 } else { 2 }
                } else {
                    1
                })
                .mount(&server)
                .await;
            let mut records = f.session.records.lock().unwrap();
            records[index * 2]["fields"]["filenameEnc"] =
                json!({"value":format!("recovered-{name}.JPG"),"type":"STRING"});
            records[index * 2]["fields"]["resOriginalRes"]["value"] = json!({"downloadURL":format!("{}/recovered-{name}",server.uri()),"size":bytes.len(),"fileChecksum":data_encoding::BASE64.encode(&Sha256::digest(&bytes))});
            payloads.push((name, bytes));
        }
        let recovered = crate::download::download_photos_with_sync(
            &reqwest::Client::new(),
            &passes,
            Arc::new(f.config.clone()),
            controls(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            recovered.stats.downloaded,
            if multi { 7 } else { 3 },
            "status={status}: {recovered:?}"
        );
        for (name, bytes) in payloads {
            if multi {
                assert_eq!(
                    std::fs::read(f.config.directory.join(format!("B/recovered-{name}.JPG")))
                        .unwrap(),
                    bytes
                );
                if name == "d" {
                    assert_eq!(
                        std::fs::read(f.config.directory.join("recovered-d.JPG")).unwrap(),
                        bytes
                    );
                }
            }
            assert_eq!(
                std::fs::read(f.config.directory.join(format!("A/recovered-{name}.JPG"))).unwrap(),
                bytes
            );
        }
        assert_eq!(
            f.db.acquire_lock("actual recovered session publication")
                .unwrap()
                .query_row::<i64, _, _>(
                    "SELECT count(*) FROM provider_active_destinations WHERE verified_media=1",
                    [],
                    |r| r.get(0)
                )
                .unwrap(),
            if multi { 7 } else { 3 }
        );
        let calls = f.session.calls.load(Ordering::SeqCst);
        let ranks = f.session.rank_calls.load(Ordering::SeqCst);
        for _ in 0..2 {
            f = reopen_destination_fixture(f).await;
            let passes = session_recovery_passes(&f, multi);
            let quiet = crate::download::download_photos_with_sync(
                &reqwest::Client::new(),
                &passes,
                Arc::new(f.config.clone()),
                controls(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
            assert_eq!(quiet.stats.downloaded, 0);
            assert_eq!(f.session.calls.load(Ordering::SeqCst), calls);
            assert_eq!(f.session.rank_calls.load(Ordering::SeqCst), ranks);
            f.preserved().await;
        }
    }
}

#[tokio::test]
async fn private_capture_timestamp_repair_keeps_its_whole_existing_receipt_owner() {
    let mut f = Fixture::new().await;
    f.pass = destination_pass_with(&f.session, &f.capture, "A");
    f.config.refresh_metadata = true;
    assert!(
        crate::download::orchestration::generation::context(
            std::slice::from_ref(&f.pass),
            &f.config,
            controls()
        )
        .await
        .unwrap()
        .is_some()
    );
    f.config.capture_timestamp_repair =
        crate::download::metadata_rewrite::CaptureTimestampRepair::ReplaceWithCaptureLocal;
    assert!(
        crate::download::orchestration::generation::context(
            std::slice::from_ref(&f.pass),
            &f.config,
            controls()
        )
        .await
        .unwrap()
        .is_none()
    );
    assert_eq!(f.count("provider_active_generations"), 0);
    f.preserved().await;
}

#[tokio::test]
async fn private_non_session_http_refusal_retains_identity_backoff_without_hot_loop() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/malformed-proof"))
        .respond_with(ResponseTemplate::new(400))
        .expect(2)
        .mount(&server)
        .await;
    let mut f = Fixture::new().await;
    let mut observed = records("m", CURRENT, "current");
    observed.extend(records("c", CURRENT, "second"));
    for pair in observed.chunks_mut(2) {
        pair[0]["fields"]["resOriginalRes"]["value"]["downloadURL"] =
            json!(format!("{}/malformed-proof", server.uri()));
    }
    *f.session.records.lock().unwrap() = observed;
    *f.session.corrupt.lock().unwrap() = Some("http400");
    f.pass = destination_pass_with(&f.session, &f.capture, "A");
    let first = crate::download::download_photos_with_sync(
        &reqwest::Client::new(),
        std::slice::from_ref(&f.pass),
        Arc::new(f.config.clone()),
        controls(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(!matches!(
        first.outcome,
        crate::download::DownloadOutcome::SessionExpired { .. }
    ));
    assert_eq!(first.stats.downloaded, 0);
    assert!(first.checkpoint.sync_token_blocked);
    assert!(first.sync_token.is_none());
    assert_eq!(f.db.acquire_lock("non-session durable declarations").unwrap().query_row::<i64,_,_>("SELECT count(*) FROM provider_active_decisions WHERE admission='deferred' AND attempts=1 AND next_retry_at>strftime('%s','now')",[],|r|r.get(0)).unwrap(),2);
    let calls = f.session.calls.load(Ordering::SeqCst);
    let ranks = f.session.rank_calls.load(Ordering::SeqCst);
    for _ in 0..2 {
        f = reopen_destination_fixture(f).await;
        let quiet = crate::download::download_photos_with_sync(
            &reqwest::Client::new(),
            std::slice::from_ref(&f.pass),
            Arc::new(f.config.clone()),
            controls(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(quiet.stats.downloaded, 0);
        assert!(quiet.checkpoint.sync_token_blocked);
        assert_eq!(f.session.calls.load(Ordering::SeqCst), calls);
        assert_eq!(f.session.rank_calls.load(Ordering::SeqCst), ranks);
        f.preserved().await;
    }
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn private_embedded_metadata_state_fault_recovers_exact_prepared_output_after_reopen() {
    Box::pin(private_metadata_recovery_fixture(
        true,
        false,
        false,
        false,
        MetadataStateFault::AfterWrite,
    ))
    .await;
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn private_prepared_embed_source_drift_reuses_exact_utf8_publication_without_copy() {
    Box::pin(private_metadata_recovery_fixture(
        true,
        false,
        false,
        false,
        MetadataStateFault::AfterWriteAndSourceDrift,
    ))
    .await;
}

#[cfg(all(target_os = "linux", feature = "xmp"))]
#[tokio::test]
async fn private_prepared_native_source_drift_preserves_lossy_utf8_sibling_and_legacy_debt() {
    Box::pin(private_metadata_recovery_fixture(
        true,
        false,
        false,
        true,
        MetadataStateFault::AfterWriteAndSourceDrift,
    ))
    .await;
}

#[cfg(target_os = "linux")]
mod late_io;
