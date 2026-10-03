//! Actual SIGKILL between durable production transitions, followed by reopen,
//! production recovery, and two quiet cycles. This proves process death only;
//! it does not model storage caches or power loss.
use std::collections::BTreeMap;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

use crate::commands::{AlbumPass, PassKind};
use crate::icloud::photos::PhotosSession;
use crate::sync_loop::test_support::{
    RUN_CYCLE_ASSET_DATE_MS, RunCycleDownloadConfigOptions, album_count_response,
    full_album_page_with_download, make_full_album_with_boxed_session, make_run_cycle_config,
    make_run_cycle_download_config_builder_with_options, make_run_cycle_library_state_with_passes,
    make_shared_session_for_run_cycle,
};
use crate::{download, state};

const POINTS: [&str; 6] = [
    "journal-displaced",
    "journal-installed",
    "journal-committed",
    "published",
    "state-persisted",
    "checkpoint-persisted",
];
const WORKER: &str = "sync_loop::runner::tests::process_death::process_death_worker";
const ZONE: &str = "PrimarySync";
const FOREIGN: &str = "SharedSync-DEATH";
const ID: &str = "asset-death";
const MEDIA: &[u8] = b"\xff\xd8\xff\xe0\0\x10JFIF\0\x01\x01\0\0\x01\0\x01\0\0\xff\xd9";
const OLD: &[u8] = b"\xff\xd8\xff";
const PRIVATE: &[u8] = b"unselected zone: private bytes";

fn checksum(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(Sha256::digest(bytes))
}
fn hash(bytes: &[u8]) -> String {
    data_encoding::HEXLOWER.encode(&Sha256::digest(bytes))
}
fn target(root: &Path) -> PathBuf {
    let date = chrono::DateTime::from_timestamp_millis(RUN_CYCLE_ASSET_DATE_MS).unwrap();
    root.join("media")
        .join(
            date.with_timezone(&chrono::Local)
                .format("%Y/%m/%d")
                .to_string(),
        )
        .join("photo.JPG")
}
fn rows(root: &Path, sql: &str) -> Vec<Vec<rusqlite::types::Value>> {
    let conn = rusqlite::Connection::open(root.join("state.db")).unwrap();
    let mut stmt = conn.prepare(sql).unwrap();
    let count = stmt.column_count();
    stmt.query_map([], |row| (0..count).map(|i| row.get(i)).collect())
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}
fn count(root: &Path, sql: &str) -> i64 {
    let conn = rusqlite::Connection::open(root.join("state.db")).unwrap();
    conn.query_row(sql, [], |row| row.get(0)).unwrap()
}
fn files(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn visit(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                visit(root, &path, out);
            } else {
                out.insert(
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    std::fs::read(path).unwrap(),
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    visit(root, root, &mut out);
    out
}
fn durable(root: &Path) -> Vec<Vec<Vec<rusqlite::types::Value>>> {
    ["SELECT library,id,version_size,status,local_path,local_checksum,last_error,metadata_write_failed_at,capture_repair_metadata_hash,capture_repair_output_checksum FROM assets ORDER BY 1,2,3",
     "SELECT library,id,version_size,local_path,provider_checksum,local_checksum,metadata_write_failed_at,capture_repair_metadata_hash,capture_repair_output_checksum FROM asset_metadata_paths ORDER BY 1,2,3,4",
     "SELECT library,asset_record_name,master_record_name FROM asset_master_mappings ORDER BY 1,2",
     "SELECT library,asset_id FROM metadata_capture_retries ORDER BY 1,2",
     "SELECT path FROM owned_temp_files ORDER BY path",
     "SELECT id,status,interrupted FROM sync_runs WHERE status!='complete' ORDER BY id",
     "SELECT key,value FROM metadata WHERE key LIKE 'sync_token:%' OR key LIKE 'pending_sync_token:%' OR key LIKE 'unresolved_asset_identity:%' ORDER BY key"]
        .iter().map(|sql| rows(root, sql)).collect()
}

#[derive(Clone, Debug)]
struct Provider {
    records: Vec<Value>,
    hydrations: Arc<AtomicUsize>,
    requests: Arc<AtomicUsize>,
    quiet: bool,
}
#[async_trait::async_trait]
impl PhotosSession for Provider {
    async fn post(&self, url: &str, body: String, _: &[(&str, &str)]) -> anyhow::Result<Value> {
        assert!(
            self.requests.fetch_add(1, Ordering::SeqCst) < 80,
            "bounded provider requests"
        );
        let request: Value = serde_json::from_str(&body)?;
        if url.contains("/changes/zone?") {
            let token = request["zones"][0]["syncToken"].as_str();
            return Ok(
                json!({"zones":[{"zoneID":{"zoneName":ZONE,"ownerRecordName":"_defaultOwner"},"records":if token.is_none() {self.records.clone()} else {Vec::new()},"syncToken":"after","moreComing":false}]}),
            );
        }
        if url.contains("/records/query/batch?") {
            return Ok(album_count_response(1));
        }
        if url.contains("/records/query?") {
            let offset = request["query"]["filterBy"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|f| f["fieldName"] == "startRank")
                .and_then(|f| f["fieldValue"]["value"].as_u64())
                .unwrap_or(0);
            return Ok(
                json!({"records":if offset == 0 {self.records.clone()} else {Vec::new()},"syncToken":"after"}),
            );
        }
        assert!(url.contains("/records/lookup?"), "unexpected route {url}");
        assert!(!self.quiet, "quiet cycle hydrated");
        self.hydrations.fetch_add(1, Ordering::SeqCst);
        let requested = request["records"].as_array().unwrap();
        Ok(
            json!({"records":self.records.iter().filter(|r| requested.iter().any(|q|q["recordName"]==r["recordName"])).cloned().collect::<Vec<_>>()}),
        )
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}
async fn cycle(root: &Path, url: &str, quiet: bool) -> (usize, usize) {
    let mut page = full_album_page_with_download(
        ZONE,
        "death",
        "after",
        url,
        MEDIA.len() as u64,
        &checksum(MEDIA),
    );
    page["records"][0]["fields"]["filenameEnc"]["value"] = json!("photo.JPG");
    let hydrations = Arc::new(AtomicUsize::new(0));
    let provider = Provider {
        records: page["records"].as_array().unwrap().clone(),
        hydrations: hydrations.clone(),
        requests: Arc::new(AtomicUsize::new(0)),
        quiet,
    };
    let pass = AlbumPass {
        kind: PassKind::Unfiled,
        album: make_full_album_with_boxed_session(ZONE, Box::new(provider.clone())),
        exclude_ids: Arc::default(),
    };
    let mut library =
        make_run_cycle_library_state_with_passes(ZONE, "sync_token:PrimarySync", vec![pass]);
    library.library =
        crate::icloud::photos::PhotoLibrary::new_stub_with_zone(Box::new(provider), ZONE);
    let db: Arc<dyn download::DownloadStore> = Arc::new(
        state::SqliteStateDb::open(&root.join("state.db"))
            .await
            .unwrap(),
    );
    let media = root.join("media");
    let base = make_run_cycle_download_config_builder_with_options(
        &media,
        db.clone(),
        RunCycleDownloadConfigOptions {
            concurrent_downloads: Some(1),
            #[cfg(feature = "xmp")]
            xmp_sidecar: true,
            ..Default::default()
        },
    );
    let build = |mode, excluded, groups, zone| {
        let mut built = base(mode, excluded, groups, zone);
        Arc::make_mut(&mut built).repair_truncated = true;
        built
    };
    let (_session_dir, session) = make_shared_session_for_run_cycle().await;
    let mut config = make_run_cycle_config();
    config.runtime.repair_truncated = true;
    let result = crate::sync_cycle::run_cycle(
        &[&library],
        &config,
        Some(db.as_ref()),
        false,
        &build,
        download::DownloadControls::download_hidden(),
        &session,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(result.failed_count, 0);
    assert!(!result.session_expired);
    assert!(result.db_sync_token_advance_safe);
    assert_eq!(result.stats.exif_failures, 0);
    if quiet {
        assert_eq!(
            result.stats.metadata_capture_refreshed, 0,
            "quiet metadata repair"
        );
        assert!(
            !result.stats.metadata_capture_progressed,
            "quiet capture progression"
        );
    }
    (result.stats.downloaded, hydrations.load(Ordering::SeqCst))
}
async fn seed(root: &Path) {
    std::fs::create_dir_all(root.join("media")).unwrap();
    std::fs::write(root.join("fixture-owner"), b"kei-synthetic-process-death").unwrap();
    let db = state::SqliteStateDb::open(&root.join("state.db"))
        .await
        .unwrap();
    let date = chrono::DateTime::from_timestamp_millis(RUN_CYCLE_ASSET_DATE_MS).unwrap();
    let path = root.join("media/foreign.jpg");
    std::fs::write(&path, PRIVATE).unwrap();
    std::fs::write(
        root.join("media/foreign.jpg.xmp"),
        b"private unrelated sidecar",
    )
    .unwrap();
    let row = crate::test_helpers::TestAssetRecord::new(ID)
        .library(FOREIGN)
        .filename("foreign.jpg")
        .created_at(date)
        .added_at(date)
        .size(PRIVATE.len() as u64)
        .checksum(&checksum(PRIVATE))
        .build();
    db.upsert_seen(&row).await.unwrap();
    db.upsert_asset_master_mapping(FOREIGN, ID, "death")
        .await
        .unwrap();
    db.mark_downloaded(
        FOREIGN,
        ID,
        "original",
        &path,
        &hash(PRIVATE),
        Some(&hash(PRIVATE)),
    )
    .await
    .unwrap();
    db.set_metadata("sync_token:SharedSync-DEATH", "foreign-before")
        .await
        .unwrap();
}
struct Worker(Child);
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn process_death_recovery_matrix() {
    let replay = std::env::var("KEI_PROCESS_DEATH_REPLAY").ok();
    if let Some(point) = &replay {
        assert!(
            POINTS.contains(&point.as_str()),
            "unknown fixed replay point"
        );
    }
    for point in POINTS
        .into_iter()
        .filter(|p| replay.as_deref().is_none_or(|r| r == *p))
    {
        eprintln!(
            "KEI_PROCESS_DEATH_REPLAY={point} cargo test --lib process_death_recovery_matrix -- --nocapture"
        );
        // A failed bind fails this proof honestly.
        let server = wiremock::MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/media"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
            .mount(&server)
            .await;
        let root = tempfile::tempdir().unwrap();
        let repair = point.starts_with("journal-");
        seed(root.path()).await;
        if repair {
            // Establish all config, identity, metadata and publication receipts
            // through production before simulating an explicitly approved repair.
            assert_eq!(
                cycle(root.path(), &format!("{}/media", server.uri()), false)
                    .await
                    .0,
                1
            );
            assert_eq!(std::fs::read(target(root.path())).unwrap(), MEDIA);
            std::fs::write(target(root.path()), OLD).unwrap();
            let db = state::SqliteStateDb::open(&root.path().join("state.db"))
                .await
                .unwrap();
            db.mark_failed(
                ZONE,
                ID,
                "original",
                crate::commands::reconcile::FILE_TRUNCATED_REASON,
            )
            .await
            .unwrap();
            db.set_metadata("sync_token:PrimarySync", "before")
                .await
                .unwrap();
        }
        let foreign = rows(
            root.path(),
            "SELECT * FROM assets WHERE library='SharedSync-DEATH'",
        );
        let foreign_receipt = rows(
            root.path(),
            "SELECT * FROM asset_metadata_paths WHERE library='SharedSync-DEATH'",
        );
        let log = std::fs::File::create(root.path().join("worker.log")).unwrap();
        let mut worker = Worker(
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", WORKER, "--ignored", "--nocapture"])
                .env("KEI_TEST_PROCESS_DEATH_ROOT", root.path())
                .env("KEI_TEST_PROCESS_DEATH_POINT", point)
                .env(
                    "KEI_TEST_PROCESS_DEATH_URL",
                    format!("{}/media", server.uri()),
                )
                .stdout(Stdio::from(log.try_clone().unwrap()))
                .stderr(Stdio::from(log))
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(20);
        while !root.path().join("ready").exists() {
            assert!(
                worker.0.try_wait().unwrap().is_none(),
                "worker exited before {point}: {}",
                std::fs::read_to_string(root.path().join("worker.log")).unwrap()
            );
            assert!(
                Instant::now() < deadline,
                "worker timeout at {point}: {}",
                std::fs::read_to_string(root.path().join("worker.log")).unwrap()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            std::fs::read_to_string(root.path().join("ready")).unwrap(),
            point
        );
        worker.0.kill().unwrap();
        assert_eq!(
            worker.0.wait().unwrap().signal(),
            Some(9),
            "actual SIGKILL without unwinding"
        );
        let checkpoint = count(
            root.path(),
            "SELECT count(*) FROM metadata WHERE key='sync_token:PrimarySync' AND value='after'",
        );
        assert_eq!(
            checkpoint,
            i64::from(point == "checkpoint-persisted"),
            "checkpoint cannot precede durable completion"
        );
        let persisted = count(
            root.path(),
            "SELECT count(*) FROM assets WHERE library='PrimarySync' AND status='downloaded'",
        );
        assert_eq!(
            persisted,
            i64::from(matches!(point, "state-persisted" | "checkpoint-persisted")),
            "finalization boundary"
        );
        let journal_files = files(&root.path().join("media"));
        let journals = journal_files
            .keys()
            .filter(|p| {
                p.components()
                    .any(|c| c.as_os_str().to_string_lossy().starts_with(".kei-replace-"))
            })
            .count();
        assert_eq!(journals > 0, repair, "real fallback journal survives death");
        if point == "journal-displaced" {
            assert!(!target(root.path()).exists());
        } else {
            assert_eq!(
                std::fs::read(target(root.path())).unwrap(),
                MEDIA,
                "published media bytes"
            );
        }
        if repair {
            assert_eq!(
                count(root.path(), "SELECT count(*) FROM owned_temp_files"),
                1,
                "death retains durable temporary ownership"
            );
        } else {
            assert_eq!(
                count(root.path(), "SELECT count(*) FROM owned_temp_files"),
                0
            );
        }
        if repair {
            assert!(
                journal_files
                    .iter()
                    .any(|(p, b)| p.file_name().is_some_and(|n| n == "original")
                        && b.as_slice() == OLD),
                "original bytes retained before restart"
            );
            assert_eq!(
                journal_files
                    .keys()
                    .any(|p| p.file_name().is_some_and(|n| n == "committed")),
                point == "journal-committed",
                "commit boundary"
            );
        }
        assert_eq!(
            count(
                root.path(),
                "SELECT count(*) FROM asset_metadata_paths WHERE library='PrimarySync'"
            ),
            i64::from(repair || persisted == 1),
            "publication receipt boundary"
        );
        // The repair runs in the retry pass after enumeration has completed
        // its tracked run. Initial-download finalization remains inside it.
        let interrupted = matches!(point, "published" | "state-persisted");
        let orphaned = u64::from(interrupted);
        assert_eq!(
            count(
                root.path(),
                "SELECT count(*) FROM sync_runs WHERE status='running'"
            ),
            i64::from(interrupted),
            "durable interrupted run"
        );
        {
            // Reuse the production runner's one-time startup recovery owner.
            let db = state::SqliteStateDb::open(&root.path().join("state.db"))
                .await
                .unwrap();
            assert_eq!(db.promote_orphaned_sync_runs().await.unwrap(), orphaned);
        }
        let downloads_before = server.received_requests().await.unwrap().len();
        let (downloaded, _) = cycle(root.path(), &format!("{}/media", server.uri()), false).await;
        assert!(downloaded <= 1, "bounded first recovery");
        assert_eq!(std::fs::read(target(root.path())).unwrap(), MEDIA);
        assert_eq!(
            count(
                root.path(),
                "SELECT count(*) FROM assets WHERE library='PrimarySync' AND status='downloaded' AND last_error IS NULL"
            ),
            1
        );
        assert_eq!(
            count(
                root.path(),
                "SELECT count(*) FROM asset_metadata_paths WHERE library='PrimarySync'"
            ),
            1,
            "one owned publication receipt"
        );
        assert_eq!(
            count(
                root.path(),
                "SELECT count(*) FROM metadata WHERE key='sync_token:PrimarySync' AND value='after'"
            ),
            1,
            "first eligible cycle advances"
        );
        assert_eq!(
            count(root.path(), "SELECT count(*) FROM metadata_capture_retries"),
            0
        );
        let claims = count(root.path(), "SELECT count(*) FROM owned_temp_files");
        assert!(
            claims <= i64::from(point == "journal-committed"),
            "only committed adoption can retain a recent part"
        );
        if claims == 1 {
            let db = state::SqliteStateDb::open(&root.path().join("state.db"))
                .await
                .unwrap();
            let owned = db.get_owned_temp_files_before(i64::MAX).await.unwrap();
            assert_eq!(owned.len(), 1);
            let part = &owned[0].path;
            assert!(part.starts_with(root.path().join("media")) && *part != target(root.path()));
            assert_eq!(
                std::fs::read(part).unwrap(),
                MEDIA,
                "retained owned prepared bytes"
            );
            // Model elapsed eligibility, retaining the exact path/ownership
            // evidence and completed-sync cutoff; never bypass deletion guards.
            std::fs::File::options()
                .write(true)
                .open(part)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(std::time::UNIX_EPOCH))
                .unwrap();
            drop(db);
            rusqlite::Connection::open(root.path().join("state.db"))
                .unwrap()
                .execute("UPDATE owned_temp_files SET claimed_at=0", [])
                .unwrap();
            assert_eq!(
                cycle(root.path(), &format!("{}/media", server.uri()), true).await,
                (0, 0)
            );
        }
        assert_eq!(
            count(root.path(), "SELECT count(*) FROM owned_temp_files"),
            0,
            "eligible production cleanup retires claims"
        );
        use rusqlite::types::Value as Sql;
        let expected_receipt = vec![vec![
            Sql::Text(target(root.path()).to_str().unwrap().to_string()),
            Sql::Text(hash(MEDIA)),
            Sql::Text(checksum(MEDIA)),
        ]];
        assert_eq!(
            rows(
                root.path(),
                "SELECT local_path,local_checksum,checksum FROM assets WHERE library='PrimarySync'"
            ),
            expected_receipt,
            "exact catalogue ownership and checksums"
        );
        assert_eq!(
            rows(
                root.path(),
                "SELECT local_path,local_checksum,provider_checksum FROM asset_metadata_paths WHERE library='PrimarySync'"
            ),
            expected_receipt,
            "exact publication ownership and checksums"
        );
        assert_eq!(
            count(
                root.path(),
                "SELECT count(*) FROM assets WHERE library='PrimarySync' AND (metadata_write_failed_at IS NOT NULL OR capture_repair_metadata_hash IS NOT NULL OR capture_repair_output_checksum IS NOT NULL)"
            ),
            0,
            "repair markers drained"
        );
        let stable_files = files(&root.path().join("media"));
        assert_eq!(
            stable_files.len(),
            3 + usize::from(cfg!(feature = "xmp")),
            "exact media and sidecar files; no duplicate or orphan publication"
        );
        assert!(
            !stable_files.keys().any(|p| p
                .components()
                .any(|c| c.as_os_str().to_string_lossy().starts_with(".kei-replace-"))),
            "journal drained"
        );
        #[cfg(feature = "xmp")]
        {
            use xmp_toolkit::{XmpMeta, xmp_ns};
            let sidecar =
                std::fs::read_to_string(target(root.path()).with_file_name("photo.JPG.xmp"))
                    .unwrap();
            let metadata = sidecar.parse::<XmpMeta>().expect("valid recovered sidecar");
            let expected = chrono::DateTime::from_timestamp_millis(RUN_CYCLE_ASSET_DATE_MS)
                .unwrap()
                .with_timezone(&chrono::Local)
                .format("%Y-%m-%dT%H:%M:%S")
                .to_string();
            assert_eq!(
                metadata
                    .property(xmp_ns::EXIF, "DateTimeOriginal")
                    .unwrap()
                    .value,
                expected
            );
        }
        assert_eq!(
            std::fs::read(root.path().join("media/foreign.jpg.xmp")).unwrap(),
            b"private unrelated sidecar"
        );
        assert_eq!(
            count(
                root.path(),
                "SELECT count(*) FROM sync_runs WHERE status='running'"
            ),
            0
        );
        assert_eq!(
            count(
                root.path(),
                "SELECT count(*) FROM sync_runs WHERE status='interrupted' AND interrupted=1"
            ),
            i64::from(interrupted)
        );
        let stable_state = durable(root.path());
        let stable_requests = server.received_requests().await.unwrap().len();
        assert!(
            stable_requests - downloads_before <= 1,
            "at most one recovery download"
        );
        for _ in 0..2 {
            assert_eq!(
                cycle(root.path(), &format!("{}/media", server.uri()), true).await,
                (0, 0)
            );
            assert_eq!(durable(root.path()), stable_state, "quiet durable state");
            assert_eq!(
                files(&root.path().join("media")),
                stable_files,
                "quiet bytes and file count"
            );
            assert_eq!(
                server.received_requests().await.unwrap().len(),
                stable_requests,
                "quiet tail cannot redownload"
            );
        }
        assert_eq!(
            rows(
                root.path(),
                "SELECT * FROM assets WHERE library='SharedSync-DEATH'"
            ),
            foreign
        );
        assert_eq!(
            rows(
                root.path(),
                "SELECT * FROM asset_metadata_paths WHERE library='SharedSync-DEATH'"
            ),
            foreign_receipt
        );
        assert_eq!(
            std::fs::read(root.path().join("media/foreign.jpg")).unwrap(),
            PRIVATE
        );
        assert_eq!(
            count(
                root.path(),
                "SELECT count(*) FROM metadata WHERE key='sync_token:SharedSync-DEATH' AND value='foreign-before'"
            ),
            1
        );
    }
}

#[tokio::test]
#[ignore = "dedicated synthetic subprocess; parent kills it at a fixed durable point"]
async fn process_death_worker() {
    let root =
        PathBuf::from(std::env::var_os("KEI_TEST_PROCESS_DEATH_ROOT").expect("parent fixture"));
    let url = std::env::var("KEI_TEST_PROCESS_DEATH_URL").unwrap();
    cycle(&root, &url, false).await;
    panic!("worker completed without reaching requested death point");
}
