#[cfg(unix)]
use super::bounded_file;
use super::{collect, endpoint_allowed};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

fn fixture(root: &Path, endpoint: &str, age: i64) -> PathBuf {
    let username = "synthetic-inventory@example.invalid";
    let namespace = crate::account::namespace(username, "com");
    std::fs::write(root.join(&namespace), b"[]").unwrap();
    std::fs::write(
        root.join(format!("{namespace}.session")),
        br#"{"session_token":"private-session","client_id":"private-client"}"#,
    )
    .unwrap();
    let cache = json!({"validated_at":chrono::Utc::now().timestamp()-age,
        "account_data":{"dsInfo":{"dsid":"private-dsid"},"webservices":{"ckdatabasews":{"url":endpoint}}}});
    std::fs::write(
        root.join(format!("{namespace}.cache")),
        serde_json::to_vec(&cache).unwrap(),
    )
    .unwrap();
    let config = root.join("config.toml");
    std::fs::write(&config, format!("data_dir = {:?}\n[auth]\nusername = {:?}\npassword_command = 'DO_NOT_EXECUTE'\n[download]\ndirectory = {:?}\n", root.to_str().unwrap(), username, root.join("media").to_str().unwrap())).unwrap();
    config
}

fn snapshot(root: &Path) -> Vec<(PathBuf, Vec<u8>, std::time::SystemTime, u32)> {
    let mut files: Vec<_> = std::fs::read_dir(root)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (
                entry.path(),
                std::fs::read(entry.path()).unwrap(),
                entry.metadata().unwrap().modified().unwrap(),
                {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::MetadataExt;
                        entry.metadata().unwrap().mode()
                    }
                    #[cfg(not(unix))]
                    {
                        u32::from(entry.metadata().unwrap().permissions().readonly())
                    }
                },
            )
        })
        .collect();
    files.sort_by(|a, b| a.0.cmp(&b.0));
    files
}

async fn discovery(server: &MockServer, state: &str) {
    Mock::given(method("POST")).and(path("/database/1/com.apple.photos.cloud/production/private/records/query"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"records":[{"recordName":"private-index","recordType":"CheckIndexingState","fields":{"state":{"value":state}}}]})))
        .mount(server).await;
    Mock::given(method("POST")).and(path("/database/1/com.apple.photos.cloud/production/private/zones/list"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"zones":[{"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner","zoneType":"REGULAR_CUSTOM_ZONE"}}]})))
        .mount(server).await;
}

#[tokio::test]
async fn narrowed_command_uses_cached_session_without_catalogue_media_or_original_auth_writes() {
    let server = MockServer::start().await;
    discovery(&server, "FINISHED").await;
    let mut records: Vec<Value> = (0..96).map(|i| json!({"recordName":format!("private-child-{i:03}"),"recordType":"CPLAsset","fields":{}})).collect();
    records
        .push(json!({"recordName":"private-long".repeat(64),"recordType":"CPLAsset","fields":{}}));
    Mock::given(method("POST")).and(path("/database/1/com.apple.photos.cloud/production/private/changes/zone"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"zones":[{"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"},"syncToken":"private-cursor","moreComing":false,"records":records}]})))
        .mount(&server).await;
    Mock::given(method("POST")).and(path("/database/1/com.apple.photos.cloud/production/private/records/lookup"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"records":[{"recordName":"private-child-000","recordType":"CPLAsset","zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"},"fields":{}}]})))
        .mount(&server).await;
    let dir = tempfile::tempdir().unwrap();
    let config = fixture(dir.path(), &format!("{}/", server.uri()), 0);
    std::fs::write(
        dir.path().join("catalogue.db"),
        b"private-unchanged-checkpoint",
    )
    .unwrap();
    std::fs::write(dir.path().join("media-canary"), b"private-media").unwrap();
    let before = snapshot(dir.path());
    let report = collect(&config, &CancellationToken::new(), true)
        .await
        .unwrap();
    assert_eq!(report["scan"], "eof_observed");
    assert_eq!(report["missing_references"], 97);
    assert_eq!(report["transport"]["discovery_requests"], 3);
    assert_eq!(report["transport"]["inventory_requests"], 1);
    assert_eq!(report["transport"]["lookup_requests"], 1);
    assert_eq!(report["lookup"]["shape"], "missing_reference");
    assert_eq!(report["lookup"]["scope"], "matched");
    assert_eq!(snapshot(dir.path()), before);
    assert_eq!(server.received_requests().await.unwrap().len(), 5);
    let output = serde_json::to_string(&report).unwrap();
    for private in ["private-", "example.invalid", "127.0.0.1", "family"] {
        assert!(!output.contains(private), "{output}");
    }
}

#[tokio::test]
async fn narrowed_command_expired_future_malformed_or_missing_cache_never_contacts_provider() {
    let server = MockServer::start().await;
    for age in [601, -60] {
        let dir = tempfile::tempdir().unwrap();
        let config = fixture(dir.path(), &server.uri(), age);
        let before = snapshot(dir.path());
        assert!(
            collect(&config, &CancellationToken::new(), true)
                .await
                .is_err()
        );
        assert_eq!(snapshot(dir.path()), before);
    }
    for bytes in [b"not-json".as_slice(), b"{}".as_slice()] {
        let dir = tempfile::tempdir().unwrap();
        let config = fixture(dir.path(), &server.uri(), 0);
        let cache = crate::auth::session::persisted_auth_files(
            dir.path(),
            "synthetic-inventory@example.invalid",
            "com",
        )[2]
        .clone();
        std::fs::write(&cache, bytes).unwrap();
        assert!(
            collect(&config, &CancellationToken::new(), true)
                .await
                .is_err()
        );
        std::fs::remove_file(&cache).unwrap();
        assert!(
            collect(&config, &CancellationToken::new(), true)
                .await
                .is_err()
        );
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn narrowed_command_running_or_http_auth_failure_never_scans_or_reauthenticates() {
    for status in [200, 401, 421] {
        let server = MockServer::start().await;
        if status == 200 {
            discovery(&server, "RUNNING").await;
        } else {
            Mock::given(method("POST"))
                .respond_with(
                    ResponseTemplate::new(status).set_body_string("private-provider-error"),
                )
                .mount(&server)
                .await;
        }
        let dir = tempfile::tempdir().unwrap();
        let config = fixture(dir.path(), &server.uri(), 0);
        let report = collect(&config, &CancellationToken::new(), true)
            .await
            .unwrap();
        assert_eq!(report["scan"], "incomplete");
        let requests = server.received_requests().await.unwrap();
        assert!(requests.len() <= 3);
        assert!(
            requests
                .iter()
                .all(|request| !request.url.path().contains("changes")
                    && !request.url.path().contains("auth"))
        );
    }
}

#[tokio::test]
async fn narrowed_command_refuses_redirect_without_followup() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("Location", format!("{}/private-redirect", server.uri())),
        )
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let config = fixture(dir.path(), &server.uri(), 0);
    let report = collect(&config, &CancellationToken::new(), true)
        .await
        .unwrap();
    assert_eq!(report["scan"], "incomplete");
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[test]
fn narrowed_command_endpoint_allowlist_is_exact() {
    for endpoint in [
        "https://p12-ckdatabasews.icloud.com",
        "https://ckdatabasews.icloud.com.cn/",
    ] {
        assert!(endpoint_allowed(endpoint, false));
    }
    for endpoint in [
        "http://p12-ckdatabasews.icloud.com",
        "https://p12-ckdatabasews.icloud.com.evil.invalid",
        "https://user@p12-ckdatabasews.icloud.com",
        "https://p12-ckdatabasews.icloud.com/path",
        "https://p12-ckdatabasews.icloud.com?token=private",
        "http://127.0.0.1:1",
    ] {
        assert!(!endpoint_allowed(endpoint, false));
    }
}

#[cfg(unix)]
#[tokio::test]
async fn narrowed_command_refuses_symlink_hardlink_and_oversize_inputs() {
    let dir = tempfile::tempdir().unwrap();
    let original = dir.path().join("original");
    std::fs::write(&original, b"private-input").unwrap();
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&original, &link).unwrap();
    assert!(bounded_file(&link).await.is_err());
    std::fs::remove_file(&link).unwrap();
    std::fs::hard_link(&original, &link).unwrap();
    assert!(bounded_file(&original).await.is_err());
    std::fs::remove_file(&link).unwrap();
    std::fs::write(&original, vec![b' '; super::INPUT_BYTES as usize + 1]).unwrap();
    assert!(bounded_file(&original).await.is_err());
}

#[tokio::test]
async fn narrowed_command_cancellation_during_discovery_returns_no_eof_or_retry() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(std::time::Duration::from_secs(30))
                .set_body_string("private-delayed-body"),
        )
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let config = fixture(dir.path(), &server.uri(), 0);
    let before = snapshot(dir.path());
    let cancel = CancellationToken::new();
    let cancellation = async {
        while server.received_requests().await.unwrap().is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        cancel.cancel();
    };
    let (report, ()) = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        tokio::join!(collect(&config, &cancel, true), cancellation)
    })
    .await
    .unwrap();
    assert_eq!(report.unwrap()["scan"], "incomplete");
    assert_eq!(snapshot(dir.path()), before);
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn narrowed_command_deadline_during_discovery_does_not_retry_or_write() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_delay(std::time::Duration::from_secs(30)))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let config = fixture(dir.path(), &server.uri(), 0);
    let before = snapshot(dir.path());
    let cancel = CancellationToken::new();
    let mut collection = Box::pin(collect(&config, &cancel, true));
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while server.received_requests().await.unwrap().is_empty() {
            tokio::select! {
                result = &mut collection => panic!("discovery completed before deadline: {result:?}"),
                () = tokio::time::sleep(std::time::Duration::from_millis(5)) => {},
            }
        }
        assert!(tokio::time::timeout(std::time::Duration::from_millis(20), collection)
            .await
            .is_err());
    })
    .await
    .unwrap();
    assert_eq!(snapshot(dir.path()), before);
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[test]
fn narrowed_command_worker() {
    let Some(config) = std::env::var_os("KEI_SYNTHETIC_INVENTORY_CONFIG") else {
        return;
    };
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap()
        .block_on(async {
            let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
            let blocker =
                if std::env::var("KEI_SYNTHETIC_INVENTORY_BLOCK_INPUT").as_deref() == Ok("1") {
                    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
                    let blocker = tokio::task::spawn_blocking(move || {
                        entered_tx.send(()).unwrap();
                        let _released = release_rx.blocking_recv();
                    });
                    entered_rx.await.unwrap();
                    Some(blocker)
                } else {
                    None
                };
            // Holding the only blocking worker queues the first async filesystem read
            // until after the command deadline, independently of machine speed.
            // Test-process-only loopback policy. Production run always passes false.
            let result = super::run_with_policy(
                Path::new(&config),
                true,
                std::time::Duration::from_millis(200),
            )
            .await;
            drop(release_tx);
            if let Some(blocker) = blocker {
                blocker.await.unwrap();
            }
            assert!(result.is_err());
        });
}

#[tokio::test]
async fn narrowed_command_process_deadline_emits_incomplete_without_writes_or_dumps() {
    for block_input in [false, true] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(std::time::Duration::from_secs(10))
                    .set_body_string("private-provider-body"),
            )
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let config = fixture(dir.path(), &server.uri(), 0);
        std::fs::write(
            dir.path().join("catalogue-canary.db"),
            b"retained-checkpoint",
        )
        .unwrap();
        let before = snapshot(dir.path());
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tokio::process::Command::new(std::env::current_exe().unwrap())
                .kill_on_drop(true)
                .args([
                    "inventory_diagnostic::tests::narrowed_command_worker",
                    "--exact",
                    "--nocapture",
                ])
                .env("KEI_SYNTHETIC_INVENTORY_CONFIG", &config)
                .env(
                    "KEI_SYNTHETIC_INVENTORY_BLOCK_INPUT",
                    if block_input { "1" } else { "0" },
                )
                .env("KEI_REQUEST_DUMP_DIR", dir.path().join("dumps"))
                .env_remove("ICLOUD_USERNAME")
                .env_remove("ICLOUD_PASSWORD")
                .env_remove("KEI_DATA_DIR")
                .output(),
        )
        .await
        .expect("diagnostic worker exceeded its process deadline")
        .unwrap();
        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).unwrap();
        let line = stdout.lines().find(|line| line.starts_with('{')).unwrap();
        let report: Value = serde_json::from_str(line).unwrap();
        assert_eq!(report["scan"], "incomplete");
        assert_eq!(report["reason"], "deadline");
        for output in [stdout, String::from_utf8(output.stderr).unwrap()] {
            assert!(!output.contains("private-"));
            assert!(!output.contains("example.invalid"));
            assert!(!output.contains("127.0.0.1"));
        }
        assert_eq!(snapshot(dir.path()), before);
        let requests = server.received_requests().await.unwrap().len();
        if block_input {
            assert_eq!(requests, 0);
        } else {
            // The whole-command deadline may expire before HTTP dispatch.
            assert!(requests <= 1);
        }
    }
}

#[tokio::test]
async fn narrowed_command_unqualified_primary_or_foreign_inventory_never_gets_lookup() {
    for foreign_inventory in [false, true] {
        let server = MockServer::start().await;
        if foreign_inventory {
            discovery(&server, "FINISHED").await;
        } else {
            Mock::given(path("/database/1/com.apple.photos.cloud/production/private/records/query"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({"records":[{"recordName":"private-index","recordType":"CheckIndexingState","fields":{"state":{"value":"FINISHED"}}}]}))).mount(&server).await;
            Mock::given(path(
                "/database/1/com.apple.photos.cloud/production/private/zones/list",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"zones":[]})))
            .mount(&server)
            .await;
        }
        Mock::given(path("/database/1/com.apple.photos.cloud/production/private/changes/zone"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"zones":[{"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"private-foreign-owner"},"syncToken":"private-cursor","moreComing":false,"records":[]}]}))).mount(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let config = fixture(dir.path(), &server.uri(), 0);
        let report = collect(&config, &CancellationToken::new(), true)
            .await
            .unwrap();
        assert_eq!(report["scan"], "incomplete");
        assert_eq!(report["transport"]["lookup_requests"], 0);
        assert!(!report.to_string().contains("private-"));
    }
}
