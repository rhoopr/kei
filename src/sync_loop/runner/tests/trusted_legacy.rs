//! Trusted Legacy File migration through the real cycle owner. Synthetic accounts and bytes only.
#![cfg(target_os = "linux")]

use crate::icloud::photos::{PhotoAsset, PhotosSession};
use crate::state::VersionSizeKey;
use crate::sync_cycle::run_cycle;
use crate::sync_loop::test_support::{
    RunCycleDownloadConfigOptions, album_count_response, full_album_page_with_download,
    make_full_album_with_boxed_session, make_run_cycle_config,
    make_run_cycle_download_config_builder_with_options, make_run_cycle_library_state_with_album,
    make_shared_session_for_run_cycle,
};
use crate::{download, state};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

const ZONE: &str = "SharedSync-SYNTHETIC-853";
const TOKEN: &str = "sync_token:SharedSync-SYNTHETIC-853";
const USER: &str = "receipt-recovery@example.invalid";

#[derive(Clone, Debug)]
struct Provider {
    records: Vec<Value>,
    zone: String,
    bridge_missing: bool,
}
#[async_trait::async_trait]
impl PhotosSession for Provider {
    async fn post(&self, url: &str, body: String, _: &[(&str, &str)]) -> anyhow::Result<Value> {
        let request: Value = serde_json::from_str(&body)?;
        if url.contains("/records/query/batch?") {
            return Ok(album_count_response(
                self.records
                    .iter()
                    .filter(|r| r["recordType"] == "CPLAsset")
                    .count() as u64,
            ));
        }
        if url.contains("/records/query?") {
            let offset = request["query"]["filterBy"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|v| v["fieldName"] == "startRank")
                .and_then(|v| v["fieldValue"]["value"].as_u64())
                .unwrap_or(0);
            return Ok(
                json!({"records":if offset==0 {self.records.clone()} else {Vec::new()}, "syncToken":"inventory-after"}),
            );
        }
        if url.contains("/records/lookup?") {
            let names = request["records"].as_array().unwrap();
            let records: Vec<_> = self
                .records
                .iter()
                .filter(|r| names.iter().any(|n| n["recordName"] == r["recordName"]))
                .collect();
            return Ok(json!({"records":records}));
        }
        assert!(
            url.contains("/changes/zone?"),
            "unexpected synthetic provider endpoint: {url}"
        );
        let delta = request["zones"][0]["syncToken"].is_string();
        let mut response = json!({"zones":[{"zoneID":{"zoneName":self.zone,"ownerRecordName":"_defaultOwner"},
            "syncToken":if delta {"delta-after"} else {"inventory-after"}, "moreComing":false,
            "records":if delta {Vec::new()} else {self.records.clone()}}]});
        if delta && self.bridge_missing {
            response["zones"][0]["syncToken"] = json!("");
        }
        Ok(response)
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

#[derive(Clone)]
struct OldFile {
    path: PathBuf,
    bytes: Vec<u8>,
    dev: u64,
    ino: u64,
    links: u64,
    mtime: (i64, i64),
}
impl OldFile {
    fn capture(path: PathBuf) -> Self {
        let m = std::fs::metadata(&path).unwrap();
        Self {
            bytes: std::fs::read(&path).unwrap(),
            path,
            dev: m.dev(),
            ino: m.ino(),
            links: m.nlink(),
            mtime: (m.mtime(), m.mtime_nsec()),
        }
    }
    fn verify(&self) {
        let path = &self.path;
        let now = Self::capture(path.clone());
        assert_eq!(now.bytes, self.bytes, "old bytes: {}", path.display());
        assert_eq!(
            (now.dev, now.ino, now.links, now.mtime),
            (self.dev, self.ino, self.links, self.mtime),
            "old inode/aliases/timestamp: {}",
            path.display()
        );
    }
}
struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    media: PathBuf,
    database: PathBuf,
    provider: Provider,
    targets: Vec<(String, VersionSizeKey)>,
    old: Vec<OldFile>,
    moved: Vec<(PathBuf, PathBuf)>,
    originals: Value,
    globals: crate::config::GlobalArgs,
    server: Arc<MockServer>,
    body: Vec<u8>,
}
fn rows(path: &Path, sql: &str) -> Value {
    let conn =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let mut stmt = conn.prepare(sql).unwrap();
    let columns = stmt.column_count();
    let result: Vec<Vec<String>> = stmt
        .query_map([], |row| {
            (0..columns)
                .map(|n| {
                    row.get::<_, rusqlite::types::Value>(n)
                        .map(|v| format!("{v:?}"))
                })
                .collect()
        })
        .unwrap()
        .map(Result::unwrap)
        .collect();
    json!(result)
}
const ORIGINAL_SQL: &str = "SELECT * FROM assets WHERE id LIKE 'family-%' ORDER BY id,version_size";
impl Fixture {
    async fn new(name: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join(name);
        assert!(
            !root.exists(),
            "never overwrite an existing qualification case"
        );
        std::fs::create_dir_all(&root).unwrap();
        let media = root.join("media");
        std::fs::create_dir_all(&media).unwrap();
        let globals = crate::config::GlobalArgs {
            username: Some(USER.into()),
            domain: None,
            data_dir: Some(root.join("state").display().to_string()),
        };
        let database = crate::get_db_path(&globals, None).await.unwrap();
        assert!(database.starts_with(&root));
        let db = state::SqliteStateDb::open(&database).await.unwrap();
        {
            let conn = db.acquire_lock("synthetic ownership fixture").unwrap();
            conn.execute_batch("CREATE TABLE account_owner(singleton INTEGER PRIMARY KEY CHECK(singleton=1),version INTEGER NOT NULL,account_key TEXT NOT NULL,provider_key TEXT NOT NULL)").unwrap();
            conn.execute(
                "INSERT INTO account_owner VALUES(1,1,?1,?2)",
                (
                    crate::account::namespace(USER, "com"),
                    crate::account::provider_fingerprint("com", "synthetic-only"),
                ),
            )
            .unwrap();
        }
        let server = Arc::new(MockServer::start().await);
        let body = include_bytes!("../../../../tests/data/media/pattern.jpg").to_vec();
        let mut records = Vec::new();
        let mut targets = Vec::new();
        let mut old = Vec::new();
        let mut config = download::DownloadConfig::test_default();
        config.directory = Arc::from(media.as_path());
        config.folder_structure = "%Y/%m/%d".into();
        config.edited = true;
        config.library = ZONE.into();
        for family in 0..2 {
            let master = format!("family-{family}");
            let checksum = if family == 0 {
                "cnJycnJycnJycnJycnJycnJycnJy"
            } else {
                "c3Nzc3Nzc3Nzc3Nzc3Nzc3Nzc3Nz"
            };
            let page = full_album_page_with_download(
                ZONE,
                &master,
                "after",
                &format!("{}/media.jpg", server.uri()),
                body.len() as u64,
                checksum,
            );
            let mut master_record = page["records"][0].clone();
            master_record["fields"]["filenameEnc"] =
                json!({"value":format!("family-{family}.jpg"),"type":"STRING"});
            records.push(master_record.clone());
            let mut assets = Vec::new();
            for child in 0..2 {
                let id = format!("child-{family}-{child}");
                let mut record = page["records"][1].clone();
                record["recordName"] = json!(id);
                record["fields"]["isHidden"] = json!({"value":1});
                record["fields"]["addedDate"]["value"] = json!(if child == 0 {
                    1_515_000_000_000_i64
                } else {
                    1_420_000_000_000_i64
                });
                if child == 1 {
                    record["fields"]["resJPEGFullRes"] = json!({"value":{"downloadURL":format!("{}/edited.jpg",server.uri()),"size":body.len(),"fileChecksum":"dHR0dHR0dHR0dHR0dHR0dHR0dHR0"}});
                    record["fields"]["resJPEGFullFileType"] = json!({"value":"public.jpeg"});
                    record["fields"]["adjustmentRenderType"] = json!({"value":1,"type":"INT64"});
                }
                let asset = PhotoAsset::new(master_record.clone(), record.clone())
                    .with_state_record_name(id.clone().into());
                db.upsert_asset_master_mapping(ZONE, &id, &master)
                    .await
                    .unwrap();
                for expected in download::filter::expected_paths_for(&asset, &config) {
                    let file = expected.path.with_file_name(
                        download::paths::insert_asset_identity_suffix(
                            expected.path.file_name().unwrap().to_str().unwrap(),
                            &id,
                        ),
                    );
                    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
                    std::fs::write(&file, &body).unwrap();
                    let row = crate::test_helpers::TestAssetRecord::new(&id)
                        .library(ZONE)
                        .version_size(expected.version_size)
                        .filename(file.file_name().unwrap().to_str().unwrap())
                        .checksum(&expected.checksum)
                        .size(body.len() as u64)
                        .created_at(asset.asset_date())
                        .added_at(asset.added_date())
                        .metadata((*asset.metadata_arc(expected.version_size)).clone())
                        .build();
                    db.upsert_seen(&row).await.unwrap();
                    let local = data_encoding::HEXLOWER.encode(&Sha256::digest(&body));
                    db.mark_downloaded(
                        ZONE,
                        &id,
                        expected.version_size.as_str(),
                        &file,
                        &local,
                        Some(&local),
                    )
                    .await
                    .unwrap();
                    for alias in 0..2 {
                        let p = root.join("aliases").join(format!(
                            "{id}-{}-{alias}.jpg",
                            expected.version_size.as_str()
                        ));
                        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                        std::fs::hard_link(&file, &p).unwrap();
                        old.push(OldFile::capture(p));
                    }
                    old.push(OldFile::capture(file));
                    targets.push((id.clone(), expected.version_size));
                }
                assets.push(asset);
                records.push(record);
            }
            for (version, asset) in [
                (VersionSizeKey::Original, &assets[0]),
                (VersionSizeKey::Adjusted, &assets[1]),
            ] {
                let file = media
                    .join("legacy")
                    .join(format!("{master}-{}.jpg", version.as_str()));
                std::fs::create_dir_all(file.parent().unwrap()).unwrap();
                std::fs::write(&file, &body).unwrap();
                let checksum = asset
                    .rendition_facts()
                    .iter()
                    .find(|(key, _)| *key == version)
                    .unwrap()
                    .1
                    .checksum
                    .as_deref()
                    .unwrap();
                let row = crate::test_helpers::TestAssetRecord::new(&master)
                    .library(ZONE)
                    .version_size(version)
                    .filename(file.file_name().unwrap().to_str().unwrap())
                    .checksum(checksum)
                    .size(body.len() as u64)
                    .created_at(asset.asset_date())
                    .added_at(asset.added_date())
                    .metadata((*asset.metadata_arc(version)).clone())
                    .build();
                db.upsert_seen(&row).await.unwrap();
                db.mark_downloaded(
                    ZONE,
                    &master,
                    version.as_str(),
                    &file,
                    &data_encoding::HEXLOWER.encode(&Sha256::digest(&body)),
                    None,
                )
                .await
                .unwrap();
                for alias in 0..2 {
                    let p = root
                        .join("aliases")
                        .join(format!("{master}-{}-{alias}.jpg", version.as_str()));
                    std::fs::hard_link(&file, &p).unwrap();
                    old.push(OldFile::capture(p));
                }
                old.push(OldFile::capture(file));
            }
            db.set_metadata_capture_revision_for_test(ZONE, &master, 0);
        }
        assert_eq!(targets.len(), 6, "exact six-rendition shape");
        db.set_metadata(TOKEN, "before").await.unwrap();
        db.begin_metadata_capture_revision(ZONE, state::METADATA_CAPTURE_REVISION)
            .await
            .unwrap();
        for candidate in db
            .get_metadata_capture_candidates(ZONE, 1, 10)
            .await
            .unwrap()
        {
            assert!(candidate.asset_id.starts_with("family-"));
            db.defer_metadata_capture_ambiguity(&candidate, 1)
                .await
                .unwrap();
        }
        {
            let conn = db.acquire_lock("synthetic pre-receipt chronology").unwrap();
            conn.execute(
                "UPDATE assets SET downloaded_at=1782172800 WHERE id LIKE 'child-%'",
                [],
            )
            .unwrap();
        }
        drop(db);
        let originals = rows(&database, ORIGINAL_SQL);
        let old = old
            .into_iter()
            .map(|entry| OldFile::capture(entry.path))
            .collect();
        let fixture = Self {
            _temp: temp,
            root,
            media,
            database,
            provider: Provider {
                records,
                zone: ZONE.into(),
                bridge_missing: false,
            },
            targets,
            old,
            moved: Vec::new(),
            originals,
            globals,
            server,
            body,
        };
        fixture.responses(200, false).await;
        fixture.evidence("seed",json!({"historical_source_receipts":false,"ready_children":4,"renditions":6,"mixed_owner_families":2}));
        fixture
    }
    async fn responses(&self, status: u16, delay: bool) {
        self.server.reset().await;
        let mut response = ResponseTemplate::new(status)
            .set_body_bytes(self.body.clone())
            .insert_header("content-type", "image/jpeg");
        if delay {
            response = response.set_delay(std::time::Duration::from_millis(400));
        }
        Mock::given(method("GET"))
            .respond_with(response)
            .mount(self.server.as_ref())
            .await;
    }
    fn verify_old(&self) {
        for file in &self.old {
            file.verify();
        }
        assert_eq!(
            rows(&self.database, ORIGINAL_SQL),
            self.originals,
            "legacy rows/dates/checksums/owners unchanged"
        );
        assert_eq!(
            rows(&self.database, "SELECT * FROM legacy_master_state_owners"),
            json!([])
        );
    }
    fn evidence(&self, stage: &str, details: Value) {
        self.verify_old();
        let state = rows(
            &self.database,
            "SELECT a.library,a.id,a.version_size,a.status,a.local_path,p.provider_checksum,p.local_checksum,p.source_checksum FROM assets a JOIN asset_metadata_paths p USING(library,id,version_size) WHERE a.id LIKE 'child-%' AND a.local_path=p.local_path ORDER BY a.id,a.version_size,p.local_path",
        );
        std::fs::write(self.root.join(format!("{stage}.json")),serde_json::to_vec_pretty(&json!({"stage":stage,"details":details,"child_receipts":state,"retained_files":self.old.len(),"renamed_entries":self.moved})).unwrap()).unwrap();
    }
    async fn source_counts(&self) -> (usize, usize) {
        let db = state::SqliteStateDb::open(&self.database).await.unwrap();
        let valid: i64 = {
            let conn = db.acquire_lock("source count").unwrap();
            conn.query_row(
                "SELECT count(*) FROM asset_metadata_paths WHERE source_checksum IS NOT NULL",
                [],
                |r| r.get(0),
            )
            .unwrap()
        };
        let mut eligible = 0;
        for (id, version) in &self.targets {
            eligible += usize::from(
                db.legacy_child_receipts(ZONE, id)
                    .await
                    .unwrap()
                    .iter()
                    .any(|r| r.version_size == *version),
            );
        }
        (usize::try_from(valid).unwrap(), eligible)
    }
    async fn plan(&self) -> download::trusted_legacy::TrustedLegacyPlan {
        let db = state::SqliteStateDb::open_owned_read_only(
            &self.database,
            &crate::get_account_owner(&self.globals, None),
        )
        .await
        .unwrap();
        download::trusted_legacy::plan(
            &db,
            ZONE,
            &self.media,
            ".part",
            true,
            &CancellationToken::new(),
        )
        .await
        .unwrap()
    }
    async fn apply(
        &self,
        plan: download::trusted_legacy::TrustedLegacyPlan,
        cancel: &CancellationToken,
    ) -> anyhow::Result<usize> {
        let db = state::SqliteStateDb::open_owned(
            &self.database,
            &crate::get_account_owner(&self.globals, None),
        )
        .await?;
        download::trusted_legacy::apply(&db, plan, &self.media, ".part", true, cancel).await
    }
    async fn cycle(
        &self,
        label: &str,
        token: CancellationToken,
        primary_hold: bool,
    ) -> anyhow::Result<crate::sync_cycle::CycleResult> {
        let db = Arc::new(state::SqliteStateDb::open(&self.database).await.unwrap());
        let mut config = make_run_cycle_config();
        config.photos.edited = true;
        config.download.legacy_preservation_allow_hardlinks = true;
        let provider = self.provider.clone();
        let mut shared = make_run_cycle_library_state_with_album(
            ZONE,
            TOKEN,
            make_full_album_with_boxed_session(ZONE, Box::new(provider.clone())),
        );
        shared.library =
            crate::icloud::photos::PhotoLibrary::new_stub_with_zone(Box::new(provider), ZONE);
        let primary_provider = Provider {
            records: Vec::new(),
            zone: "PrimarySync".into(),
            bridge_missing: primary_hold,
        };
        let mut primary = make_run_cycle_library_state_with_album(
            "PrimarySync",
            "sync_token:PrimarySync",
            make_full_album_with_boxed_session("PrimarySync", Box::new(primary_provider.clone())),
        );
        primary.library = crate::icloud::photos::PhotoLibrary::new_stub_with_zone(
            Box::new(primary_provider),
            "PrimarySync",
        );
        if primary_hold {
            db.set_metadata(crate::sync_cycle::ENUM_CONFIG_HASH_KEY, "old-enum-hash")
                .await
                .unwrap();
            db.set_metadata("sync_token:PrimarySync", "primary-before")
                .await
                .unwrap();
        }
        let mut libraries = vec![&shared];
        if primary_hold {
            libraries.push(&primary);
        }
        let store = db.clone() as Arc<dyn download::DownloadStore>;
        let base = make_run_cycle_download_config_builder_with_options(
            &self.media,
            store.clone(),
            RunCycleDownloadConfigOptions {
                concurrent_downloads: Some(1),
                ..Default::default()
            },
        );
        let builder = |mode, excluded, groups, library| {
            let mut c = base(mode, excluded, groups, library);
            let c_mut = Arc::make_mut(&mut c);
            c_mut.edited = true;
            c_mut.legacy_preservation_allow_hardlinks = true;
            c_mut.retry.max_retries = 0;
            c
        };
        let (_session_dir, session) = make_shared_session_for_run_cycle().await;
        use tracing::instrument::WithSubscriber as _;
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_max_level(tracing::Level::WARN)
            .with_writer(std::sync::Mutex::new(
                std::fs::File::create(self.root.join(format!("{label}.log"))).unwrap(),
            ))
            .finish();
        let result = Box::pin(
            run_cycle(
                &libraries,
                &config,
                Some(store.as_ref()),
                false,
                &builder,
                download::DownloadControls::download_hidden(),
                &session,
                &token,
            )
            .with_subscriber(subscriber),
        )
        .await;
        let summary=result.as_ref().map(|r|json!({"downloaded":r.stats.downloaded,"failed":r.stats.failed,"failed_count":r.failed_count,"interrupted":r.stats.interrupted,"checkpoint_blocked":r.stats.sync_token_blocked,"checkpoint_reason":r.stats.sync_token_blocked_reason,"state_write_failures":r.stats.state_write_failures,"database_checkpoint_safe":r.can_advance_database_checkpoint()})).unwrap_or_else(|e|json!({"error":format!("{e:#}")}));
        drop(base);
        drop(store);
        drop(db);
        self.evidence(label, summary);
        result
    }
}

#[tokio::test]
async fn trusted_legacy_files_zero_get_reopen_and_quiet_followups() {
    let f = Fixture::new("accepted").await;
    assert_eq!(f.source_counts().await, (0, 0));
    let baseline = f
        .cycle("baseline", CancellationToken::new(), false)
        .await
        .unwrap();
    assert!(baseline.stats.sync_token_blocked);
    let plan = f.plan().await;
    assert_eq!(plan.targets.len(), 6);
    assert_eq!(
        f.apply(plan.clone(), &CancellationToken::new())
            .await
            .unwrap(),
        6
    );
    assert_eq!(
        f.apply(plan, &CancellationToken::new()).await.unwrap(),
        6,
        "idempotent after reopen"
    );
    assert_eq!(
        f.source_counts().await,
        (0, 6),
        "local acceptance never fabricates source receipts"
    );
    for label in ["accepted-cycle", "quiet-one", "quiet-two"] {
        let result = f
            .cycle(label, CancellationToken::new(), false)
            .await
            .unwrap();
        assert_eq!(result.stats.downloaded, 0);
        assert!(!result.stats.sync_token_blocked, "{label}");
        assert!(result.can_advance_database_checkpoint());
        assert_eq!(f.source_counts().await, (0, 6));
    }
    assert_eq!(f.server.received_requests().await.unwrap().len(), 0);
    f.verify_old();
    let proof = rows(
        &f.database,
        "SELECT provider_evidence FROM unattributed_legacy_proofs ORDER BY asset_id,generation",
    );
    assert!(proof.to_string().contains("trusted_legacy_file"));
}

#[tokio::test]
async fn trusted_legacy_files_stale_cancelled_and_failed_plans_are_atomic() {
    for scenario in [
        "account",
        "library",
        "child",
        "master",
        "rendition",
        "path",
        "provider",
        "bytes",
        "temporary",
        "symlink",
        "cancel",
        "sql",
        "duplicate",
        "empty",
    ] {
        let f = Fixture::new(scenario).await;
        f.cycle("baseline", CancellationToken::new(), false)
            .await
            .unwrap();
        let mut plan = f.plan().await;
        let token = CancellationToken::new();
        let conn = rusqlite::Connection::open(&f.database).unwrap();
        match scenario {
            "account"=> {conn.execute("UPDATE account_owner SET provider_key=?1",["a".repeat(64)]).unwrap();}
            "library"=>plan.targets.last_mut().unwrap().library="OtherLibrary".into(),
            "child"=>plan.targets.last_mut().unwrap().child="other-child".into(),
            "master"=>plan.targets.last_mut().unwrap().master="other-master".into(),
            "rendition"=>plan.targets.last_mut().unwrap().rendition="other".into(),
            "path"=>plan.targets.last_mut().unwrap().path=f.media.join("other.jpg"),
            "provider"=>{conn.execute("UPDATE assets SET checksum='other-generation' WHERE id='child-1-1'",[]).unwrap();}
            "bytes"=>{std::fs::write(&plan.targets.last().unwrap().path,b"changed").unwrap();}
            "temporary" => {
                let mut part = plan.targets.last().unwrap().path.as_os_str().to_owned();
                part.push(".part");
                std::fs::write(Path::new(&part), b"pending").unwrap();
            }
            "symlink" => {
                let target = &plan.targets.last().unwrap().path;
                std::fs::remove_file(target).unwrap();
                std::os::unix::fs::symlink(&f.old.first().unwrap().path, target).unwrap();
            }
            "cancel" => token.cancel(),
            "sql"=>conn.execute_batch("CREATE TRIGGER refuse_trust BEFORE INSERT ON metadata WHEN NEW.key LIKE 'trusted_legacy_file:%' AND NEW.value LIKE '%child-1-1%' BEGIN SELECT RAISE(ABORT,'synthetic receipt failure'); END;").unwrap(),
            "duplicate"=>plan.targets.push(plan.targets.first().unwrap().clone()),
            "empty"=>plan.targets.clear(),
            _=>panic!("unexpected synthetic scenario: {scenario}"),
        }
        drop(conn);
        assert!(f.apply(plan, &token).await.is_err(), "{scenario}");
        assert_eq!(
            rows(
                &f.database,
                "SELECT key,value FROM metadata WHERE key LIKE 'trusted_legacy_file:%'"
            ),
            json!([]),
            "no partial acceptance: {scenario}"
        );
        assert_eq!(f.server.received_requests().await.unwrap().len(), 0);
    }
}

#[tokio::test]
async fn trusted_legacy_files_readiness_and_primary_bridge_still_hold() {
    for scenario in ["readiness", "primary"] {
        let f = Fixture::new(scenario).await;
        f.cycle("baseline", CancellationToken::new(), false)
            .await
            .unwrap();
        f.apply(f.plan().await, &CancellationToken::new())
            .await
            .unwrap();
        if scenario == "readiness" {
            let conn = rusqlite::Connection::open(&f.database).unwrap();
            conn.execute(
                "UPDATE assets SET metadata_write_failed_at=1 WHERE id='child-1-1'",
                [],
            )
            .unwrap();
            let db = state::SqliteStateDb::open(&f.database).await.unwrap();
            assert!(
                db.legacy_child_receipts(ZONE, "child-1-1")
                    .await
                    .unwrap()
                    .is_empty()
            );
        } else {
            let result = f
                .cycle("primary-hold", CancellationToken::new(), true)
                .await
                .unwrap();
            assert!(result.stats.sync_token_blocked);
            assert!(!result.can_advance_database_checkpoint());
        }
        assert_eq!(f.server.received_requests().await.unwrap().len(), 0);
        f.verify_old();
    }
}

#[tokio::test]
async fn trusted_legacy_files_acceptance_invalidates_after_binding_changes() {
    for (name, sql) in [
        (
            "provider",
            "UPDATE assets SET checksum='new-generation' WHERE id='child-1-1'",
        ),
        (
            "path",
            "UPDATE assets SET local_path=local_path||'.other' WHERE id='child-1-1'",
        ),
        (
            "local",
            "UPDATE assets SET local_checksum='other-sha' WHERE id='child-1-1'",
        ),
        (
            "metadata",
            "UPDATE assets SET metadata_hash='other-metadata' WHERE id='child-1-1'",
        ),
        (
            "date",
            "UPDATE assets SET added_at=added_at+1 WHERE id='child-1-1'",
        ),
        (
            "revision",
            "UPDATE asset_metadata_capture_revisions SET revision=revision+1 WHERE asset_id='child-1-1'",
        ),
        (
            "mapping",
            "UPDATE asset_master_mappings SET master_record_name='other-family' WHERE asset_record_name='child-1-1'",
        ),
    ] {
        let f = Fixture::new(name).await;
        f.cycle("baseline", CancellationToken::new(), false)
            .await
            .unwrap();
        f.apply(f.plan().await, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(f.source_counts().await, (0, 6));
        // Persisted coverage is established before the dependency changes.
        assert!(
            f.cycle("accepted", CancellationToken::new(), false)
                .await
                .unwrap()
                .can_advance_database_checkpoint()
        );
        let conn = rusqlite::Connection::open(&f.database).unwrap();
        conn.execute(sql, []).unwrap();
        drop(conn);
        let db = state::SqliteStateDb::open(&f.database).await.unwrap();
        assert!(
            db.legacy_child_receipts(ZONE, "child-1-1")
                .await
                .unwrap()
                .is_empty(),
            "{name}"
        );
        let status = db.get_summary().await.unwrap().metadata_capture;
        assert!(
            status
                .iter()
                .any(|s| s.library == ZONE && s.unattributed_legacy_pending > 0),
            "changed binding invalidates checkpoint coverage: {name}"
        );
        assert_eq!(db.stored_trusted_legacy_file_count().await.unwrap(), 6);
        assert_eq!(f.server.received_requests().await.unwrap().len(), 0);
    }
}

#[tokio::test]
async fn trusted_legacy_files_commit_failure_and_receipt_immutability() {
    let f = Fixture::new("commit").await;
    f.cycle("baseline", CancellationToken::new(), false)
        .await
        .unwrap();
    let plan = f.plan().await;
    let db = state::SqliteStateDb::open(&f.database).await.unwrap();
    let cancel = CancellationToken::new();
    let cancelled = cancel.clone();
    assert!(
        db.apply_trusted_legacy_files(plan.targets.clone(), move || {
            cancelled.cancel();
            Err(state::error::StateError::Invariant {
                operation: "synthetic_final_validation",
                detail: "cancelled after insertion".into(),
            })
        })
        .await
        .is_err()
    );
    assert!(cancel.is_cancelled());
    assert_eq!(
        db.stored_trusted_legacy_file_count().await.unwrap(),
        0,
        "final validation rolls back every insertion"
    );
    drop(db);
    f.apply(plan, &CancellationToken::new()).await.unwrap();
    let conn = rusqlite::Connection::open(&f.database).unwrap();
    for sql in [
        "UPDATE metadata SET value='changed' WHERE key LIKE 'trusted_legacy_file:%'",
        "DELETE FROM metadata WHERE key LIKE 'trusted_legacy_file:%'",
        "UPDATE metadata SET key='renamed' WHERE key LIKE 'trusted_legacy_file:%'",
        "INSERT OR REPLACE INTO metadata(key,value) SELECT key,'changed' FROM metadata WHERE key LIKE 'trusted_legacy_file:%' LIMIT 1",
    ] {
        assert!(conn.execute(sql, []).is_err(), "{sql}");
    }
    assert_eq!(f.source_counts().await, (0, 6));
    f.verify_old();
}

#[tokio::test]
async fn trusted_legacy_files_offline_command_and_account_lock() {
    use clap::Parser;
    crate::cli::Cli::try_parse_from([
        "kei",
        "reconcile",
        "trust-legacy-files",
        "plan",
        "--library",
        ZONE,
        "--output",
        "plan.json",
    ])
    .unwrap();
    crate::cli::Cli::try_parse_from([
        "kei",
        "reconcile",
        "trust-legacy-files",
        "apply",
        "--plan",
        "plan.json",
    ])
    .unwrap();
    let f = Fixture::new("command").await;
    f.cycle("baseline", CancellationToken::new(), false)
        .await
        .unwrap();
    let toml: crate::config::TomlConfig = toml::from_str(&format!(
        "[download]\ndirectory = {:?}\nlegacy_preservation_allow_hardlinks = true\n",
        f.media.display().to_string()
    ))
    .unwrap();
    let db = state::SqliteStateDb::open_owned_read_only(
        &f.database,
        &crate::get_account_owner(&f.globals, None),
    )
    .await
    .unwrap();
    assert!(
        download::trusted_legacy::plan(
            &db,
            ZONE,
            &f.media,
            ".part",
            false,
            &CancellationToken::new()
        )
        .await
        .is_err(),
        "hardlink trust remains default off"
    );
    drop(db);
    let output = f.root.join("plan.json");
    let args = crate::cli::TrustedLegacyArgs {
        action: crate::cli::TrustedLegacyAction::Plan {
            library: ZONE.into(),
            output: output.clone(),
        },
    };
    run_trust_command(args.clone(), &f.globals, Some(&toml))
        .await
        .unwrap();
    assert_eq!(f.source_counts().await, (0, 0), "plan cannot accept files");
    assert!(
        run_trust_command(args, &f.globals, Some(&toml))
            .await
            .is_err(),
        "no output overwrite"
    );
    let config = crate::config::Config::build(
        &f.globals,
        &crate::cli::PasswordArgs::default(),
        crate::cli::SyncArgs::default(),
        Some(&toml),
    )
    .unwrap();
    let guard = crate::auth::session::SessionStateGuard::acquire(
        &config.auth.cookie_directory,
        &config.auth.username,
        config.auth.domain.as_str(),
    )
    .await
    .unwrap();
    let apply = crate::cli::TrustedLegacyArgs {
        action: crate::cli::TrustedLegacyAction::Apply { plan: output },
    };
    assert!(
        run_trust_command(apply.clone(), &f.globals, Some(&toml))
            .await
            .is_err(),
        "sync account lock excludes offline apply"
    );
    drop(guard);
    run_trust_command(apply, &f.globals, Some(&toml))
        .await
        .unwrap();
    assert_eq!(f.source_counts().await, (0, 6));
    assert_eq!(f.server.received_requests().await.unwrap().len(), 0);
    f.verify_old();
}

#[tokio::test]
async fn trusted_legacy_files_unaccepted_rendition_remains_held() {
    let f = Fixture::new("partial-reviewed-cohort").await;
    f.cycle("baseline", CancellationToken::new(), false)
        .await
        .unwrap();
    let mut plan = f.plan().await;
    let unaccepted = plan.targets.pop().unwrap();
    assert_eq!(f.apply(plan, &CancellationToken::new()).await.unwrap(), 5);
    assert_eq!(f.source_counts().await, (0, 5));
    let result = f
        .cycle("unaccepted-control", CancellationToken::new(), false)
        .await
        .unwrap();
    assert!(result.stats.sync_token_blocked);
    assert!(!result.can_advance_database_checkpoint());
    let db = state::SqliteStateDb::open(&f.database).await.unwrap();
    assert!(
        db.legacy_child_provenance(ZONE, &unaccepted.child, &unaccepted.rendition)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(f.source_counts().await, (0, 5));
    assert_eq!(f.server.received_requests().await.unwrap().len(), 0);
    f.verify_old();
}

async fn run_trust_command(
    args: crate::cli::TrustedLegacyArgs,
    globals: &crate::config::GlobalArgs,
    toml: Option<&crate::config::TomlConfig>,
) -> anyhow::Result<()> {
    crate::commands::run_reconcile(
        crate::cli::ReconcileArgs {
            dry_run: false,
            action: Some(crate::cli::ReconcileAction::TrustLegacyFiles(args)),
        },
        globals,
        toml,
    )
    .await
}
