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
        "write_boundary",
        "flush_transport",
        "flush_cancel",
        "file_sync",
        "directory_sync",
        "reconcile_sync",
        "reconcile_parent",
        #[cfg(feature = "xmp")]
        "sidecar_sync",
        #[cfg(feature = "xmp")]
        "sidecar_parent",
    ] {
        let root = directory.path().join(case);
        std::fs::create_dir(&root).unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        let test_name = if case.starts_with("reconcile_") {
            "sync_loop::runner::tests::configuration::late_io::actual_reconciliation_fault_child"
        } else {
            TEST_NAME
        };
        if case == "sidecar_parent" {
            // Reuse the existing test-only fallback selector. This mode adds no
            // Rust error: the preload fixture fails the real parent fsync.
            child.env("KEI_TEST_TERMINAL_ROOT", root.join("media/A"));
            child.env("KEI_TEST_TERMINAL_MODE", "late-io");
        }
        let output = child
            .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
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

async fn wait_for_marker(root: &Path, marker: &str) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !root.join(marker).is_file() {
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

    // A body larger than Tokio's file buffer forces another write_all poll.
    // The flush-suppression control must still pass this write-error case.
    let large_body = (case == "write_boundary").then(|| {
        let mut bytes = MEDIA[..MEDIA.len() - 2].to_vec();
        bytes.resize(4 * 1024 * 1024, 0);
        bytes.extend_from_slice(&MEDIA[MEDIA.len() - 2..]);
        bytes
    });
    let media = large_body.as_deref().unwrap_or(MEDIA);
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
        .respond_with(ResponseTemplate::new(200).set_body_bytes(media.to_vec()))
        .mount(&server)
        .await;
    let mut endpoint = format!("{}/recovered", server.uri());
    let cancel = CancellationToken::new();
    let transport = matches!(case, "flush_transport" | "flush_cancel" | "write_boundary");
    let transport_stop = CancellationToken::new();
    let transport_requests = Arc::new(AtomicUsize::new(0));
    let transport_task = if transport {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        endpoint = format!("http://{}/partial", listener.local_addr().unwrap());
        let root = root.to_owned();
        let cancel = cancel.clone();
        let cancellation = case == "flush_cancel";
        let write_boundary = case == "write_boundary";
        let stop = transport_stop.clone();
        let requests = transport_requests.clone();
        let media = media.to_vec();
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
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", media.len()).as_bytes()).await.unwrap();
                if root.join("fault-enabled").is_file() {
                    stream.write_all(&media[..12]).await.unwrap();
                    wait_for_marker(&root, "fault-reached").await;
                    if write_boundary {
                        // A second write_all must consume the first write's
                        // ENOSPC before terminal flush can hide this boundary.
                        wait_for_marker(&root, "fault-returning").await;
                        // The expected write refusal may close the connection
                        // while the larger second body is still being sent.
                        let _closed_by_disk_error = stream.write_all(&media[12..]).await;
                    } else {
                        if cancellation {
                            cancel.cancel();
                        }
                        stream.shutdown().await.unwrap();
                        drop(stream);
                        std::fs::write(root.join("fault-release"), b"transport stopped").unwrap();
                        continue;
                    }
                } else {
                    stream.write_all(&media).await.unwrap();
                }
                drop(stream);
            }
        }))
    } else {
        None
    };
    fixture.session.records.lock().unwrap()[0]["fields"]["resOriginalRes"]["value"] = serde_json::json!({
        "downloadURL":endpoint,"size":media.len(),"fileChecksum":data_encoding::BASE64.encode(&Sha256::digest(media))
    });
    fixture
        .capture(
            super::records("m", super::OLD, "old-retained"),
            "late-io-source",
        )
        .await;
    fixture.db.acquire_lock("seed independent config checkpoints").unwrap().execute_batch(
        "INSERT INTO metadata(key,value) VALUES('config_hash','active-layout'),('enum_config_hash','active-enumeration'),('pending_download_config_hash','retained-layout-debt');"
    ).unwrap();
    let unrelated_checksum = data_encoding::HEXLOWER.encode(&Sha256::digest(b"existing-media"));
    fixture
        .db
        .upsert_seen(
            &crate::test_helpers::TestAssetRecord::new("unrelated-owned")
                .filename("existing.jpg")
                .size(14)
                .build(),
        )
        .await
        .unwrap();
    fixture
        .db
        .mark_downloaded(
            "PrimarySync",
            "unrelated-owned",
            "original",
            &media_root.join("existing.jpg"),
            &unrelated_checksum,
            None,
        )
        .await
        .unwrap();
    let unrelated_receipts = durable_rows(
        &fixture,
        &[
            "assets WHERE id='unrelated-owned'",
            "asset_metadata_paths WHERE id='unrelated-owned'",
        ],
    );
    let owner_before = durable_rows(&fixture, &["account_owner"]);
    let checkpoints_before = durable_rows(
        &fixture,
        &[
            "metadata WHERE key IN ('config_hash','enum_config_hash','pending_download_config_hash','pending_sync_token:sparse:OtherZone','sync_token:PrimarySync')",
        ],
    );
    #[cfg(feature = "xmp")]
    if matches!(case, "sidecar_sync" | "sidecar_parent") {
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
    std::fs::write(root.join("fault-enabled"), b"enabled").unwrap();
    let first = crate::download::download_photos_with_sync(
        &reqwest::Client::new(),
        std::slice::from_ref(&fixture.pass),
        Arc::new(fixture.config.clone()),
        super::controls(),
        cancel,
    )
    .await
    .unwrap();
    // Release every owner and independently reopen before any SQL outcome
    // assertion or fault removal. Reopened values, not cached owners, qualify.
    fixture = reopen_destination_fixture(fixture).await;
    assert_eq!(durable_rows(&fixture, &["account_owner"]), owner_before);
    assert_eq!(
        durable_rows(
            &fixture,
            &[
                "assets WHERE id='unrelated-owned'",
                "asset_metadata_paths WHERE id='unrelated-owned'"
            ]
        ),
        unrelated_receipts
    );
    assert_eq!(
        durable_rows(
            &fixture,
            &[
                "metadata WHERE key IN ('config_hash','enum_config_hash','pending_download_config_hash','pending_sync_token:sparse:OtherZone','sync_token:PrimarySync')"
            ]
        ),
        checkpoints_before
    );
    {
        let reached = std::fs::read_to_string(root.join("fault-reached"))
            .expect("the exact syscall fault must be reached before interpreting its outcome");
        assert!(
            reached.contains(if matches!(case, "write_enospc" | "write_boundary") {
                "errno=28"
            } else {
                "errno=5"
            }),
            "{case}: {reached}"
        );
        assert!(!reached.contains("progress=0\n"), "{case}: {reached}");
    }
    let enumeration_pending = fixture
        .db
        .get_metadata("enum_in_progress:PrimarySync")
        .await
        .unwrap();
    if case == "flush_cancel" {
        assert!(
            enumeration_pending
                .as_deref()
                .is_some_and(|value| value.parse::<i64>().is_ok_and(|time| time > 0)),
            "cancelled enumeration must retain its durable restart marker"
        );
    } else {
        assert!(
            enumeration_pending.is_none(),
            "{case}: unexpected interrupted enumeration"
        );
    }
    assert_eq!(
        fixture
            .db
            .get_metadata("active_selection_metadata_retry_offset")
            .await
            .unwrap(),
        matches!(case, "sidecar_sync" | "sidecar_parent").then(|| "1".to_owned()),
        "{case}: metadata retry work must survive reopening"
    );
    let selected = media_root.join("A/changed.JPG");
    if case == "directory_sync" {
        assert_eq!(first.stats.downloaded, 1, "{case}: {first:?}");
        assert_eq!(std::fs::read(&selected).unwrap(), media);
    } else if matches!(case, "sidecar_sync" | "sidecar_parent") {
        assert_eq!(first.stats.downloaded, 1, "{first:?}");
        assert!(
            first.checkpoint.sync_token_blocked,
            "metadata debt cannot be forgotten: {first:?}"
        );
        if case == "sidecar_sync" {
            assert_eq!(
                std::fs::read(media_root.join("A/changed.JPG.xmp")).ok(),
                old_sidecar
            );
        } else {
            // The strict failure follows installed bytes and a synced commit
            // marker. Changed target bytes are truthful unfinished metadata.
            assert_ne!(
                std::fs::read(media_root.join("A/changed.JPG.xmp")).ok(),
                old_sidecar
            );
            assert!(file_census(&media_root).iter().any(
                |entry| entry.0.contains(".kei-replace-") && entry.0.ends_with("manifest.json")
            ));
        }
        assert_eq!(std::fs::read(&selected).unwrap(), media);
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
    let reached = std::fs::read_to_string(root.join("fault-reached")).unwrap();
    if matches!(case, "write_enospc" | "write_boundary") {
        assert!(reached.contains("errno=28"));
    } else {
        assert!(reached.contains("errno=5"));
    }
    if !matches!(case, "directory_sync" | "sidecar_sync" | "sidecar_parent") {
        let catalog: (String, Option<String>, Option<String>) = fixture
            .db
            .acquire_lock("failed catalogue truth")
            .unwrap()
            .query_row(
                "SELECT status,local_path,local_checksum FROM assets WHERE id='asset-m'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            catalog,
            ("failed".into(), None, None),
            "{case}: failed work is durable"
        );
        assert_eq!(
            fixture
                .db
                .acquire_lock("no false path receipt")
                .unwrap()
                .query_row::<i64, _, _>(
                    "SELECT count(*) FROM asset_metadata_paths WHERE id='asset-m'",
                    [],
                    |row| row.get(0)
                )
                .unwrap(),
            0
        );
    }
    fixture.preserved().await;
    assert_eq!(
        fixture.count("owned_temp_files"),
        0,
        "all transfer claims retire"
    );
    let failed_files = file_census(&media_root);
    if !matches!(case, "sidecar_parent") {
        assert!(
            failed_files.iter().all(|entry| !entry.0.ends_with(".part")),
            "{case}: {failed_files:?}"
        );
    }
    let retained_preparation = failed_files
        .iter()
        .filter(|entry| entry.0.ends_with(".part"))
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(
        retained_preparation.len(),
        usize::from(case == "sidecar_parent")
    );
    if case == "sidecar_parent" {
        let installed = failed_files
            .iter()
            .find(|entry| entry.0 == "A/changed.JPG.xmp")
            .unwrap();
        assert_eq!(retained_preparation[0].1, installed.1);
        assert_eq!(retained_preparation[0].2, installed.2);
        assert_eq!(retained_preparation[0].3, 2);
    }
    // Remove only the exact fault after the independent failure oracle.
    std::fs::remove_file(root.join("fault-enabled")).unwrap();
    if case != "directory_sync" {
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
            usize::from(!matches!(case, "sidecar_sync" | "sidecar_parent"))
        );
        assert_eq!(std::fs::read(&selected).unwrap(), media);
    }
    fixture = reopen_destination_fixture(fixture).await;
    assert_eq!(fixture.count("owned_temp_files"), 0);
    let completed_receipts = durable_rows(
        &fixture,
        &[
            "assets",
            "asset_metadata_paths",
            "provider_active_destinations",
            "account_owner",
            "metadata",
        ],
    );
    let completed_files = file_census(&media_root);
    assert!(
        completed_files
            .iter()
            .all(|entry| !entry.0.contains(".kei-replace-")),
        "{case}: {completed_files:?}"
    );
    // Recovery has no persisted name/ownership for the writer's prepared
    // sidecar hardlink. Retain that exact historical entry; never grow it.
    assert_eq!(
        completed_files
            .iter()
            .filter(|entry| entry.0.ends_with(".part"))
            .cloned()
            .collect::<Vec<_>>(),
        retained_preparation
    );
    assert_eq!(
        fixture
            .db
            .acquire_lock("exact completed catalogue path")
            .unwrap()
            .query_row::<String, _, _>(
                "SELECT local_path FROM assets WHERE id='asset-m'",
                [],
                |r| r.get(0)
            )
            .unwrap(),
        selected.to_str().unwrap()
    );
    let checksum = format!("{:x}", Sha256::digest(media));
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
        assert_eq!(
            file_census(&media_root),
            completed_files,
            "{case}: quiet files/sidecars/temp census"
        );
        assert_eq!(
            durable_rows(
                &fixture,
                &[
                    "assets",
                    "asset_metadata_paths",
                    "provider_active_destinations",
                    "account_owner",
                    "metadata"
                ]
            ),
            completed_receipts,
            "{case}: quiet full receipts"
        );
        fixture.preserved().await;
    }
    transport_stop.cancel();
    if let Some(transport_task) = transport_task {
        transport_task.await.unwrap();
    }
}

// Full rows include receipt paths, hashes, retry markers and ownership. Sort
// every column so WITHOUT ROWID and native path spellings are both preserved.
fn durable_rows(fixture: &Fixture, tables: &[&str]) -> Vec<Vec<Vec<rusqlite::types::Value>>> {
    let connection = fixture
        .db
        .acquire_lock("independent full-row oracle")
        .unwrap();
    tables
        .iter()
        .map(|table| {
            let mut statement = connection
                .prepare(&format!("SELECT * FROM {table}"))
                .unwrap();
            let columns = statement.column_count();
            let mut rows: Vec<Vec<rusqlite::types::Value>> = statement
                .query_map([], |row| {
                    (0..columns).map(|column| row.get(column)).collect()
                })
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            rows.sort_by_key(|row| format!("{row:?}"));
            rows
        })
        .collect()
}

type FileEntry = (String, Vec<u8>, u64, u64, std::time::SystemTime);

fn file_census(root: &Path) -> Vec<FileEntry> {
    use std::os::unix::fs::MetadataExt;
    fn visit(root: &Path, path: &Path, files: &mut Vec<FileEntry>) {
        for entry in std::fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            let metadata = std::fs::symlink_metadata(&path).unwrap();
            if metadata.is_dir() {
                visit(root, &path, files);
            } else {
                assert!(metadata.is_file());
                files.push((
                    path.strip_prefix(root)
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .to_owned(),
                    std::fs::read(&path).unwrap(),
                    metadata.ino(),
                    metadata.nlink(),
                    metadata.modified().unwrap(),
                ));
            }
        }
    }
    let mut files = Vec::new();
    visit(root, root, &mut files);
    files.sort_by(|a, b| a.0.cmp(&b.0));
    files
}
