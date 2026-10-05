use super::{admit_retained_work, config_hash};
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
        anyhow::ensure!(
            url.contains("/records/lookup?"),
            "unexpected enumeration in catalog work"
        );
        self.calls.fetch_add(1, Ordering::SeqCst);
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
        let (_, scope, zone) = self.pass.album.catalog_work_scope().unwrap().unwrap();
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
                    31
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
