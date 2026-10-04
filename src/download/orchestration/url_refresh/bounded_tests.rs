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
        result.downloaded, 1,
        "persistent 410 must not cancel the healthy peer"
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
    let calls = requests.lock().unwrap();
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
    drop(calls);
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
            "rendition" => {
                fresh[0]["fields"]
                    .as_object_mut()
                    .unwrap()
                    .remove("resOriginalRes");
            }
            _ => unreachable!(),
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
        let (result, ()) = tokio::time::timeout(Duration::from_secs(3), async {
            tokio::join!(worker, cancel)
        })
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
    use wiremock::matchers::method;
    use wiremock::{Mock, ResponseTemplate};
    let server = crate::start_wiremock_or_skip!();
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(BYTES))
        .expect(1)
        .mount(&server)
        .await;
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("selected-path.jpg");
    std::fs::write(&path, b"external file edit").unwrap();
    let db_path = dir.path().join("state.db");
    let db = Arc::new(SqliteStateDb::open(&db_path).await.unwrap());
    let records = incremental_photo_records_with_url(
        "MASTER",
        "different-provider-name.jpg",
        &server.uri(),
        8,
    );
    let mut task = retry_test_task("child-state", VersionSizeKey::Original, "unused");
    task.asset_record_name = "asset-MASTER".into();
    task.download_path = path.clone();
    task.size = 8;
    db.upsert_seen(
        &TestAssetRecord::new("child-state")
            .filename("selected-path.jpg")
            .size(8)
            .build(),
    )
    .await
    .unwrap();
    let passes = vec![AlbumPass {
        kind: PassKind::Unfiled,
        album: album_with_session(
            "PrimarySync",
            "",
            Box::new(BoundedSession {
                delta: Arc::new(Vec::new()),
                fresh: Arc::new(records),
                requests: Arc::new(Mutex::new(Vec::new())),
            }),
        ),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];
    let mut sources = rustc_hash::FxHashMap::default();
    sources.insert(
        RetryTaskKey::from(&task),
        UrlRetrySource {
            asset_record_name: task.asset_record_name.clone(),
            master_record_name: "MASTER".into(),
            pass_index: 0,
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
    assert_eq!(refreshed.download_path, path);
    assert_eq!(refreshed.asset_id.as_ref(), "child-state");
    assert_eq!(refreshed.asset_record_name.as_ref(), "asset-MASTER");
    assert_eq!(refreshed.created_local, task.created_local);
    assert_eq!(refreshed.checksum, task.checksum);
    assert_eq!(refreshed.version_size, task.version_size);
    assert!(Arc::ptr_eq(&refreshed.metadata, &task.metadata));
    assert!(matches!(
        refreshed.publication,
        crate::download::file::FinalPublication::NoReplace
    ));
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
    assert_eq!(result.downloaded, 0);
    assert_eq!(result.failed.len(), 1);
    assert_eq!(std::fs::read(&path).unwrap(), b"external file edit");
    drop(db);
    let reopened = SqliteStateDb::open(&db_path).await.unwrap();
    assert_eq!(reopened.get_summary().await.unwrap().failed, 1);
    server.verify().await;
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
