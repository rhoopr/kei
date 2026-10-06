//! Actual Linux syscalls in an exclusive disposable child, never production
//! fault switches. Other platform/storage/power-loss claims remain out of scope.

use super::{Fixture, destination_pass, reopen_destination_fixture};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

const MEDIA: &[u8] = b"\xff\xd8\xff\xe0\0\x10JFIF\0\x01\x01\0\0\x01\0\x01\0\0\xff\xd9";
const TEST_NAME: &str = "download::orchestration::queue_projection::tests::late_io::private_actual_late_io_faults_preserve_owned_work_and_recover_after_reopen";

#[test]
fn private_actual_late_io_faults_preserve_owned_work_and_recover_after_reopen() {
    if let Ok(case) = std::env::var("KEI_TEST_LATE_IO_CASE") {
        let root = PathBuf::from(std::env::var_os("KEI_TEST_LATE_IO_ROOT").unwrap());
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
            .block_on(run_case(&root, &case));
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let shim = directory.path().join("late-io.so");
    let compile = std::process::Command::new("cc")
        .args(["-shared", "-fPIC", "-Wall", "-Wextra", "-Werror", "-o"])
        .arg(&shim)
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/late_io_fixture.c"))
        .args(["-ldl", "-pthread"])
        .output()
        .unwrap();
    assert!(
        compile.status.success(),
        "{}",
        String::from_utf8_lossy(&compile.stderr)
    );
    for case in [
        "write_enospc",
        "flush_transport",
        "flush_cancel",
        "file_sync",
        "directory_sync",
        "reconcile_sync",
        #[cfg(feature = "xmp")]
        "sidecar_sync",
    ] {
        let root = directory.path().join(case);
        std::fs::create_dir(&root).unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST_NAME, "--nocapture", "--test-threads=1"])
            .env("LD_PRELOAD", &shim)
            .env("KEI_TEST_LATE_IO_ROOT", &root)
            .env("KEI_TEST_LATE_IO_CASE", case)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{case}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let reached = std::fs::read_to_string(root.join("fault-reached")).unwrap();
        assert!(reached.contains("progress="), "{case}: {reached}");
        assert!(
            !reached.contains("progress=0\n"),
            "the fault must follow actual bytes: {case}: {reached}"
        );
    }
}

async fn wait_for_fault(root: &Path) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !root.join("fault-reached").is_file() {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
}

async fn run_case(root: &Path, case: &str) {
    use super::super::super::models::{DownloadOutcome, SyncMode};
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mut fixture = Fixture::new().await;
    let media_root = root.join("media");
    std::fs::create_dir_all(media_root.join("A")).unwrap();
    std::fs::write(media_root.join("existing.jpg"), b"existing-media").unwrap();
    std::fs::write(media_root.join("existing.xmp"), b"retained-sidecar").unwrap();
    fixture.config.directory = Arc::from(media_root.as_path());
    fixture.config.folder_structure_albums = Arc::from("{album}");
    fixture.config.retry.max_retries = 0;
    fixture.config.temp_suffix = Arc::from(".part");
    fixture.pass = destination_pass(&fixture, "A");
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/recovered"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
        .mount(&server)
        .await;
    let mut endpoint = format!("{}/recovered", server.uri());
    let cancel = CancellationToken::new();
    let transport = matches!(case, "flush_transport" | "flush_cancel");
    let transport_stop = CancellationToken::new();
    let transport_requests = Arc::new(AtomicUsize::new(0));
    let transport_task = if transport {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        endpoint = format!("http://{}/partial", listener.local_addr().unwrap());
        let root = root.to_owned();
        let cancel = cancel.clone();
        let cancellation = case == "flush_cancel";
        let stop = transport_stop.clone();
        let requests = transport_requests.clone();
        Some(tokio::spawn(async move {
            loop {
                let accepted = tokio::select! {
                    () = stop.cancelled() => break,
                    accepted = listener.accept() => accepted,
                };
                let (mut stream, _) = accepted.unwrap();
                requests.fetch_add(1, Ordering::SeqCst);
                let mut request = [0; 4096];
                let _ = stream.read(&mut request).await.unwrap();
                stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: image/jpeg\r\nContent-Length: 22\r\nConnection: close\r\n\r\n").await.unwrap();
                if root.join("fault-enabled").is_file() {
                    stream.write_all(&MEDIA[..12]).await.unwrap();
                    wait_for_fault(&root).await;
                    if cancellation {
                        cancel.cancel();
                    }
                    // The fixture syscall remains in flight while transport closes.
                } else {
                    stream.write_all(MEDIA).await.unwrap();
                }
                drop(stream);
            }
        }))
    } else {
        None
    };
    fixture.session.records.lock().unwrap()[0]["fields"]["resOriginalRes"]["value"] = serde_json::json!({
        "downloadURL":endpoint,"size":MEDIA.len(),"fileChecksum":data_encoding::BASE64.encode(&Sha256::digest(MEDIA))
    });
    fixture
        .capture(
            super::records("m", super::OLD, "old-retained"),
            "late-io-source",
        )
        .await;
    #[cfg(feature = "xmp")]
    if case == "sidecar_sync" {
        fixture.config.metadata.xmp_sidecar = true;
        let write = crate::download::metadata::MetadataWrite {
            title: Some("old-owned-title".into()),
            ..Default::default()
        };
        crate::download::metadata::write_sidecar(
            &media_root.join("A/changed.JPG"),
            &write,
            ".part",
        )
        .await
        .unwrap();
    }
    let old_sidecar = std::fs::read(media_root.join("A/changed.JPG.xmp")).ok();
    // Local copy qualification starts from an actual completed owned download.
    if case != "reconcile_sync" {
        std::fs::write(root.join("fault-enabled"), b"enabled").unwrap();
    }
    let first = crate::download::download_photos_with_sync(
        &reqwest::Client::new(),
        std::slice::from_ref(&fixture.pass),
        Arc::new(fixture.config.clone()),
        super::controls(),
        cancel,
    )
    .await
    .unwrap();
    if case != "reconcile_sync" {
        let reached = std::fs::read_to_string(root.join("fault-reached"))
            .expect("the exact syscall fault must be reached before interpreting its outcome");
        assert!(
            reached.contains(if case == "write_enospc" {
                "errno=28"
            } else {
                "errno=5"
            }),
            "{case}: {reached}"
        );
        assert!(!reached.contains("progress=0\n"), "{case}: {reached}");
    }
    let selected = media_root.join("A/changed.JPG");
    if case == "directory_sync" || case == "reconcile_sync" {
        assert_eq!(first.stats.downloaded, 1, "{case}: {first:?}");
        assert_eq!(std::fs::read(&selected).unwrap(), MEDIA);
    } else if case == "sidecar_sync" {
        assert_eq!(first.stats.downloaded, 1, "{first:?}");
        assert!(
            first.checkpoint.sync_token_blocked,
            "metadata debt cannot be forgotten: {first:?}"
        );
        assert_eq!(
            std::fs::read(media_root.join("A/changed.JPG.xmp")).ok(),
            old_sidecar
        );
        assert_eq!(fixture.db.acquire_lock("failed strict sidecar receipt").unwrap().query_row::<i64,_,_>("SELECT count(*) FROM provider_active_destinations WHERE verified_media=1 AND verified_metadata=0",[],|r|r.get(0)).unwrap(),1);
    } else {
        assert_eq!(first.stats.downloaded, 0, "{case}: {first:?}");
        assert!(first.sync_token.is_none());
        assert!(first.checkpoint.sync_token_blocked || first.checkpoint.interrupted);
        assert!(!selected.exists(), "disk failure must precede publication");
        let error = fixture
            .db
            .acquire_lock("actual disk failure precedence")
            .unwrap()
            .query_row::<Option<String>, _, _>(
                "SELECT last_error FROM assets WHERE id='asset-m'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            error
                .as_deref()
                .is_some_and(|e| e.contains("Could not write to disk")),
            "{case}: disk error must win: {error:?}; {first:?}"
        );
        assert_eq!(
            fixture
                .db
                .acquire_lock("no false verified media receipt")
                .unwrap()
                .query_row::<i64, _, _>(
                    "SELECT count(*) FROM provider_active_destinations WHERE verified_media=1",
                    [],
                    |r| r.get(0)
                )
                .unwrap(),
            0
        );
    }
    let copied = media_root.join("reconciled.JPG");
    if case == "reconcile_sync" {
        std::fs::write(root.join("fault-enabled"), b"enabled").unwrap();
        let error = crate::download::file::copy_local_file_no_replace(
            &media_root,
            &selected,
            &copied,
            ".part",
        )
        .await
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("Input/output error"),
            "{error:#}"
        );
        assert!(!copied.exists());
        assert_eq!(std::fs::read(&selected).unwrap(), MEDIA);
    }
    let reached = std::fs::read_to_string(root.join("fault-reached")).unwrap();
    if case == "write_enospc" {
        assert!(reached.contains("errno=28"));
    } else {
        assert!(reached.contains("errno=5"));
    }
    fixture.preserved().await;
    // Remove only the exact fault. Ownership/state is independently reopened.
    std::fs::remove_file(root.join("fault-enabled")).unwrap();
    fixture = reopen_destination_fixture(fixture).await;
    if !matches!(case, "directory_sync" | "reconcile_sync") {
        let recovered = crate::download::download_photos_with_sync(
            &reqwest::Client::new(),
            std::slice::from_ref(&fixture.pass),
            Arc::new(fixture.config.clone()),
            super::controls(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(
            matches!(recovered.outcome, DownloadOutcome::Success),
            "{case}: {recovered:?}"
        );
        assert_eq!(
            recovered.stats.downloaded,
            usize::from(case != "sidecar_sync")
        );
        assert_eq!(std::fs::read(&selected).unwrap(), MEDIA);
    }
    if case == "reconcile_sync" {
        let copied_receipt = crate::download::file::copy_local_file_no_replace(
            &media_root,
            &selected,
            &copied,
            ".part",
        )
        .await
        .unwrap()
        .unwrap();
        copied_receipt.validate().await.unwrap();
        assert_eq!(std::fs::read(&copied).unwrap(), MEDIA);
    }
    let checksum = format!("{:x}", Sha256::digest(MEDIA));
    assert_eq!(fixture.db.acquire_lock("verified independent source after recovery").unwrap().query_row::<String,_,_>("SELECT source_checksum FROM provider_active_destinations WHERE verified_media=1 LIMIT 1",[],|r|r.get(0)).unwrap(),checksum);
    let requests = server.received_requests().await.unwrap().len();
    let raw_requests = transport_requests.load(Ordering::SeqCst);
    let lookups = fixture
        .session
        .calls
        .load(std::sync::atomic::Ordering::SeqCst);
    let rank = fixture
        .session
        .rank_calls
        .load(std::sync::atomic::Ordering::SeqCst);
    let time = std::fs::metadata(&selected).unwrap().modified().unwrap();
    for _ in 0..2 {
        fixture = reopen_destination_fixture(fixture).await;
        fixture.config.sync_mode = SyncMode::Incremental {
            zone_sync_token: "old-cursor".into(),
        };
        let quiet = crate::download::download_photos_with_sync(
            &reqwest::Client::new(),
            std::slice::from_ref(&fixture.pass),
            Arc::new(fixture.config.clone()),
            super::controls(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(
            matches!(quiet.outcome, DownloadOutcome::Success),
            "{case}: {quiet:?}"
        );
        assert_eq!(quiet.stats.downloaded, 0);
        assert_eq!(
            fixture
                .session
                .calls
                .load(std::sync::atomic::Ordering::SeqCst),
            lookups
        );
        assert_eq!(
            fixture
                .session
                .rank_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            rank
        );
        assert_eq!(server.received_requests().await.unwrap().len(), requests);
        assert_eq!(transport_requests.load(Ordering::SeqCst), raw_requests);
        assert_eq!(
            std::fs::metadata(&selected).unwrap().modified().unwrap(),
            time
        );
        fixture.preserved().await;
    }
    transport_stop.cancel();
    if let Some(transport_task) = transport_task {
        transport_task.await.unwrap();
    }
}
