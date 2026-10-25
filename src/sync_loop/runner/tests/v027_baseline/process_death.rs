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
    let mut inventory = pair(
        "current-existing",
        "existing.JPG",
        &format!("{url}/existing"),
        1,
    );
    inventory.extend(pair(
        "current-missing",
        "missing.JPG",
        &format!("{url}/missing"),
        2,
    ));
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
    assert_eq!(result.failed_count, 0, "{result:?}");
    assert!(result.db_sync_token_advance_safe, "{result:?}");
    assert_eq!(db.get_metadata(&key).await.unwrap().as_deref(), Some(AFTER));
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
        db.set_metadata(&format!("sync_token:{ZONE}"), OLD_TOKEN)
            .await
            .unwrap();
        drop(db);
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
        let requests_before = server.received_requests().await.unwrap().len();
        assert_eq!(
            requests_before,
            usize::from(!point.starts_with("selection-"))
        );
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
        }
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            1,
            "exactly one transfer across crash and recovery at {point}"
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
