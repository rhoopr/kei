//! Actual SIGKILL between durable production transitions, followed by reopen,
//! production recovery, and two quiet cycles. This proves process death only;
//! it does not model storage caches or power loss.
use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt;
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
use crate::download;
use crate::icloud::photos::PhotosSession;
use crate::sync_loop::test_support::{
    RUN_CYCLE_ASSET_DATE_MS, RunCycleDownloadConfigOptions, album_count_response,
    full_album_page_with_download, make_owned_cycle_album, make_run_cycle_config,
    make_run_cycle_download_config_builder_with_options, make_run_cycle_library_state_with_passes,
    make_shared_session_for_run_cycle,
};

const POINTS: [&str; 6] = [
    "journal-displaced",
    "journal-installed",
    "journal-committed",
    "published",
    "state-persisted",
    "checkpoint-persisted",
];
const WORKER: &str = "sync_loop::runner::tests::process_death::process_death_worker";
const METADATA_WORKER: &str =
    "sync_loop::runner::tests::process_death::metadata_ownership_process_death_worker";
const ZONE: &str = "PrimarySync";
const FOREIGN: &str = "SharedSync-DEATH";
const ID: &str = "asset-death";
const CAPTION: &str = "initial embedded description";
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
fn file_stamps(
    root: &Path,
    files: &BTreeMap<PathBuf, Vec<u8>>,
) -> BTreeMap<PathBuf, (u64, std::time::SystemTime)> {
    files
        .keys()
        .map(|path| {
            let metadata = std::fs::metadata(root.join(path)).unwrap();
            (path.clone(), (metadata.ino(), metadata.modified().unwrap()))
        })
        .collect()
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

fn assert_owned_receipts(root: &Path) {
    use rusqlite::types::Value as Sql;
    let expected_receipt = vec![vec![
        Sql::Text(target(root).to_str().unwrap().to_string()),
        Sql::Text(hash(MEDIA)),
        Sql::Text(checksum(MEDIA)),
    ]];
    assert_eq!(
        rows(
            root,
            "SELECT local_path,local_checksum,checksum FROM assets WHERE library='PrimarySync'"
        ),
        expected_receipt,
        "exact catalogue ownership and checksums"
    );
    assert_eq!(
        rows(
            root,
            "SELECT local_path,local_checksum,provider_checksum FROM asset_metadata_paths WHERE library='PrimarySync'"
        ),
        expected_receipt,
        "exact publication ownership and checksums"
    );
    assert_eq!(
        count(
            root,
            "SELECT count(*) FROM assets WHERE library='PrimarySync' AND (metadata_write_failed_at IS NOT NULL OR capture_repair_metadata_hash IS NOT NULL OR capture_repair_output_checksum IS NOT NULL)"
        ),
        0,
        "repair markers drained"
    );
}

#[derive(Clone, Debug)]
struct Provider {
    records: Vec<Value>,
    hydrations: Arc<AtomicUsize>,
    requests: Arc<AtomicUsize>,
    quiet: bool,
    records_with_token: bool,
    token_records_sent: Arc<std::sync::atomic::AtomicBool>,
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
            let send_even_with_token =
                self.records_with_token && !self.token_records_sent.swap(true, Ordering::SeqCst);
            return Ok(
                json!({"zones":[{"zoneID":{"zoneName":ZONE,"ownerRecordName":"_defaultOwner"},"records":if token.is_none() || send_even_with_token {self.records.clone()} else {Vec::new()},"syncToken":"after","moreComing":false}]}),
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
    // The repair opt-in is enabled only after the initial verified publication.
    // Its changed current configuration must select the damaged rendition;
    // a historical failed asset row alone is not a work scheduler.
    let repair_truncated = root.join("repair-requested").exists();
    cycle_source(root, url, quiet, MEDIA, None, repair_truncated, false, true).await
}

async fn cycle_source(
    root: &Path,
    url: &str,
    quiet: bool,
    source: &[u8],
    description: Option<&str>,
    repair_truncated: bool,
    records_with_token: bool,
    sidecar: bool,
) -> (usize, usize) {
    let mut page = full_album_page_with_download(
        ZONE,
        "death",
        "after",
        url,
        source.len() as u64,
        &checksum(source),
    );
    page["records"][0]["fields"]["filenameEnc"]["value"] = json!("photo.JPG");
    if let Some(description) = description {
        page["records"][1]["fields"]["extendedDescEnc"] =
            json!({"type":"STRING","value":description});
        page["records"][1]["fields"]["captionEnc"] = json!({"type":"STRING","value":""});
        // Provider revisions must identify the emitted fields so an explicit
        // clear is a real observation rather than a duplicate record.
        for record in page["records"].as_array_mut().unwrap() {
            let change_tag = hash(&serde_json::to_vec(&record["fields"]).unwrap());
            record["recordChangeTag"] = json!(change_tag);
        }
    }
    let hydrations = Arc::new(AtomicUsize::new(0));
    let provider = Provider {
        records: page["records"].as_array().unwrap().clone(),
        hydrations: hydrations.clone(),
        requests: Arc::new(AtomicUsize::new(0)),
        quiet,
        records_with_token,
        token_records_sent: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };
    let inner = crate::sync_loop::test_support::open_owned_cycle_db(&root.join("state.db")).await;
    let pass = AlbumPass {
        kind: PassKind::Unfiled,
        album: make_owned_cycle_album(&inner, ZONE, "private", Box::new(provider.clone())),
        exclude_ids: Arc::default(),
    };
    let mut library =
        make_run_cycle_library_state_with_passes(ZONE, "sync_token:PrimarySync", vec![pass]);
    library.library =
        crate::icloud::photos::PhotoLibrary::new_stub_with_zone(Box::new(provider), ZONE);
    let db: Arc<dyn download::DownloadStore> = inner;
    let media = root.join("media");
    #[cfg(feature = "xmp")]
    let xmp_sidecar = sidecar;
    #[cfg(not(feature = "xmp"))]
    let _ = sidecar;
    let base = make_run_cycle_download_config_builder_with_options(
        &media,
        db.clone(),
        RunCycleDownloadConfigOptions {
            concurrent_downloads: Some(1),
            #[cfg(feature = "xmp")]
            xmp_sidecar,
            ..Default::default()
        },
    );
    let build = |mode, excluded, groups, zone| {
        let mut built = base(mode, excluded, groups, zone);
        let config = Arc::make_mut(&mut built);
        config.repair_truncated = repair_truncated;
        config.metadata.set_exif_description = description.is_some();
        built
    };
    let (_session_dir, session) = make_shared_session_for_run_cycle().await;
    let mut config = make_run_cycle_config();
    config.runtime.repair_truncated = repair_truncated;
    config.metadata.set_exif_description = description.is_some();
    let result = Box::pin(crate::sync_cycle::run_cycle(
        &[&library],
        &config,
        Some(db.as_ref()),
        &build,
        download::DownloadControls::download_hidden(),
        &session,
        &CancellationToken::new(),
    ))
    .await
    .unwrap();
    assert_eq!(result.failed_count, 0, "failed cycle: {:?}", result.stats);
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
    let db = crate::sync_loop::test_support::open_owned_cycle_db(&root.join("state.db")).await;
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
            std::fs::write(
                root.path().join("repair-requested"),
                b"explicit repair opt-in",
            )
            .unwrap();
            let db =
                crate::sync_loop::test_support::open_owned_cycle_db(&root.path().join("state.db"))
                    .await;
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
        if persisted == 1 {
            assert_owned_receipts(root.path());
        }
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
        // Both initial publication and explicitly selected repair execute in
        // the current enumeration's tracked run. Only the checkpoint hook is
        // reached after that run has completed.
        let interrupted = point != "checkpoint-persisted";
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
            let db =
                crate::sync_loop::test_support::open_owned_cycle_db(&root.path().join("state.db"))
                    .await;
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
            let db =
                crate::sync_loop::test_support::open_owned_cycle_db(&root.path().join("state.db"))
                    .await;
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
        assert_owned_receipts(root.path());
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
        let stable_stamps = file_stamps(&root.path().join("media"), &stable_files);
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
                file_stamps(&root.path().join("media"), &stable_files),
                stable_stamps,
                "quiet file identity and modification time; no identical metadata rewrite"
            );
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
    let (downloads, hydrations) = cycle(&root, &url, false).await;
    let preserved = count(
        &root,
        "SELECT count(*) FROM provider_active_destinations WHERE preserved_local IS NOT NULL",
    );
    panic!(
        "worker completed without reaching requested death point: downloads={downloads}, hydrations={hydrations}, preserved={preserved}, old_bytes={}",
        std::fs::read(target(&root)).unwrap() == OLD
    );
}

fn metadata_source() -> Vec<u8> {
    let base = include_bytes!("../../../../tests/data/media/pattern.jpg").to_vec();
    let mut metadata = little_exif::metadata::Metadata::new_from_vec(
        &base,
        little_exif::filetype::FileExtension::JPEG,
    )
    .unwrap_or_else(|_| little_exif::metadata::Metadata::new());
    metadata.set_tag(little_exif::exif_tag::ExifTag::Artist(
        "camera artist".into(),
    ));
    let mut output = base;
    metadata
        .write_to_vec(&mut output, little_exif::filetype::FileExtension::JPEG)
        .unwrap();
    output
}

fn native_image_description(path: &Path) -> Option<String> {
    let metadata = little_exif::metadata::Metadata::new_from_path(path).ok()?;
    metadata
        .get_tag(&little_exif::exif_tag::ExifTag::ImageDescription(
            String::new(),
        ))
        .find_map(|tag| match tag {
            little_exif::exif_tag::ExifTag::ImageDescription(value) => Some(value.clone()),
            _ => None,
        })
}

fn native_artist(path: &Path) -> Option<String> {
    let metadata = little_exif::metadata::Metadata::new_from_path(path).ok()?;
    metadata
        .get_tag(&little_exif::exif_tag::ExifTag::Artist(String::new()))
        .find_map(|tag| match tag {
            little_exif::exif_tag::ExifTag::Artist(value) => Some(value.clone()),
            _ => None,
        })
}

#[cfg(feature = "xmp")]
fn expected_initial_fields(path: &Path) -> crate::state::types::EmbeddedMetadataFields {
    let native_description = native_image_description(path).map(|value| {
        let mut bytes = value.into_bytes();
        bytes.push(0);
        data_encoding::HEXLOWER.encode(&bytes)
    });
    crate::state::types::EmbeddedMetadataFields {
        description: Some(CAPTION.to_owned()),
        native_description,
        ..Default::default()
    }
}

#[cfg(not(feature = "xmp"))]
fn expected_initial_fields(path: &Path) -> crate::state::types::EmbeddedMetadataFields {
    let fields = crate::state::types::EmbeddedMetadataFields {
        description: native_image_description(path),
        ..Default::default()
    };
    assert_eq!(
        fields.description.as_deref(),
        Some(CAPTION),
        "native description write"
    );
    fields
}

fn embedded_receipt(
    root: &Path,
    sql: &str,
) -> Option<crate::state::types::EmbeddedMetadataReceipt> {
    let conn = rusqlite::Connection::open(root.join("state.db")).unwrap();
    let mut statement = conn.prepare(sql).unwrap();
    let mut rows = statement.query([]).unwrap();
    match rows.next().unwrap() {
        Some(row) => {
            let raw: Option<String> = row.get(0).unwrap();
            raw.map(|raw| serde_json::from_str(&raw).unwrap())
        }
        None => None,
    }
}

fn assert_metadata_receipts(
    root: &Path,
    expected: &crate::state::types::EmbeddedMetadataReceipt,
    native_root: bool,
) {
    let native = embedded_receipt(
        root,
        "SELECT embedded_metadata_receipt FROM provider_active_destinations \
         WHERE library='PrimarySync' AND asset_id='asset-death' AND version_size='original' \
         AND admitted=1 AND current_publication=1",
    );
    assert_eq!(
        native.as_ref(),
        Some(expected),
        "exact native published receipt"
    );
    let legacy = embedded_receipt(
        root,
        "SELECT embedded_metadata_receipt FROM asset_metadata_paths \
         WHERE library='PrimarySync' AND id='asset-death' AND version_size='original'",
    );
    if native_root {
        // The legacy catalogue path is lossy for a native root. When present it
        // must still agree with the authoritative native receipt.
        if let Some(legacy) = legacy {
            assert_eq!(&legacy, expected, "lossy catalogue receipt diverged");
        }
    } else {
        assert_eq!(
            legacy.as_ref(),
            Some(expected),
            "exact catalogue published receipt"
        );
    }
}

fn assert_downloaded_state(
    root: &Path,
    target: &Path,
    checksum: &str,
    label: &str,
    native_root: bool,
) {
    let conn = rusqlite::Connection::open(root.join("state.db")).unwrap();
    let (status, local_checksum, local_path, last_error): (
        String,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = conn
        .query_row(
            "SELECT status,local_checksum,local_path,last_error FROM assets \
             WHERE library='PrimarySync' AND id='asset-death' AND version_size='original'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(status, "downloaded", "{label}: status");
    assert_eq!(
        local_path.as_deref(),
        Some(target.to_string_lossy().as_ref()),
        "{label}: catalogue path"
    );
    assert_eq!(last_error, None, "{label}: error");
    if native_root {
        // The lossy legacy catalogue path is not authority for a native root.
        let native_checksum: Option<String> = conn
            .query_row(
                "SELECT local_checksum FROM provider_active_destinations \
                 WHERE library='PrimarySync' AND asset_id='asset-death' \
                 AND version_size='original' AND admitted=1 AND current_publication=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            native_checksum.as_deref(),
            Some(checksum),
            "{label}: native checksum"
        );
    } else {
        assert_eq!(
            local_checksum.as_deref(),
            Some(checksum),
            "{label}: checksum"
        );
    }
}

async fn metadata_ownership_case(native_root: bool) {
    let server = wiremock::MockServer::start().await;
    let source = metadata_source();
    Mock::given(method("GET"))
        .and(path("/media"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(source.clone()))
        .mount(&server)
        .await;
    let root = if native_root {
        use std::os::unix::ffi::OsStringExt;
        tempfile::Builder::new()
            .prefix(&std::ffi::OsString::from_vec(
                b"kei-death-metadata-\xff".to_vec(),
            ))
            .tempdir()
            .unwrap()
    } else {
        tempfile::tempdir().unwrap()
    };
    std::fs::create_dir_all(root.path().join("media")).unwrap();
    std::fs::write(
        root.path().join("fixture-owner"),
        b"kei-synthetic-process-death",
    )
    .unwrap();
    std::fs::write(root.path().join("source.jpg"), &source).unwrap();
    let log = std::fs::File::create(root.path().join("worker.log")).unwrap();
    let mut worker = Worker(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", METADATA_WORKER, "--ignored", "--nocapture"])
            .env("KEI_TEST_PROCESS_DEATH_ROOT", root.path())
            .env("KEI_TEST_PROCESS_DEATH_POINT", "download-state-committed")
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
            "worker exited before download-state-committed: {}",
            std::fs::read_to_string(root.path().join("worker.log")).unwrap()
        );
        assert!(
            Instant::now() < deadline,
            "worker timeout at download-state-committed: {}",
            std::fs::read_to_string(root.path().join("worker.log")).unwrap()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        std::fs::read_to_string(root.path().join("ready")).unwrap(),
        "download-state-committed"
    );
    worker.0.kill().unwrap();
    assert_eq!(
        worker.0.wait().unwrap().signal(),
        Some(9),
        "actual SIGKILL without unwinding"
    );

    let target = target(root.path());
    let initial_bytes = std::fs::read(&target).unwrap();
    let initial_checksum = data_encoding::HEXLOWER.encode(&Sha256::digest(&initial_bytes));
    let initial_fields = expected_initial_fields(&target);
    let initial_receipt = crate::state::types::EmbeddedMetadataReceipt {
        published: true,
        input_checksum: initial_checksum.clone(),
        output_checksum: initial_checksum.clone(),
        previous: Default::default(),
        written: initial_fields.clone(),
    };
    assert_eq!(
        count(
            root.path(),
            "SELECT count(*) FROM metadata WHERE key='sync_token:PrimarySync' AND value='after'"
        ),
        0,
        "state commit precedes checkpoint"
    );
    assert_eq!(
        count(
            root.path(),
            "SELECT count(*) FROM provider_active_destinations \
             WHERE library='PrimarySync' AND asset_id='asset-death' \
             AND admitted=1 AND verified_media=1 AND current_publication=1"
        ),
        1,
        "current verified native destination"
    );
    assert_downloaded_state(
        root.path(),
        &target,
        &initial_checksum,
        "after kill",
        native_root,
    );
    assert_metadata_receipts(root.path(), &initial_receipt, native_root);
    assert_eq!(
        native_artist(&target).as_deref(),
        Some("camera artist"),
        "unmanaged native metadata published"
    );
    assert_eq!(
        count(
            root.path(),
            "SELECT count(*) FROM sync_runs WHERE status='running'"
        ),
        1,
        "durable interrupted run"
    );

    {
        let db = crate::sync_loop::test_support::open_owned_cycle_db(&root.path().join("state.db"))
            .await;
        assert_eq!(db.promote_orphaned_sync_runs().await.unwrap(), 1);
    }

    // Settle the interrupted run with unchanged provider metadata first. This
    // advances the checkpoint so the explicit clear arrives as a normal delta
    // rather than a full-inventory mutation of the retained selection.
    let (downloaded, _) = cycle_source(
        root.path(),
        &format!("{}/media", server.uri()),
        false,
        &source,
        Some(CAPTION),
        false,
        false,
        false,
    )
    .await;
    assert_eq!(downloaded, 0, "recovery cannot re-download media");
    assert_eq!(
        count(
            root.path(),
            "SELECT count(*) FROM metadata WHERE key='sync_token:PrimarySync' AND value='after'"
        ),
        1,
        "recovery settles the interrupted checkpoint"
    );
    let pre_clear_bytes = std::fs::read(&target).unwrap();
    let pre_clear_checksum = data_encoding::HEXLOWER.encode(&Sha256::digest(&pre_clear_bytes));
    assert_downloaded_state(
        root.path(),
        &target,
        &pre_clear_checksum,
        "after recovery",
        native_root,
    );
    assert_eq!(
        native_artist(&target).as_deref(),
        Some("camera artist"),
        "recovery preserves unmanaged metadata"
    );

    // The explicit provider clear is delivered as a delta after recovery.
    let (downloaded, _) = cycle_source(
        root.path(),
        &format!("{}/media", server.uri()),
        false,
        &source,
        Some(""),
        false,
        true,
        false,
    )
    .await;
    assert_eq!(downloaded, 0, "provider clear cannot re-download media");
    assert_eq!(
        count(
            root.path(),
            "SELECT count(*) FROM metadata WHERE key='sync_token:PrimarySync' AND value='after'"
        ),
        1,
        "clear retains the settled checkpoint"
    );

    let clear_bytes = std::fs::read(&target).unwrap();
    let clear_checksum = data_encoding::HEXLOWER.encode(&Sha256::digest(&clear_bytes));
    let clear_receipt = crate::state::types::EmbeddedMetadataReceipt {
        published: true,
        input_checksum: pre_clear_checksum,
        output_checksum: clear_checksum.clone(),
        previous: initial_fields,
        written: Default::default(),
    };
    assert_downloaded_state(
        root.path(),
        &target,
        &clear_checksum,
        "after clear",
        native_root,
    );
    assert_metadata_receipts(root.path(), &clear_receipt, native_root);
    assert_eq!(
        native_artist(&target).as_deref(),
        Some("camera artist"),
        "clear removes only the managed description"
    );
    assert!(
        native_image_description(&target).is_none(),
        "managed native description removed"
    );
    if native_root {
        // The lossy legacy catalogue row cannot own this root's completion.
        // Assert native authority; the legacy marker may remain as lossy-path
        // debt without re-driving the native file.
        assert_eq!(
            count(
                root.path(),
                "SELECT count(*) FROM provider_active_destinations \
                 WHERE library='PrimarySync' AND asset_id='asset-death' \
                 AND admitted=1 AND current_publication=1 \
                 AND verified_metadata=1 AND prepared_hash IS NULL"
            ),
            1,
            "native metadata completion"
        );
    } else {
        assert_eq!(
            count(
                root.path(),
                "SELECT count(*) FROM assets WHERE library='PrimarySync' AND id='asset-death' \
                 AND metadata_write_failed_at IS NULL"
            ),
            1,
            "clear retires its marker"
        );
    }
    assert_eq!(
        count(root.path(), "SELECT count(*) FROM metadata_capture_retries"),
        0
    );

    let stable_mtime = std::fs::metadata(&target).unwrap().modified().unwrap();
    for _ in 0..2 {
        let (downloaded, hydrations) = cycle_source(
            root.path(),
            &format!("{}/media", server.uri()),
            true,
            &source,
            Some(""),
            false,
            false,
            false,
        )
        .await;
        assert_eq!((downloaded, hydrations), (0, 0), "quiet reopened cycle");
        assert_eq!(std::fs::read(&target).unwrap(), clear_bytes, "quiet bytes");
        assert_eq!(
            std::fs::metadata(&target).unwrap().modified().unwrap(),
            stable_mtime,
            "quiet metadata rewrite"
        );
        assert_metadata_receipts(root.path(), &clear_receipt, native_root);
        assert_eq!(
            native_artist(&target).as_deref(),
            Some("camera artist"),
            "quiet unmanaged metadata"
        );
        assert_eq!(
            count(
                root.path(),
                "SELECT count(*) FROM metadata WHERE key='sync_token:PrimarySync' AND value='after'"
            ),
            1
        );
    }
}

#[tokio::test]
async fn initial_download_embedded_description_ownership_survives_state_commit_sigkill_and_provider_clear()
 {
    metadata_ownership_case(false).await;
    // The default toolkit cannot open a non-UTF path even when native EXIF is
    // enabled; the native-only build can.
    #[cfg(not(feature = "xmp"))]
    {
        metadata_ownership_case(true).await;
    }
}

#[tokio::test]
#[ignore = "dedicated synthetic subprocess; parent kills it at a fixed durable point"]
async fn metadata_ownership_process_death_worker() {
    let root =
        PathBuf::from(std::env::var_os("KEI_TEST_PROCESS_DEATH_ROOT").expect("parent fixture"));
    let url = std::env::var("KEI_TEST_PROCESS_DEATH_URL").unwrap();
    let source = std::fs::read(root.join("source.jpg")).unwrap();
    cycle_source(
        &root,
        &url,
        false,
        &source,
        Some(CAPTION),
        false,
        false,
        false,
    )
    .await;
    panic!("worker completed without reaching requested death point");
}
