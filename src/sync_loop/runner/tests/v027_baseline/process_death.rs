//! SIGKILL at v27 publication boundaries; not a power-loss simulation.
use super::{
    BOUNDARY, BaselineSession, MEDIA, OLD_TOKEN, ZONE, baseline_owner, generation, historical_rows,
    pair, zone,
};
use crate::commands::{AlbumPass, PassKind};
use crate::download;
use crate::icloud::photos::{PhotoLibrary, PhotosSession};
use crate::state::DownloadStateStore;
use crate::state::SqliteStateDb;
use crate::state::db::provider_selection::SelectionPath;
use crate::sync_cycle::{run_cycle, sync_token_key};
use crate::sync_loop::test_support::{
    make_run_cycle_config, make_run_cycle_download_config_builder,
    make_run_cycle_library_state_with_passes, make_shared_session_for_run_cycle,
};
use serde_json::{Value, json};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

const AFTER: &str = "v27-death-committed";
const WORKER: &str = "sync_loop::runner::tests::v027_baseline::process_death::v027_death_worker";
const POINTS: [&str; 8] = [
    "selection-admission-partial",
    "selection-work-committed",
    "publication-prepared",
    "published",
    "state-persisted",
    "checkpoint-persisted",
    "checkpoint-before-commit",
    "checkpoint-after-commit",
];

#[derive(Clone)]
struct DeathProvider {
    base: BaselineSession,
    quiet: bool,
}
#[async_trait::async_trait]
impl PhotosSession for DeathProvider {
    async fn post(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        assert!(!body.contains(OLD_TOKEN));
        let request: Value = serde_json::from_str(&body)?;
        if url.contains("/changes/zone?") {
            let token = request["zones"][0]["syncToken"].as_str().unwrap();
            assert!(token == BOUNDARY || token == AFTER);
            if self.quiet {
                assert_eq!(token, AFTER);
            }
            return Ok(
                json!({"zones":[{"zoneID":zone(),"records":[],"syncToken":AFTER,"moreComing":false}]}),
            );
        }
        if self.quiet
            && url.contains("/records/query?")
            && request["query"]["recordType"] != "CheckIndexingState"
        {
            panic!("quiet completed v27 baseline re-enumerated");
        }
        self.base.post(url, body, headers).await
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

async fn death_cycle(root: &Path, url: &str, quiet: bool) -> usize {
    reference_cycle(root, url, quiet, None).await
}
const CHANGED_MEDIA: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/data/media/metadata.jpg"
));
async fn reference_cycle(root: &Path, url: &str, quiet: bool, reference: Option<u8>) -> usize {
    reference_cycle_with_inventory(root, url, quiet, reference, true).await
}
async fn reference_cycle_with_inventory(
    root: &Path,
    url: &str,
    quiet: bool,
    reference: Option<u8>,
    include_missing: bool,
) -> usize {
    let (trace, _trace_guard) = crate::test_helpers::TracingCapture::install();
    let mut inventory = pair(
        "current-existing",
        "existing.JPG",
        &format!("{url}/existing"),
        1,
    );
    if include_missing {
        inventory.extend(pair(
            "current-missing",
            "missing.JPG",
            &format!("{url}/missing"),
            2,
        ));
    }
    if let Some(phase) = reference {
        inventory = pair(
            "current-missing",
            "missing.JPG",
            &format!("{url}/{}", if phase == 0 { "blocked" } else { "missing" }),
            2,
        );
        inventory[0]["fields"]["resOriginalVidComplRes"] = json!({"value":{"downloadURL":format!("{url}/motion"),"size":MEDIA.len(),"fileChecksum":generation(2)}});
        inventory[0]["fields"]["resOriginalVidComplFileType"] =
            json!({"value":"com.apple.quicktime-movie"});
        if phase > 0 {
            inventory[0]["recordName"] = json!("new-current-master");
            inventory[1]["fields"]["masterRef"]["value"]["recordName"] =
                json!("new-current-master");
            inventory[0]["fields"]["resOriginalRes"]["value"]["size"] = json!(CHANGED_MEDIA.len());
            inventory[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
                json!(generation(3));
        }
    }
    let provider = DeathProvider {
        quiet,
        base: BaselineSession {
            database: "private",
            cycle: 0,
            inventory: Arc::new(inventory),
            concurrent: Arc::new(Vec::new()),
            incremental_addition: Arc::new(Vec::new()),
            requests: Arc::new(Mutex::new(Vec::new())),
            inventory_queries: Arc::new(AtomicUsize::new(0)),
            inventory_starts: Arc::new(AtomicUsize::new(0)),
            boundaries: Arc::new(AtomicUsize::new(0)),
            changes: Arc::new(AtomicUsize::new(0)),
        },
    };
    let db = Arc::new(
        SqliteStateDb::open_owned(&root.join("state.db"), &baseline_owner())
            .await
            .unwrap(),
    );
    if reference.is_some_and(|phase| phase > 0) {
        db.make_selection_retry_due(baseline_owner(), "asset-current-missing".into())
            .await
            .unwrap();
    }
    let mut library = PhotoLibrary::new(
        "https://synthetic.example/database/1/photos/production/private".into(),
        Arc::new(std::collections::HashMap::new()),
        Box::new(provider.clone()),
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
        exclude_ids: Default::default(),
    }];
    let key = sync_token_key(ZONE);
    let mut selected = make_run_cycle_library_state_with_passes(ZONE, &key, passes);
    selected.library = library;
    let media_root = root.join("media");
    let base = make_run_cycle_download_config_builder(&media_root, db.clone());
    let builder = |mode, excluded, groupings, zone| {
        let mut config = (*base(mode, excluded, groupings, zone)).clone();
        config.folder_structure = "none".into();
        config.concurrent_downloads = 1;
        if reference.is_some() {
            config.media.live_photos = true;
        }
        config.retry.max_retries = 0;
        Arc::new(config)
    };
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
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
    assert_eq!(
        result.failed_count,
        usize::from(reference == Some(0)),
        "{result:?} events={:?}",
        trace.events()
    );
    assert!(result.db_sync_token_advance_safe, "{result:?}");
    let token = db.get_metadata(&key).await.unwrap();
    assert_eq!(
        token.as_deref(),
        Some(AFTER),
        "recovery must commit the baseline token"
    );
    if reference.is_some_and(|phase| phase > 0) {
        assert_eq!(
            provider.base.inventory_queries.load(Ordering::SeqCst),
            0,
            "reference retry must not repeat inventory"
        );
    }
    if quiet {
        assert_eq!(provider.base.inventory_queries.load(Ordering::SeqCst), 0);
        assert_eq!(result.stats.downloaded, 0);
    }
    result.stats.downloaded
}

struct Worker(Child);
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn v027_sigkill_publication_reopens_then_two_quiet_cycles() {
    for point in POINTS {
        let server = wiremock::MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/missing"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/existing"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
            .expect(0)
            .mount(&server)
            .await;
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("fixture-owner"),
            b"kei-synthetic-process-death",
        )
        .unwrap();
        let media = root.path().join("media");
        std::fs::create_dir(&media).unwrap();
        let old = media.join("old-renamed.JPG");
        std::fs::write(&old, MEDIA).unwrap();
        let old_mtime = std::fs::metadata(&old).unwrap().modified().unwrap();
        let db_path = root.path().join("state.db");
        let db = SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap();
        let record = crate::test_helpers::TestAssetRecord::new("old-primary-master")
            .library("PrimarySync")
            .filename("existing.JPG")
            .checksum(&generation(1))
            .size(MEDIA.len() as u64)
            .build();
        db.upsert_seen(&record).await.unwrap();
        let sha = download::file::compute_sha256(&old).await.unwrap();
        db.mark_verified_download(
            "PrimarySync",
            "old-primary-master",
            "original",
            &old,
            &sha,
            Some(&sha),
            false,
        )
        .await
        .unwrap();
        drop(db);
        let historical_before_prime = historical_rows(&db_path);
        let prime = wiremock::MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/existing"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
            .expect(1)
            .mount(&prime)
            .await;
        assert_eq!(
            reference_cycle_with_inventory(root.path(), &prime.uri(), false, None, false).await,
            1,
            "prime creates the current-owned existing publication"
        );
        let prime_urls: Vec<String> = prime
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .map(|request| request.url.to_string())
            .collect();
        assert_eq!(
            prime_urls.len(),
            1,
            "prime transfer identity: {prime_urls:?}"
        );
        assert!(
            prime_urls[0].ends_with("/existing"),
            "prime transfer identity: {prime_urls:?}"
        );
        prime.verify().await;
        use rusqlite::types::Value as Sql;
        let existing = media.join("existing.JPG");
        let existing_sha = download::file::compute_sha256(&existing).await.unwrap();
        let existing_bytes = std::fs::read(&existing).unwrap();
        let existing_mtime = std::fs::metadata(&existing).unwrap().modified().unwrap();
        assert_eq!(existing_bytes, MEDIA);
        let expected_scope = serde_json::to_string(&json!({
            "format": 1,
            "realm": "com",
            "container": "com.apple.photos.cloud",
            "environment": "production",
            "database": "private",
            "zone": zone(),
        }))
        .unwrap();
        let owner = baseline_owner();
        let (account_key, provider_key) = owner.authenticated_keys().unwrap();
        let native = SelectionPath::from_path(&existing).key().unwrap();
        assert_eq!(
            count_at(
                &db_path,
                &format!(
                    "SELECT count(*) FROM assets WHERE library='{ZONE}' AND id='asset-current-existing' \
                     AND version_size='original' AND status='downloaded' AND local_path='{}' \
                     AND local_checksum='{existing_sha}' AND checksum='{}'",
                    existing.display(),
                    generation(1)
                )
            ),
            1,
            "prime asset receipt"
        );
        let decision = db_rows(
            &db_path,
            "SELECT generation,pass_key,child,outcome,admission,manifest,manifest_hash FROM provider_active_decisions WHERE child='asset-current-existing'",
        );
        assert_eq!(decision.len(), 1, "prime decision cardinality");
        assert_eq!(
            decision[0][2],
            Sql::Text("asset-current-existing".to_owned())
        );
        assert_eq!(decision[0][3], Sql::Text("selected".to_owned()));
        assert_eq!(decision[0][4], Sql::Text("admitted".to_owned()));
        let Sql::Text(pass_key) = &decision[0][1] else {
            panic!("prime pass key type");
        };
        assert_eq!(pass_key.len(), 64, "prime pass key length");
        let Sql::Blob(manifest) = &decision[0][5] else {
            panic!("prime decision manifest type");
        };
        let manifest: Value = serde_json::from_slice(manifest).unwrap();
        assert_eq!(
            manifest["state_id"],
            json!("asset-current-existing"),
            "prime decision root"
        );
        assert_eq!(manifest["sources"], json!([]), "prime decision sources");
        assert_eq!(
            manifest["decision"]["child"],
            json!("asset-current-existing")
        );
        assert_eq!(manifest["decision"]["pass_key"], json!(pass_key));
        assert_eq!(
            manifest["decision"]["destinations"][0]["path"],
            serde_json::to_value(SelectionPath::from_path(&existing)).unwrap(),
            "prime manifest destination"
        );
        assert_eq!(
            manifest["decision"]["destinations"][0]["checksum"],
            json!(generation(1))
        );
        assert_eq!(
            manifest["decision"]["destinations"][0]["size"],
            json!(MEDIA.len())
        );
        let Sql::Text(manifest_hash) = &decision[0][6] else {
            panic!("prime manifest hash type");
        };
        assert_eq!(manifest_hash.len(), 64, "prime manifest hash length");
        let destination = db_rows(
            &db_path,
            "SELECT generation,pass_key,child,library,asset_id,master,version_size,path,compat_path,checksum,size_bytes,admitted,verified_media,local_checksum,source_checksum FROM provider_active_destinations WHERE child='asset-current-existing'",
        );
        assert_eq!(destination.len(), 1, "prime destination cardinality");
        assert_eq!(destination[0][1], Sql::Text(pass_key.clone()));
        assert_eq!(
            destination[0][2],
            Sql::Text("asset-current-existing".to_owned())
        );
        assert_eq!(destination[0][3], Sql::Text(ZONE.to_owned()));
        assert_eq!(
            destination[0][4],
            Sql::Text("asset-current-existing".to_owned())
        );
        assert_eq!(destination[0][5], Sql::Text("current-existing".to_owned()));
        assert_eq!(destination[0][6], Sql::Text("original".to_owned()));
        assert_eq!(destination[0][7], Sql::Text(native.clone()));
        assert_eq!(destination[0][8], Sql::Text(existing.display().to_string()));
        assert_eq!(destination[0][9], Sql::Text(generation(1)));
        assert_eq!(destination[0][10], Sql::Integer(MEDIA.len() as i64));
        assert_eq!(destination[0][11], Sql::Integer(1));
        assert_eq!(destination[0][12], Sql::Integer(1));
        assert_eq!(destination[0][13], Sql::Text(existing_sha.clone()));
        assert_eq!(destination[0][14], Sql::Text(existing_sha.clone()));
        let generation_id = destination[0][0].clone();
        let generations = db_rows(
            &db_path,
            "SELECT id,account_key,provider_key,scope,specification FROM provider_active_generations",
        );
        let generation_row = generations
            .iter()
            .find(|row| row[0] == generation_id)
            .expect("prime generation binding");
        assert_eq!(generation_row[1], Sql::Text(account_key.to_owned()));
        assert_eq!(generation_row[2], Sql::Text(provider_key.to_owned()));
        assert_eq!(generation_row[3], Sql::Text(expected_scope.clone()));
        let Sql::Blob(specification) = &generation_row[4] else {
            panic!("prime generation specification type");
        };
        let specification: Value = serde_json::from_slice(specification).unwrap();
        assert_eq!(specification["zone"], zone(), "prime generation zone");
        let sources = db_rows(
            &db_path,
            "SELECT generation,pass_key,child,page_id,ordinal FROM provider_active_sources WHERE child='asset-current-existing'",
        );
        assert!(
            sources.is_empty(),
            "synthetic inventory has no rank-page sources; the decision manifest is the root"
        );
        let receipt_sql = "SELECT account_key,provider_key,scope,library,child,master,state_id,origin_pass,version_size,provider_checksum,provider_size,native_path,source_checksum,local_checksum,generation_id FROM download_receipts WHERE state_id='asset-current-existing'";
        let receipt = db_rows(&db_path, receipt_sql);
        assert_eq!(receipt.len(), 1, "prime receipt cardinality");
        assert_eq!(
            receipt[0],
            vec![
                Sql::Text(account_key.to_owned()),
                Sql::Text(provider_key.to_owned()),
                Sql::Text(expected_scope),
                Sql::Text(ZONE.to_owned()),
                Sql::Text("asset-current-existing".to_owned()),
                Sql::Text("current-existing".to_owned()),
                Sql::Text("asset-current-existing".to_owned()),
                Sql::Text(pass_key.clone()),
                Sql::Text("original".to_owned()),
                Sql::Text(generation(1)),
                Sql::Integer(MEDIA.len() as i64),
                Sql::Text(native),
                Sql::Text(existing_sha.clone()),
                Sql::Text(existing_sha.clone()),
                Sql::Null,
            ]
        );
        let metadata_sql = "SELECT library,id,version_size,local_path,provider_checksum,local_checksum,source_checksum FROM asset_metadata_paths WHERE library='SharedSync-v027-synthetic' AND id='asset-current-existing'";
        let metadata = db_rows(&db_path, metadata_sql);
        assert_eq!(
            metadata,
            vec![vec![
                Sql::Text(ZONE.to_owned()),
                Sql::Text("asset-current-existing".to_owned()),
                Sql::Text("original".to_owned()),
                Sql::Text(existing.display().to_string()),
                Sql::Text(generation(1)),
                Sql::Text(existing_sha.clone()),
                Sql::Text(existing_sha.clone()),
            ]],
            "prime metadata receipt"
        );
        assert_eq!(
            count_at(&db_path, "SELECT count(*) FROM download_receipts"),
            1,
            "one primed compact receipt"
        );
        assert_eq!(
            count_at(
                &db_path,
                "SELECT count(*) FROM download_receipts WHERE state_id='asset-current-missing'"
            ),
            0,
            "no receipt for the unprimed missing resource"
        );
        assert_eq!(
            count_at(
                &db_path,
                "SELECT count(DISTINCT state_id) FROM download_receipts"
            ),
            1,
            "prime receipt belongs to one distinct resource"
        );
        let primed_receipt = receipt[0].clone();
        let primed_metadata = metadata[0].clone();
        assert_eq!(
            historical_rows(&db_path),
            historical_before_prime,
            "foreign Primary history unchanged by priming"
        );
        assert_eq!(std::fs::read(&old).unwrap(), MEDIA);
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            super::clear_test_epoch(&conn);
            conn.execute(
                "INSERT INTO metadata(key,value) VALUES(?1,?2) \
                 ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                rusqlite::params![format!("sync_token:{ZONE}"), OLD_TOKEN],
            )
            .unwrap();
        }
        let historical = historical_rows(&db_path);
        let log = std::fs::File::create(root.path().join("worker.log")).unwrap();
        let mut worker = Worker(
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", WORKER, "--ignored", "--nocapture"])
                .env("KEI_TEST_PROCESS_DEATH_ROOT", root.path())
                .env("KEI_TEST_PROCESS_DEATH_POINT", point)
                .env("KEI_V027_DEATH_URL", server.uri())
                .stdout(Stdio::from(log.try_clone().unwrap()))
                .stderr(Stdio::from(log))
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(45);
        while !root.path().join("ready").exists() {
            assert!(
                worker.0.try_wait().unwrap().is_none(),
                "worker exited at {point}: {}",
                std::fs::read_to_string(root.path().join("worker.log")).unwrap()
            );
            assert!(Instant::now() < deadline, "worker timeout at {point}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            std::fs::read_to_string(root.path().join("ready")).unwrap(),
            point
        );
        worker.0.kill().unwrap();
        assert_eq!(worker.0.wait().unwrap().signal(), Some(9));
        // Model the watch interval elapsing while the killed process's crash
        // guard remains: the production epoch gate then retries the baseline.
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            super::force_epoch_due(&conn, ZONE);
        }
        let requests_before = server.received_requests().await.unwrap().len();
        assert_eq!(
            requests_before,
            usize::from(!point.starts_with("selection-"))
        );
        if matches!(
            point,
            "checkpoint-before-commit" | "checkpoint-after-commit"
        ) {
            let reopened = SqliteStateDb::open_owned(&db_path, &baseline_owner())
                .await
                .unwrap();
            let token = reopened.get_metadata(&sync_token_key(ZONE)).await.unwrap();
            let proof = reopened
                .get_metadata(&format!("observed_selection:v27:{ZONE}"))
                .await
                .unwrap();
            assert_eq!(
                token.is_some(),
                proof.is_some(),
                "token/proof split at {point}"
            );
            assert_eq!(proof.is_some(), point == "checkpoint-after-commit");
        }
        // checkpoint-persisted is the end-of-cycle hook, not an in-transaction pause.
        death_cycle(root.path(), &server.uri(), false).await;
        let completed = SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap();
        assert!(completed.get_pending().await.unwrap().is_empty());
        assert!(completed.get_failed().await.unwrap().is_empty());
        let receipts = completed.get_downloaded_page(0, 20).await.unwrap();
        assert!(receipts.iter().any(|r| r.library.as_ref() == ZONE
            && r.id.as_ref() == "asset-current-missing"
            && r.local_path.as_ref() == Some(&media.join("missing.JPG"))));
        drop(completed);
        let new_mtime = std::fs::metadata(media.join("missing.JPG"))
            .unwrap()
            .modified()
            .unwrap();
        let missing = media.join("missing.JPG");
        assert_eq!(std::fs::read(&existing).unwrap(), existing_bytes);
        assert_eq!(
            std::fs::metadata(&existing).unwrap().modified().unwrap(),
            existing_mtime
        );
        assert_eq!(db_rows(&db_path, receipt_sql), vec![primed_receipt.clone()]);
        assert_eq!(
            db_rows(&db_path, metadata_sql),
            vec![primed_metadata.clone()]
        );
        assert_eq!(std::fs::read(&old).unwrap(), MEDIA);
        assert_eq!(std::fs::read(&missing).unwrap(), MEDIA);
        assert_eq!(
            std::fs::metadata(&old).unwrap().modified().unwrap(),
            old_mtime
        );
        assert_eq!(
            std::fs::metadata(&missing).unwrap().modified().unwrap(),
            new_mtime
        );
        assert_eq!(historical_rows(&db_path), historical);
        assert_eq!(
            count_at(&db_path, "SELECT count(*) FROM download_receipts"),
            2,
            "two distinct completed resources after recovery"
        );
        assert_eq!(
            count_at(
                &db_path,
                "SELECT count(*) FROM download_receipts WHERE state_id='asset-current-existing'"
            ),
            1,
            "primed resource receipt cardinality"
        );
        assert_eq!(
            count_at(
                &db_path,
                "SELECT count(*) FROM download_receipts WHERE state_id='asset-current-missing'"
            ),
            1,
            "missing resource receipt cardinality"
        );
        assert_eq!(
            count_at(
                &db_path,
                "SELECT count(DISTINCT state_id) FROM download_receipts"
            ),
            2,
            "two distinct receipt identities"
        );
        let missing_native = SelectionPath::from_path(&missing).key().unwrap();
        assert_eq!(
            db_rows(
                &db_path,
                "SELECT provider_checksum,native_path,source_checksum,local_checksum FROM download_receipts WHERE state_id='asset-current-missing'"
            ),
            vec![vec![
                Sql::Text(generation(2)),
                Sql::Text(missing_native),
                Sql::Text(existing_sha.clone()),
                Sql::Text(existing_sha.clone()),
            ]],
            "missing compact receipt keeps its own resource identity"
        );
        for _ in 0..2 {
            death_cycle(root.path(), &server.uri(), true).await;
            assert_eq!(historical_rows(&db_path), historical);
            assert_eq!(std::fs::read(&old).unwrap(), MEDIA);
            assert_eq!(
                std::fs::metadata(&old).unwrap().modified().unwrap(),
                old_mtime
            );
            assert_eq!(std::fs::read(media.join("missing.JPG")).unwrap(), MEDIA);
            assert_eq!(
                std::fs::metadata(media.join("missing.JPG"))
                    .unwrap()
                    .modified()
                    .unwrap(),
                new_mtime
            );
            assert_eq!(std::fs::read(&existing).unwrap(), existing_bytes);
            assert_eq!(
                std::fs::metadata(&existing).unwrap().modified().unwrap(),
                existing_mtime
            );
            assert_eq!(db_rows(&db_path, receipt_sql), vec![primed_receipt.clone()]);
            assert_eq!(
                db_rows(&db_path, metadata_sql),
                vec![primed_metadata.clone()]
            );
            assert_eq!(
                count_at(&db_path, "SELECT count(*) FROM download_receipts"),
                2
            );
        }
        let urls: Vec<String> = server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .map(|request| request.url.to_string())
            .collect();
        assert_eq!(
            urls.len(),
            1,
            "exactly one transfer across crash and recovery at {point}: {urls:?}"
        );
        assert!(
            urls[0].ends_with("/missing"),
            "scenario transfer identity at {point}: {urls:?}"
        );
        server.verify().await;
    }
}

#[tokio::test]
#[ignore = "synthetic subprocess killed by v27 parent at a durable hook"]
async fn v027_death_worker() {
    let root = PathBuf::from(std::env::var_os("KEI_TEST_PROCESS_DEATH_ROOT").unwrap());
    let url = std::env::var("KEI_V027_DEATH_URL").unwrap();
    death_cycle(&root, &url, false).await;
    panic!("worker completed without requested hook");
}

const REFERENCE_WORKER: &str =
    "sync_loop::runner::tests::v027_baseline::process_death::v027_reference_death_worker";
fn completed_reference_rows(root: &Path) -> Vec<Vec<rusqlite::types::Value>> {
    let conn = rusqlite::Connection::open(root.join("state.db")).unwrap();
    let mut stmt = conn
        .prepare("SELECT * FROM download_receipts WHERE master='current-missing' ORDER BY id")
        .unwrap();
    let n = stmt.column_count();
    stmt.query_map([], |r| (0..n).map(|i| r.get(i)).collect())
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}
fn db_rows(path: &Path, sql: &str) -> Vec<Vec<rusqlite::types::Value>> {
    let conn = rusqlite::Connection::open(path).unwrap();
    let mut stmt = conn.prepare(sql).unwrap();
    let n = stmt.column_count();
    stmt.query_map([], |r| (0..n).map(|i| r.get(i)).collect())
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}
fn count_at(path: &Path, sql: &str) -> i64 {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.query_row(sql, [], |r| r.get(0)).unwrap()
}
#[tokio::test]
async fn changed_reference_sigkill_transfers_new_bytes_without_repeating_completed_motion() {
    for point in [
        "selection-reference-transferred",
        "selection-reference-committed",
    ] {
        let server = wiremock::MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/blocked"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/motion"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/missing"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(CHANGED_MEDIA))
            .expect(1)
            .mount(&server)
            .await;
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("fixture-owner"),
            b"kei-synthetic-process-death",
        )
        .unwrap();
        assert_eq!(
            reference_cycle(root.path(), &server.uri(), false, Some(0)).await,
            1
        );
        let completed = completed_reference_rows(root.path());
        assert_eq!(completed.len(), 1);
        let files: Vec<_> = std::fs::read_dir(root.path().join("media"))
            .unwrap()
            .map(|entry| {
                let path = entry.unwrap().path();
                let bytes = std::fs::read(&path).unwrap();
                let stamp = std::fs::metadata(&path).unwrap().modified().unwrap();
                (path, bytes, stamp)
            })
            .collect();
        assert_eq!(files.len(), 1);
        let log = std::fs::File::create(root.path().join("worker.log")).unwrap();
        let mut worker = Worker(
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", REFERENCE_WORKER, "--ignored", "--nocapture"])
                .env("KEI_TEST_PROCESS_DEATH_ROOT", root.path())
                .env("KEI_TEST_PROCESS_DEATH_POINT", point)
                .env("KEI_V027_DEATH_URL", server.uri())
                .stdout(Stdio::from(log.try_clone().unwrap()))
                .stderr(Stdio::from(log))
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(45);
        while !root.path().join("ready").exists() {
            assert!(
                worker.0.try_wait().unwrap().is_none(),
                "worker exited: {}",
                std::fs::read_to_string(root.path().join("worker.log")).unwrap()
            );
            assert!(Instant::now() < deadline, "worker timeout at {point}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            std::fs::read_to_string(root.path().join("ready")).unwrap(),
            point
        );
        worker.0.kill().unwrap();
        assert_eq!(worker.0.wait().unwrap().signal(), Some(9));
        {
            let db = SqliteStateDb::open_owned(&root.path().join("state.db"), &baseline_owner())
                .await
                .unwrap();
            let conn = db
                .acquire_lock("one unfinished reservation survives SIGKILL")
                .unwrap();
            let pending:Vec<(String,String)>=conn.prepare("SELECT master,compat_path FROM provider_active_destinations WHERE verified_media=0").unwrap().query_map([],|r|Ok((r.get(0)?,r.get(1)?))).unwrap().collect::<Result<_,_>>().unwrap();
            assert_eq!(
                pending,
                vec![(
                    if point == "selection-reference-transferred" {
                        "current-missing"
                    } else {
                        "new-current-master"
                    }
                    .to_owned(),
                    root.path()
                        .join("media/missing.JPG")
                        .to_string_lossy()
                        .into_owned()
                )]
            );
        }
        assert_eq!(completed_reference_rows(root.path()), completed);
        assert_eq!(
            reference_cycle(root.path(), &server.uri(), false, Some(1)).await,
            1
        );
        for _ in 0..2 {
            assert_eq!(
                reference_cycle(root.path(), &server.uri(), true, Some(2)).await,
                0
            );
        }
        assert_eq!(completed_reference_rows(root.path()), completed);
        for (path, bytes, stamp) in files {
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
            assert_eq!(std::fs::metadata(path).unwrap().modified().unwrap(), stamp);
        }
        assert_eq!(
            std::fs::read(root.path().join("media/missing.JPG")).unwrap(),
            CHANGED_MEDIA
        );
        let db = SqliteStateDb::open_owned(&root.path().join("state.db"), &baseline_owner())
            .await
            .unwrap();
        {
            let conn = db
                .acquire_lock("all obligations settled after killed reference refresh")
                .unwrap();
            assert_eq!(
                conn.query_row::<i64, _, _>(
                    "SELECT count(*) FROM provider_active_destinations WHERE verified_media=0",
                    [],
                    |r| r.get(0)
                )
                .unwrap(),
                0
            );
            let receipt:(String,i64,String,String)=conn.query_row("SELECT provider_checksum,provider_size,source_checksum,local_checksum FROM download_receipts WHERE child='asset-current-missing' AND master='new-current-master' AND version_size='original'",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap();
            use sha2::{Digest, Sha256};
            let hash = format!("{:x}", Sha256::digest(CHANGED_MEDIA));
            assert_eq!(
                receipt,
                (
                    generation(3),
                    i64::try_from(CHANGED_MEDIA.len()).unwrap(),
                    hash.clone(),
                    hash
                )
            );
        }
        server.verify().await;
    }
}
#[tokio::test]
#[ignore = "dedicated reference transition subprocess killed by parent"]
async fn v027_reference_death_worker() {
    let root = PathBuf::from(std::env::var_os("KEI_TEST_PROCESS_DEATH_ROOT").unwrap());
    let url = std::env::var("KEI_V027_DEATH_URL").unwrap();
    reference_cycle(&root, &url, false, Some(1)).await;
    panic!("reference worker missed requested hook");
}
