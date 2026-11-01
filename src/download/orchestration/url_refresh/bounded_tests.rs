//! Synthetic provider/CDN regressions. No authenticated provider data is used.
use super::super::incremental::download_photos_incremental_collecting_inner;
use super::super::models::{DownloadControls, DownloadOutcome};
use super::super::test_support::{
    album_with_session, changes_zone_response, incremental_photo_records_with_url, retry_test_task,
    test_config,
};
use crate::commands::{AlbumPass, PassKind};
use crate::download::DownloadReporting;
use crate::download::pipeline::{MetadataFlags, PassConfig, run_download_pass};
use crate::icloud::photos::PhotosSession;
use crate::retry::RetryConfig;
use crate::state::{SqliteStateDb, VersionSizeKey};
use crate::test_helpers::TestAssetRecord;
use rustc_hash::FxHashSet;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

const BYTES: &[u8] = &[0xff, 0xd8, 0xff, 0xe0, 0, 0x10, 0x4a, 0x46];

#[derive(Debug, Clone)]
struct BoundedSession {
    delta: Arc<Vec<Value>>,
    fresh: Arc<Vec<Value>>,
    requests: Arc<Mutex<Vec<Value>>>,
}
#[async_trait::async_trait]
impl PhotosSession for BoundedSession {
    async fn post(
        &self,
        url: &str,
        body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        let request: Value = serde_json::from_str(&body)?;
        self.requests.lock().unwrap().push(request.clone());
        if url.contains("/changes/zone?") {
            assert!(
                request["zones"][0].get("syncToken").is_some(),
                "URL refresh must not enumerate the zone"
            );
            return Ok(changes_zone_response(self.delta.as_ref().clone(), "next"));
        }
        assert!(
            url.contains("/records/lookup?"),
            "refresh must use exact lookup"
        );
        assert_eq!(request["zoneID"]["zoneName"], "PrimarySync");
        let names: FxHashSet<_> = request["records"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["recordName"].as_str().unwrap())
            .collect();
        assert!(names.len() <= 4, "bounded by requested tasks");
        Ok(
            json!({"records": self.fresh.iter().filter(|r| names.contains(r["recordName"].as_str().unwrap())).cloned().collect::<Vec<_>>()}),
        )
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

#[tokio::test]
async fn persistent_expiry_does_not_starve_delayed_healthy_peer() {
    let (trace, _guard) = crate::test_helpers::TracingCapture::install();
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};
    let server = crate::start_wiremock_or_skip!();
    Mock::given(method("GET"))
        .and(path("/poison"))
        .respond_with(ResponseTemplate::new(410))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/healthy"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(BYTES)
                .set_delay(Duration::from_millis(150)),
        )
        .expect(1)
        .mount(&server)
        .await;
    let dir = TempDir::new().unwrap();
    let state_path = dir.path().join("state.db");
    let db = Arc::new(SqliteStateDb::open(&state_path).await.unwrap());
    let mut tasks = Vec::new();
    for name in ["poison", "healthy"] {
        let record = TestAssetRecord::new(name)
            .filename(&format!("{name}.jpg"))
            .checksum("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
            .size(8)
            .build();
        db.upsert_seen(&record).await.unwrap();
        let mut task = retry_test_task(name, VersionSizeKey::Original, "unused");
        task.url = format!("{}/{name}", server.uri()).into();
        task.download_path = dir.path().join(format!("{name}.jpg"));
        task.size = 8;
        tasks.push(task);
    }
    std::fs::write(dir.path().join("unrelated.jpg"), b"untouched").unwrap();
    let client = reqwest::Client::new();
    let retry = RetryConfig {
        max_retries: 0,
        base_delay_secs: 0,
        max_delay_secs: 0,
    };
    let result = run_download_pass(
        PassConfig {
            prior_auth_errors: 0,
            url_obtained_at: Default::default(),
            client: &client,
            retry_config: &retry,
            metadata: MetadataFlags::default(),
            mark_capture_repair_after_download: false,
            concurrency: 2,
            reporting: DownloadReporting::hidden(),
            temp_suffix: ".part".into(),
            shutdown_token: CancellationToken::new(),
            state_db: Some(db.clone()),
            rate_limit_counter: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            bandwidth_limiter: None,
            library: "PrimarySync".into(),
        },
        tasks,
    )
    .await;
    assert_eq!(
        result.downloaded,
        1,
        "persistent 410 must not cancel the healthy peer: {result:?}, events={:?}",
        trace.events()
    );
    assert_eq!(result.failed.len(), 1);
    assert_eq!(result.failed[0].asset_id.as_ref(), "poison");
    drop(db);
    let reopened = SqliteStateDb::open(&state_path).await.unwrap();
    let summary = reopened.get_summary().await.unwrap();
    assert_eq!(summary.downloaded, 1);
    assert_eq!(summary.failed + summary.pending, 1);
    assert_eq!(
        std::fs::read(dir.path().join("healthy.jpg")).unwrap(),
        BYTES
    );
    assert_eq!(
        std::fs::read(dir.path().join("unrelated.jpg")).unwrap(),
        b"untouched"
    );
    assert!(!dir.path().join("poison.jpg").exists());
    server.verify().await;
}

#[tokio::test]
async fn incremental_refresh_is_bounded_and_persistent_expiry_keeps_durable_debt() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};
    let server = crate::start_wiremock_or_skip!();
    Mock::given(method("GET"))
        .and(path("/poison"))
        .respond_with(ResponseTemplate::new(410))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/old-healthy"))
        .respond_with(ResponseTemplate::new(410))
        .expect(0)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/healthy"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(BYTES)
                .set_delay(Duration::from_millis(150)),
        )
        .expect(1)
        .mount(&server)
        .await;
    let mut delta = incremental_photo_records_with_url(
        "POISON",
        "poison.jpg",
        &format!("{}/poison", server.uri()),
        8,
    );
    delta.extend(incremental_photo_records_with_url(
        "HEALTHY",
        "healthy.jpg",
        &format!("{}/old-healthy", server.uri()),
        8,
    ));
    let mut fresh = delta.clone();
    fresh[2]["fields"]["resOriginalRes"]["value"]["downloadURL"] =
        json!(format!("{}/healthy", server.uri()));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let passes = vec![AlbumPass {
        kind: PassKind::Unfiled,
        album: album_with_session(
            "PrimarySync",
            "",
            Box::new(BoundedSession {
                delta: Arc::new(delta),
                fresh: Arc::new(fresh),
                requests: requests.clone(),
            }),
        ),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];
    let dir = TempDir::new().unwrap();
    let state_path = dir.path().join("state.db");
    let db = Arc::new(SqliteStateDb::open(&state_path).await.unwrap());
    let mut config = test_config();
    config.directory = Arc::from(dir.path().join("media"));
    config.folder_structure = String::new();
    config.state_db = Some(db.clone());
    config.concurrent_downloads = 2;
    let config = Arc::new(config);
    let result = download_photos_incremental_collecting_inner(
        &reqwest::Client::new(),
        &passes,
        &config,
        "previous",
        DownloadControls::download_hidden(),
        CancellationToken::new(),
        Duration::ZERO,
    )
    .await
    .unwrap();
    assert!(matches!(
        result.outcome,
        DownloadOutcome::PartialFailure { failed_count: 1 }
    ));
    assert_eq!(result.stats.downloaded, 1);
    assert_eq!(result.stats.failed, 1);
    assert!(!result.stats.interrupted);
    let downloaded = db.get_downloaded_page(0, 10).await.unwrap();
    assert_eq!(downloaded.len(), 1);
    assert_eq!(
        downloaded[0].local_path.as_ref().unwrap(),
        &config.directory.join("healthy.JPG")
    );
    assert_eq!(
        std::fs::read(downloaded[0].local_path.as_ref().unwrap()).unwrap(),
        BYTES
    );
    assert!(!config.directory.join("poison.JPG").exists());
    let calls = requests.lock().unwrap().clone();
    assert_eq!(
        calls.len(),
        5,
        "one delta plus two bounded asset/master lookups per refresh"
    );
    assert_eq!(
        calls[3]["records"].as_array().unwrap().len(),
        1,
        "healthy completed work is not refreshed again"
    );
    drop(config);
    drop(db);
    let reopened = SqliteStateDb::open(&state_path).await.unwrap();
    let summary = reopened.get_summary().await.unwrap();
    assert_eq!(summary.downloaded, 1);
    assert_eq!(summary.failed + summary.pending, 1);
    server.verify().await;
}

#[tokio::test]
async fn incremental_refresh_refuses_changed_or_missing_resources_and_recovers_after_restart() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};
    for fault in [
        "checksum",
        "size",
        "master",
        "child",
        "owner",
        "zone",
        "missing",
        "rendition",
    ] {
        let server = crate::start_wiremock_or_skip!();
        Mock::given(method("GET"))
            .and(path("/old"))
            .respond_with(ResponseTemplate::new(410))
            .expect(0)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/new"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(BYTES))
            .expect(1)
            .mount(&server)
            .await;
        let delta = incremental_photo_records_with_url(
            "RESOURCE",
            "resource.jpg",
            &format!("{}/old", server.uri()),
            8,
        );
        let mut fresh = delta.clone();
        fresh[0]["fields"]["resOriginalRes"]["value"]["downloadURL"] =
            json!(format!("{}/new", server.uri()));
        let correct = fresh.clone();
        match fault {
            "checksum" => {
                fresh[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!("changed")
            }
            "size" => fresh[0]["fields"]["resOriginalRes"]["value"]["size"] = json!(9),
            "master" => {
                fresh[0]["recordName"] = json!("MOVED");
                fresh[1]["fields"]["masterRef"]["value"]["recordName"] = json!("MOVED");
            }
            "child" => fresh[1]["recordName"] = json!("other-child"),
            "owner" => {
                fresh[1]["fields"]["masterRef"]["value"]["zoneID"] =
                    json!({"zoneName":"PrimarySync", "ownerRecordName":"another-account"})
            }
            "zone" => {
                fresh[0]["zoneID"] =
                    json!({"zoneName":"SharedSync-other", "ownerRecordName":"_defaultOwner"})
            }
            "missing" => fresh.clear(),
            _ => {
                assert_eq!(fault, "rendition");
                fresh[0]["fields"]
                    .as_object_mut()
                    .unwrap()
                    .remove("resOriginalRes");
            }
        }
        let dir = TempDir::new().unwrap();
        let state_path = dir.path().join("state.db");
        let sentinel = dir.path().join("media/unrelated.jpg");
        std::fs::create_dir_all(sentinel.parent().unwrap()).unwrap();
        std::fs::write(&sentinel, b"existing media").unwrap();
        let mut db = Arc::new(SqliteStateDb::open(&state_path).await.unwrap());
        let record = TestAssetRecord::new("asset-RESOURCE")
            .filename("resource.jpg")
            .size(8)
            .build();
        db.upsert_seen(&record).await.unwrap();
        let mut config = test_config();
        config.directory = Arc::from(dir.path().join("media"));
        config.folder_structure = String::new();
        config.state_db = Some(db.clone());
        let mut config = Arc::new(config);
        let requests = Arc::new(Mutex::new(Vec::new()));
        let passes = |delta: Vec<Value>, fresh: Vec<Value>| {
            vec![AlbumPass {
                kind: PassKind::Unfiled,
                album: album_with_session(
                    "PrimarySync",
                    "",
                    Box::new(BoundedSession {
                        delta: Arc::new(delta),
                        fresh: Arc::new(fresh),
                        requests: requests.clone(),
                    }),
                ),
                exclude_ids: Arc::new(FxHashSet::default()),
            }]
        };
        let client = reqwest::Client::new();
        let refused = download_photos_incremental_collecting_inner(
            &client,
            &passes(delta.clone(), fresh),
            &config,
            "previous",
            DownloadControls::download_hidden(),
            CancellationToken::new(),
            Duration::ZERO,
        )
        .await
        .unwrap();
        assert_eq!(refused.stats.downloaded, 0, "{fault}");
        assert_eq!(refused.stats.failed, 1, "{fault}");
        assert!(!config.directory.join("resource.JPG").exists(), "{fault}");
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"existing media");
        drop(config);
        drop(db);
        db = Arc::new(SqliteStateDb::open(&state_path).await.unwrap());
        assert_eq!(db.get_summary().await.unwrap().failed, 1, "{fault}");
        let mut cfg = test_config();
        cfg.directory = Arc::from(dir.path().join("media"));
        cfg.folder_structure = String::new();
        cfg.state_db = Some(db.clone());
        config = Arc::new(cfg);
        let recovered = download_photos_incremental_collecting_inner(
            &client,
            &passes(delta, correct),
            &config,
            "previous",
            DownloadControls::download_hidden(),
            CancellationToken::new(),
            Duration::ZERO,
        )
        .await
        .unwrap();
        assert!(
            matches!(recovered.outcome, DownloadOutcome::Success),
            "{fault}"
        );
        assert_eq!(recovered.stats.downloaded, 1, "{fault}");
        let downloaded = db.get_downloaded_page(0, 10).await.unwrap();
        assert_eq!(downloaded.len(), 1);
        assert_eq!(
            downloaded[0].local_path.as_ref().unwrap(),
            &config.directory.join("resource.JPG")
        );
        assert_eq!(
            std::fs::read(downloaded[0].local_path.as_ref().unwrap()).unwrap(),
            BYTES
        );
        let count = requests.lock().unwrap().len();
        let quiet = download_photos_incremental_collecting_inner(
            &client,
            &passes(Vec::new(), Vec::new()),
            &config,
            "next",
            DownloadControls::download_hidden(),
            CancellationToken::new(),
            Duration::ZERO,
        )
        .await
        .unwrap();
        assert!(matches!(quiet.outcome, DownloadOutcome::Success));
        assert_eq!(quiet.stats.downloaded, 0);
        assert_eq!(
            requests.lock().unwrap().len(),
            count + 1,
            "quiet tail has only delta, no refresh"
        );
        drop(config);
        drop(db);
        let reopened = SqliteStateDb::open(&state_path).await.unwrap();
        let summary = reopened.get_summary().await.unwrap();
        assert_eq!(summary.downloaded, 1);
        assert_eq!(summary.failed + summary.pending, 0);
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"existing media");
        server.verify().await;
    }
}

#[tokio::test]
async fn cancelled_explicit_dispatch_reports_and_persists_every_unfinished_task() {
    use wiremock::matchers::method;
    use wiremock::{Mock, ResponseTemplate};
    let server = crate::start_wiremock_or_skip!();
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(BYTES)
                .set_delay(Duration::from_secs(60)),
        )
        .mount(&server)
        .await;
    let dir = TempDir::new().unwrap();
    let state_path = dir.path().join("state.db");
    let db = Arc::new(SqliteStateDb::open(&state_path).await.unwrap());
    let mut tasks = Vec::new();
    for name in ["inflight", "queued"] {
        db.upsert_seen(
            &TestAssetRecord::new(name)
                .filename(&format!("{name}.jpg"))
                .size(8)
                .build(),
        )
        .await
        .unwrap();
        let mut task = retry_test_task(name, VersionSizeKey::Original, "unused");
        task.url = server.uri().into();
        task.download_path = dir.path().join(format!("{name}.jpg"));
        task.size = 8;
        tasks.push(task);
    }
    let shutdown = CancellationToken::new();
    let client = reqwest::Client::new();
    let retry = RetryConfig {
        max_retries: 0,
        base_delay_secs: 0,
        max_delay_secs: 0,
    };
    let cancel = async {
        loop {
            if !server.received_requests().await.unwrap().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
        shutdown.cancel();
    };
    let worker = run_download_pass(
        PassConfig {
            prior_auth_errors: 0,
            url_obtained_at: Default::default(),
            client: &client,
            retry_config: &retry,
            metadata: MetadataFlags::default(),
            mark_capture_repair_after_download: false,
            concurrency: 1,
            reporting: DownloadReporting::hidden(),
            temp_suffix: ".part".into(),
            shutdown_token: shutdown.clone(),
            state_db: Some(db.clone()),
            rate_limit_counter: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            bandwidth_limiter: None,
            library: "PrimarySync".into(),
        },
        tasks,
    );
    let (result, ()) = tokio::time::timeout(Duration::from_secs(3), async {
        tokio::join!(worker, cancel)
    })
    .await
    .expect("shutdown must interrupt an active transfer promptly");
    assert_eq!(result.downloaded, 0);
    assert_eq!(result.failed.len(), 2);
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        1,
        "queued task must not reach CDN"
    );
    drop(db);
    let reopened = SqliteStateDb::open(&state_path).await.unwrap();
    assert_eq!(reopened.get_summary().await.unwrap().failed, 2);
    assert!(!dir.path().join("inflight.jpg").exists());
    assert!(!dir.path().join("queued.jpg").exists());
}

#[derive(Debug, Clone)]
struct RefreshFailureSession {
    inner: BoundedSession,
    blocking: bool,
    calls: Arc<std::sync::atomic::AtomicUsize>,
}
#[async_trait::async_trait]
impl PhotosSession for RefreshFailureSession {
    async fn post(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        if url.contains("/records/lookup?") {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.blocking {
                return std::future::pending().await;
            }
            return Err(crate::icloud::photos::session::HttpStatusError {
                status: 401,
                url: url.into(),
                body: Some("synthetic-auth-error".into()),
                retry_after: None,
            }
            .into());
        }
        self.inner.post(url, body, headers).await
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

#[tokio::test]
async fn refresh_authentication_and_shutdown_remain_authoritative() {
    use wiremock::matchers::method;
    use wiremock::{Mock, ResponseTemplate};
    for (blocking, preflight) in [(false, true), (false, false), (true, true)] {
        let server = crate::start_wiremock_or_skip!();
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(410))
            .expect(if preflight { 0 } else { 1 })
            .mount(&server)
            .await;
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("state.db");
        let db = Arc::new(SqliteStateDb::open(&db_path).await.unwrap());
        let delta = incremental_photo_records_with_url("AUTH", "auth.jpg", &server.uri(), 8);
        let lookup_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: album_with_session(
                "PrimarySync",
                "",
                Box::new(RefreshFailureSession {
                    inner: BoundedSession {
                        delta: Arc::new(delta),
                        fresh: Arc::new(Vec::new()),
                        requests: Arc::new(Mutex::new(Vec::new())),
                    },
                    blocking,
                    calls: lookup_calls.clone(),
                }),
            ),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];
        let mut cfg = test_config();
        cfg.directory = Arc::from(dir.path().join("media"));
        cfg.folder_structure = String::new();
        cfg.state_db = Some(db.clone());
        let cfg = Arc::new(cfg);
        let shutdown = CancellationToken::new();
        let cancel = async {
            if blocking {
                while lookup_calls.load(std::sync::atomic::Ordering::SeqCst) == 0 {
                    tokio::task::yield_now().await;
                }
                shutdown.cancel();
            }
        };
        let client = reqwest::Client::new();
        let worker = download_photos_incremental_collecting_inner(
            &client,
            &passes,
            &cfg,
            "previous",
            DownloadControls::download_hidden(),
            shutdown.clone(),
            if preflight {
                Duration::ZERO
            } else {
                Duration::from_secs(300)
            },
        );
        let (result, ()) = Box::pin(tokio::time::timeout(Duration::from_secs(3), async {
            tokio::join!(worker, cancel)
        }))
        .await
        .expect("cancel a pending lookup promptly");
        let result = result.unwrap();
        assert!(result.stats.interrupted);
        assert_eq!(result.stats.failed, 1);
        if !blocking {
            assert!(matches!(
                result.outcome,
                DownloadOutcome::SessionExpired {
                    auth_error_count: 1
                }
            ));
        }
        assert_eq!(lookup_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(!cfg.directory.join("auth.jpg").exists());
        drop(cfg);
        drop(db);
        let reopened = SqliteStateDb::open(&db_path).await.unwrap();
        let summary = reopened.get_summary().await.unwrap();
        assert_eq!(summary.failed + summary.pending, 1);
        server.verify().await;
    }
}

#[tokio::test]
async fn targeted_refresh_preserves_selection_and_no_overwrite_publication() {
    use super::{RetryTaskKey, UrlRetrySource, build_incremental_expired_url_retry_tasks};
    use base64::Engine as _;
    use sha2::{Digest as _, Sha256};
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // A URL refresh must preserve each original publication mode. An
    // occupied foreign file is never authority to finalize a download.
    for (require_created_entry, occupied) in
        [(false, true), (true, true), (false, false), (true, false)]
    {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(BYTES))
            .expect(1)
            .mount(&server)
            .await;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("selected-path.jpg");
        let foreign = b"external file edit";
        if occupied {
            std::fs::write(&path, foreign).unwrap();
        }
        let foreign_mtime = occupied.then(|| std::fs::metadata(&path).unwrap().modified().unwrap());
        #[cfg(unix)]
        let foreign_identity = {
            use std::os::unix::fs::MetadataExt as _;
            occupied.then(|| {
                let metadata = std::fs::metadata(&path).unwrap();
                (metadata.dev(), metadata.ino())
            })
        };
        let db_path = dir.path().join("state.db");
        let owner = crate::state::db::account::AccountOwner::authenticated(
            "retry-fixture@example.invalid",
            "com",
            &serde_json::from_value(json!({"dsInfo":{"dsid":"retry-fixture-provider"}})).unwrap(),
        )
        .unwrap();
        let wrong_owner = crate::state::db::account::AccountOwner::authenticated(
            "different@example.invalid",
            "com",
            &serde_json::from_value(json!({"dsInfo":{"dsid":"different-provider"}})).unwrap(),
        )
        .unwrap();
        let db = Arc::new(SqliteStateDb::open_owned(&db_path, &owner).await.unwrap());
        assert!(matches!(
            SqliteStateDb::open_owned(&db_path, &wrong_owner).await,
            Err(crate::state::error::StateError::AccountOwnerMismatch)
        ));
        let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(BYTES));
        let new_url = server.uri();
        let mut records = incremental_photo_records_with_url(
            "MASTER",
            "different-provider-name.jpg",
            &new_url,
            BYTES.len() as u64,
        );
        records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(checksum);
        for record in &mut records {
            record["zoneID"] = json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"});
        }
        records[1]["fields"]["masterRef"]["value"]["zoneID"] =
            json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"});
        let mut task = retry_test_task("child-state", VersionSizeKey::Original, "unused");
        task.require_created_entry = require_created_entry;
        task.asset_record_name = "asset-MASTER".into();
        task.download_path = path.clone();
        task.size = BYTES.len() as u64;
        task.checksum = checksum.clone().into();
        assert!(task.replacement_fingerprint.is_none());
        if require_created_entry {
            assert!(matches!(
                task.publication(),
                crate::download::file::FinalPublication::CreateOnly
            ));
        } else {
            assert!(matches!(
                task.publication(),
                crate::download::file::FinalPublication::NoReplace
            ));
        }
        db.upsert_seen(
            &TestAssetRecord::new("child-state")
                .filename("selected-path.jpg")
                .checksum(&checksum)
                .size(BYTES.len() as u64)
                .build(),
        )
        .await
        .unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let mut album = crate::icloud::photos::PhotoAlbum::new(
            crate::icloud::photos::PhotoAlbumConfig {
                params: Arc::default(),
                service_endpoint: Arc::from("https://example.com"),
                name: Arc::from(""),
                list_type: Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
                obj_type: Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
                query_filter: None,
                page_size: 100,
                zone_id: Arc::new(
                    json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"}),
                ),
                retry_config: RetryConfig::default(),
                container_id: None,
                cross_zone_sources: Vec::new(),
            },
            Box::new(BoundedSession {
                delta: Arc::new(Vec::new()),
                fresh: Arc::new(records),
                requests: requests.clone(),
            }),
        );
        album.set_shadow_capture(
            crate::icloud::photos::inbox::ShadowCapture::new(db.clone(), owner.clone(), "com"),
            Arc::from("private"),
        );
        let (capture, scope, zone) = album.download_scope().unwrap().expect("scoped retry owner");
        assert!(Arc::ptr_eq(&capture.db, &db));
        assert!(capture.owner == owner);
        assert_eq!(
            zone,
            json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"})
        );
        db.validate_download_scope(owner.clone(), scope, "PrimarySync".to_owned())
            .await
            .unwrap();
        drop(capture);
        let passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album,
            exclude_ids: Arc::new(FxHashSet::default()),
        }];
        let mut sources = rustc_hash::FxHashMap::default();
        sources.insert(
            RetryTaskKey::from(&task),
            UrlRetrySource {
                asset_record_name: task.asset_record_name.clone(),
                master_record_name: "MASTER".into(),
                provider_version: VersionSizeKey::Original,
                pass_index: 0,
                recorded_pass: None,
            },
        );
        let plan = build_incremental_expired_url_retry_tasks(
            &passes,
            &sources,
            &[task.clone()],
            CancellationToken::new(),
        )
        .await;
        assert_eq!(plan.tasks.len(), 1);
        let refreshed = &plan.tasks[0];
        assert_eq!(refreshed.url.as_ref(), new_url);
        assert_ne!(refreshed.url, task.url);
        assert_eq!(refreshed.download_path, path);
        assert_eq!(refreshed.asset_id.as_ref(), "child-state");
        assert_eq!(refreshed.asset_record_name.as_ref(), "asset-MASTER");
        assert_eq!(refreshed.created_local, task.created_local);
        assert_eq!(refreshed.checksum, task.checksum);
        assert_eq!(refreshed.size, task.size);
        assert_eq!(refreshed.version_size, task.version_size);
        assert_eq!(refreshed.require_created_entry, require_created_entry);
        assert!(refreshed.replacement_fingerprint.is_none());
        assert!(Arc::ptr_eq(&refreshed.metadata, &task.metadata));
        if require_created_entry {
            assert!(matches!(
                refreshed.publication(),
                crate::download::file::FinalPublication::CreateOnly
            ));
        } else {
            assert!(matches!(
                refreshed.publication(),
                crate::download::file::FinalPublication::NoReplace
            ));
        }
        let looked_up: Vec<String> = requests
            .lock()
            .unwrap()
            .iter()
            .flat_map(|request| request["records"].as_array().into_iter().flatten())
            .filter_map(|record| record["recordName"].as_str().map(str::to_owned))
            .collect();
        assert!(looked_up.iter().any(|name| name == "asset-MASTER"));
        assert!(looked_up.iter().any(|name| name == "MASTER"));
        let client = reqwest::Client::new();
        let retry = RetryConfig {
            max_retries: 0,
            base_delay_secs: 0,
            max_delay_secs: 0,
        };
        let result = run_download_pass(
            PassConfig {
                prior_auth_errors: 0,
                url_obtained_at: plan.url_obtained_at,
                client: &client,
                retry_config: &retry,
                metadata: MetadataFlags::default(),
                mark_capture_repair_after_download: false,
                concurrency: 1,
                reporting: DownloadReporting::hidden(),
                temp_suffix: ".part".into(),
                shutdown_token: CancellationToken::new(),
                state_db: Some(db.clone()),
                rate_limit_counter: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                bandwidth_limiter: None,
                library: "PrimarySync".into(),
            },
            plan.tasks,
        )
        .await;
        if occupied {
            assert_eq!(result.downloaded, 0);
            assert_eq!(result.failed.len(), 1);
            assert_eq!(result.failed[0].asset_id.as_ref(), "child-state");
            assert_eq!(result.failed[0].version_size, VersionSizeKey::Original);
            assert_eq!(std::fs::read(&path).unwrap(), foreign);
            assert_eq!(
                std::fs::metadata(&path).unwrap().modified().unwrap(),
                foreign_mtime.unwrap()
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt as _;
                let metadata = std::fs::metadata(&path).unwrap();
                assert_eq!(Some((metadata.dev(), metadata.ino())), foreign_identity);
            }
        } else {
            assert_eq!(result.downloaded, 1);
            assert!(result.failed.is_empty());
            assert_eq!(std::fs::read(&path).unwrap(), BYTES);
        }
        assert_eq!(result.state_write_failures, 0);
        drop(passes);
        let closed_owner = Arc::downgrade(&db);
        assert_eq!(
            Arc::strong_count(&db),
            1,
            "all scoped capture and pass handles must be closed"
        );
        drop(db);
        assert!(
            closed_owner.upgrade().is_none(),
            "owner must close before reopening"
        );
        let reopened = SqliteStateDb::open_owned(&db_path, &owner).await.unwrap();
        {
            let conn = reopened.acquire_lock("retry publication evidence").unwrap();
            let (status, last_error): (String, Option<String>) = conn.query_row(
            "SELECT status,last_error FROM assets WHERE library='PrimarySync' AND id='child-state' AND version_size='original' AND checksum=?1",
            [&checksum], |row| Ok((row.get(0)?, row.get(1)?)),
        ).unwrap();
            let path_rows: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM asset_metadata_paths WHERE local_path=?1",
                    [path.to_str().unwrap()],
                    |row| row.get(0),
                )
                .unwrap();
            let receipts: i64 = conn
                .query_row("SELECT COUNT(*) FROM download_receipts", [], |row| {
                    row.get(0)
                })
                .unwrap();
            if occupied {
                assert_eq!(status, "failed");
                let error = last_error.expect("foreign collision must remain visible");
                if require_created_entry {
                    assert!(
                        error.contains("Destination already exists"),
                        "unexpected create-only error: {error}"
                    );
                } else {
                    assert!(
                        error.contains("Refusing to deduplicate"),
                        "unexpected no-replace error: {error}"
                    );
                }
                assert_eq!(
                    path_rows, 0,
                    "foreign file must not acquire any path ownership"
                );
                assert_eq!(
                    receipts, 0,
                    "foreign file must not acquire any compact receipt"
                );
            } else {
                assert_eq!(status, "downloaded");
                assert_eq!(path_rows, 1);
                let (library, id, version, provider, local, source): (String,String,String,String,String,String) =
                conn.query_row(
                    "SELECT library,id,version_size,provider_checksum,local_checksum,download_checksum FROM asset_metadata_paths WHERE local_path=?1",
                    [path.to_str().unwrap()], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?)),
                ).unwrap();
                let digest = data_encoding::HEXLOWER.encode(&Sha256::digest(BYTES));
                assert_eq!(
                    (library.as_str(), id.as_str(), version.as_str()),
                    ("PrimarySync", "child-state", "original")
                );
                assert_eq!(provider, checksum);
                assert_eq!(local, digest);
                assert_eq!(source, digest);
            }
        }
        server.verify().await;
        eprintln!("URL_RETRY_REACHED create_only={require_created_entry} occupied={occupied}");
    }
}

#[tokio::test]
async fn failed_refresh_state_write_halts_dispatch_and_preserves_restart_work() {
    use wiremock::matchers::method;
    use wiremock::{Mock, ResponseTemplate};
    let server = crate::start_wiremock_or_skip!();
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(BYTES))
        .expect(0)
        .mount(&server)
        .await;
    let mut delta = incremental_photo_records_with_url("REFUSED", "refused.jpg", &server.uri(), 8);
    let healthy = incremental_photo_records_with_url("HEALTHY", "healthy.jpg", &server.uri(), 8);
    delta.extend(healthy.clone());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let passes = vec![AlbumPass {
        kind: PassKind::Unfiled,
        album: album_with_session(
            "PrimarySync",
            "",
            Box::new(BoundedSession {
                delta: Arc::new(delta),
                fresh: Arc::new(healthy),
                requests,
            }),
        ),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("state.db");
    let db = Arc::new(SqliteStateDb::open(&db_path).await.unwrap());
    db.acquire_lock("inject failure persistence error")
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER deny_failure BEFORE UPDATE OF status ON assets
         WHEN NEW.status = 'failed' AND NEW.id = 'asset-REFUSED'
         BEGIN SELECT RAISE(ABORT, 'synthetic failure write refused'); END;",
        )
        .unwrap();
    let mut cfg = test_config();
    cfg.directory = Arc::from(dir.path().join("media"));
    cfg.folder_structure = String::new();
    cfg.state_db = Some(db.clone());
    let cfg = Arc::new(cfg);
    let result = download_photos_incremental_collecting_inner(
        &reqwest::Client::new(),
        &passes,
        &cfg,
        "previous",
        DownloadControls::download_hidden(),
        CancellationToken::new(),
        Duration::ZERO,
    )
    .await
    .unwrap();
    assert_eq!(result.stats.downloaded, 0);
    assert_eq!(result.stats.failed, 2);
    assert_eq!(result.checkpoint.state_write_failures, 1);
    assert!(result.checkpoint.interrupted);
    drop(cfg);
    drop(db);
    let reopened = SqliteStateDb::open(&db_path).await.unwrap();
    let summary = reopened.get_summary().await.unwrap();
    assert_eq!(summary.pending + summary.failed, 2);
    assert_eq!(summary.downloaded, 0);
    server.verify().await;
}

#[tokio::test]
async fn explicit_retry_enforces_cumulative_auth_threshold_and_retains_queued_debt() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};
    let server = crate::start_wiremock_or_skip!();
    Mock::given(method("GET"))
        .and(path("/auth"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/healthy"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(BYTES))
        .expect(0)
        .mount(&server)
        .await;
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("state.db");
    let db = Arc::new(SqliteStateDb::open(&db_path).await.unwrap());
    let tasks: Vec<_> = ["auth", "healthy"]
        .into_iter()
        .map(|name| {
            let mut task = retry_test_task(name, VersionSizeKey::Original, "unused");
            task.url = format!("{}/{name}", server.uri()).into();
            task.download_path = dir.path().join(format!("{name}.jpg"));
            task.size = 8;
            task
        })
        .collect();
    for task in &tasks {
        db.upsert_seen(
            &TestAssetRecord::new(&task.asset_id)
                .filename("selected.jpg")
                .size(8)
                .build(),
        )
        .await
        .unwrap();
    }
    let client = reqwest::Client::new();
    let retry = RetryConfig {
        max_retries: 0,
        base_delay_secs: 0,
        max_delay_secs: 0,
    };
    let result = run_download_pass(
        PassConfig {
            prior_auth_errors: 2,
            url_obtained_at: Default::default(),
            client: &client,
            retry_config: &retry,
            metadata: MetadataFlags::default(),
            mark_capture_repair_after_download: false,
            concurrency: 1,
            reporting: DownloadReporting::hidden(),
            temp_suffix: ".part".into(),
            shutdown_token: CancellationToken::new(),
            state_db: Some(db.clone()),
            rate_limit_counter: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            bandwidth_limiter: None,
            library: "PrimarySync".into(),
        },
        tasks,
    )
    .await;
    assert_eq!(result.auth_errors, 1, "current phase count stays honest");
    assert_eq!(result.failed.len(), 2);
    assert_eq!(result.downloaded, 0);
    drop(db);
    let reopened = SqliteStateDb::open(&db_path).await.unwrap();
    assert_eq!(reopened.get_summary().await.unwrap().failed, 2);
    server.verify().await;
}

#[tokio::test]
async fn incremental_refresh_preserves_frozen_raw_jpeg_provider_renditions() {
    use crate::types::RawPolicy;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};
    const RAW_BYTES: &[u8] = &[0x49, 0x49, 0x2a, 0x00, 8, 0, 0, 0];
    for policy in [RawPolicy::PreferRaw, RawPolicy::PreferJpeg] {
        for expiry_retry in [false, true] {
            for fault in ["none", "checksum", "size"] {
                let server = crate::start_wiremock_or_skip!();
                let (original_type, alternative_type, original_bytes, alternative_bytes) =
                    if policy == RawPolicy::PreferRaw {
                        ("public.jpeg", "com.adobe.raw-image", BYTES, RAW_BYTES)
                    } else {
                        ("com.adobe.raw-image", "public.jpeg", RAW_BYTES, BYTES)
                    };
                for (resource, bytes) in [
                    ("original", original_bytes),
                    ("alternative", alternative_bytes),
                ] {
                    Mock::given(method("GET"))
                        .and(path(format!("/old-{resource}")))
                        .respond_with(ResponseTemplate::new(410))
                        .expect(u64::from(expiry_retry))
                        .mount(&server)
                        .await;
                    Mock::given(method("GET"))
                        .and(path(format!("/new-{resource}")))
                        .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes))
                        .expect(u64::from(resource == "original" || fault == "none"))
                        .mount(&server)
                        .await;
                }
                let mut delta = incremental_photo_records_with_url(
                    "SWAPPED",
                    "paired.jpg",
                    &format!("{}/old-original", server.uri()),
                    8,
                );
                delta[0]["fields"]["resOriginalFileType"] = json!({"value": original_type});
                delta[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
                    json!("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=");
                delta[0]["fields"]["resOriginalAltFileType"] = json!({"value": alternative_type});
                delta[0]["fields"]["resOriginalAltRes"] = json!({"value": {"downloadURL":format!("{}/old-alternative", server.uri()), "fileChecksum":"AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=", "size":8}});
                let mut fresh = delta.clone();
                fresh[0]["fields"]["resOriginalRes"]["value"]["downloadURL"] =
                    json!(format!("{}/new-original", server.uri()));
                fresh[0]["fields"]["resOriginalAltRes"]["value"]["downloadURL"] =
                    json!(format!("{}/new-alternative", server.uri()));
                if fault == "checksum" {
                    fresh[0]["fields"]["resOriginalAltRes"]["value"]["fileChecksum"] =
                        json!("changed");
                }
                if fault == "size" {
                    fresh[0]["fields"]["resOriginalAltRes"]["value"]["size"] = json!(9);
                }
                let passes = vec![AlbumPass {
                    kind: PassKind::Unfiled,
                    album: album_with_session(
                        "PrimarySync",
                        "",
                        Box::new(BoundedSession {
                            delta: Arc::new(delta),
                            fresh: Arc::new(fresh),
                            requests: Arc::new(Mutex::new(Vec::new())),
                        }),
                    ),
                    exclude_ids: Arc::new(FxHashSet::default()),
                }];
                let dir = TempDir::new().unwrap();
                let state_path = dir.path().join("state.db");
                let db = Arc::new(SqliteStateDb::open(&state_path).await.unwrap());
                std::fs::create_dir_all(dir.path().join("media")).unwrap();
                std::fs::write(dir.path().join("media/unrelated.jpg"), b"existing media").unwrap();
                let mut config = test_config();
                config.directory = Arc::from(dir.path().join("media"));
                config.folder_structure = String::new();
                config.raw_policy = policy;
                config.alternative = true;
                config.state_db = Some(db.clone());
                let config = Arc::new(config);
                let result = download_photos_incremental_collecting_inner(
                    &reqwest::Client::new(),
                    &passes,
                    &config,
                    "previous",
                    DownloadControls::download_hidden(),
                    CancellationToken::new(),
                    if expiry_retry {
                        Duration::from_secs(3600)
                    } else {
                        Duration::ZERO
                    },
                )
                .await
                .unwrap();
                assert_eq!(
                    result.stats.downloaded,
                    if fault == "none" { 2 } else { 1 },
                    "{policy:?}, expiry_retry={expiry_retry}, fault={fault}; failures={:?}; stats={:?}",
                    db.get_failed().await.unwrap(),
                    result.stats
                );
                assert_eq!(result.stats.failed, usize::from(fault != "none"));
                drop(config);
                drop(db);
                let reopened = SqliteStateDb::open(&state_path).await.unwrap();
                let rows = reopened.get_downloaded_page(0, 10).await.unwrap();
                assert_eq!(rows.len(), if fault == "none" { 2 } else { 1 });
                for row in rows {
                    let bytes = std::fs::read(row.local_path.unwrap()).unwrap();
                    match row.version_size {
                        VersionSizeKey::Original => {
                            assert_eq!(
                                &*row.checksum,
                                "AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE="
                            );
                            assert_eq!(bytes, alternative_bytes);
                        }
                        _ => {
                            assert_eq!(row.version_size, VersionSizeKey::Alternative);
                            assert_eq!(
                                &*row.checksum,
                                "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
                            );
                            assert_eq!(bytes, original_bytes);
                        }
                    }
                }
                assert_eq!(
                    reopened.get_summary().await.unwrap().failed,
                    u64::from(fault != "none")
                );
                assert_eq!(
                    std::fs::read(dir.path().join("media/unrelated.jpg")).unwrap(),
                    b"existing media"
                );
                server.verify().await;
            }
        }
    }
}

#[tokio::test]
async fn recorded_retry_refresh_uses_original_pass_not_current_index() {
    use super::{RetryTaskKey, UrlRetrySource, build_incremental_expired_url_retry_tasks};
    let original_requests = Arc::new(Mutex::new(Vec::new()));
    let current_requests = Arc::new(Mutex::new(Vec::new()));
    let records = incremental_photo_records_with_url(
        "MASTER",
        "old.jpg",
        "https://p01.icloud-content.com/old",
        8,
    );
    let original = AlbumPass {
        kind: PassKind::Unfiled,
        album: album_with_session(
            "PrimarySync",
            "",
            Box::new(BoundedSession {
                delta: Arc::new(Vec::new()),
                fresh: Arc::new(records),
                requests: original_requests.clone(),
            }),
        ),
        exclude_ids: Arc::new(FxHashSet::default()),
    };
    let current = AlbumPass {
        kind: PassKind::SmartFolder,
        album: album_with_session(
            "PrimarySync",
            "Favorites",
            Box::new(BoundedSession {
                delta: Arc::new(Vec::new()),
                fresh: Arc::new(Vec::new()),
                requests: current_requests.clone(),
            }),
        ),
        exclude_ids: Arc::new(FxHashSet::default()),
    };
    let mut task = retry_test_task("child-state", VersionSizeKey::Original, "unused");
    task.asset_record_name = "asset-MASTER".into();
    task.size = 8;
    let key = crate::download::orchestration::generation::pass_key(&original);
    let mut sources = rustc_hash::FxHashMap::default();
    sources.insert(
        RetryTaskKey::from(&task),
        UrlRetrySource {
            asset_record_name: task.asset_record_name.clone(),
            master_record_name: "MASTER".into(),
            provider_version: VersionSizeKey::Original,
            pass_index: 0,
            recorded_pass: Some((key, original)),
        },
    );
    let plan = build_incremental_expired_url_retry_tasks(
        &[current],
        &sources,
        &[task.clone()],
        CancellationToken::new(),
    )
    .await;
    assert_eq!(
        plan.tasks.len(),
        1,
        "old lookups={}, current lookups={}, unrefreshed={}",
        original_requests.lock().unwrap().len(),
        current_requests.lock().unwrap().len(),
        plan.unrefreshed.len()
    );
    assert_eq!(plan.tasks[0].download_path, task.download_path);
    assert_eq!(plan.tasks[0].checksum, task.checksum);
    assert!(current_requests.lock().unwrap().is_empty());
    assert_eq!(original_requests.lock().unwrap().len(), 2);
    sources
        .get_mut(&RetryTaskKey::from(&task))
        .unwrap()
        .recorded_pass
        .as_mut()
        .unwrap()
        .0 = "wrong-key".into();
    let refused =
        build_incremental_expired_url_retry_tasks(&[], &sources, &[task], CancellationToken::new())
            .await;
    assert!(refused.tasks.is_empty());
    assert_eq!(refused.unrefreshed.len(), 1);
    assert_eq!(original_requests.lock().unwrap().len(), 2);
}
