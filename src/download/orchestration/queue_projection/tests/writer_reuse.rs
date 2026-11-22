use super::{DownloadStore, Fixture, controls, destination_pass, owner};
use crate::download::orchestration::generation::TEST_METADATA_WRITER_BUDGET;
use crate::icloud::photos::inbox::ShadowCapture;
use crate::state::SqliteStateDb;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

#[tokio::test]
async fn sidecar_backfill_refuses_media_changed_before_receipt_commit() {
    use crate::download::metadata_rewrite::BEFORE_SIDECAR_RECEIPT;
    use crate::download::orchestration::generation::drain_local_backfill;
    use std::path::Path;

    fn same_size_edit(path: &Path) {
        let mut file = std::fs::File::options().write(true).open(path).unwrap();
        let mtime = file.metadata().unwrap().modified().unwrap();
        std::io::Write::write_all(&mut file, b"edit").unwrap();
        file.set_modified(mtime).unwrap();
    }
    fn replace(path: &Path) {
        let copy = path.with_extension("copy");
        std::fs::copy(path, &copy).unwrap();
        std::fs::rename(copy, path).unwrap();
    }
    let server = MockServer::start().await;
    let media = include_bytes!("../../../../../tests/data/media/pattern.jpg");
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(media.as_slice()))
        .expect(2)
        .mount(&server)
        .await;
    for edit in [same_size_edit as fn(&Path), replace] {
        let mut f = Fixture::new().await;
        f.config.metadata.xmp_sidecar = true;
        f.config.post_sync_metadata = true;
        f.session.records.lock().unwrap()[0]["fields"]["resOriginalRes"]["value"] = json!({"downloadURL":server.uri(),"size":media.len(),"fileChecksum":data_encoding::BASE64.encode(&Sha256::digest(media))});
        let passes = [destination_pass(&f, "A")];
        let cancel = CancellationToken::new();
        let result = crate::download::download_photos_with_sync(
            &reqwest::Client::new(),
            &passes,
            Arc::new(f.config.clone()),
            controls(),
            cancel.clone(),
        )
        .await
        .unwrap();
        assert_eq!(result.stats.downloaded, 1);
        let pending = || {
            f.db.acquire_lock("sidecar transaction evidence").unwrap().query_row::<i64,_,_>("SELECT count(*) FROM provider_active_destinations WHERE verified_media=1 AND verified_metadata=0 AND writer_output_proof IS NULL",[],|r|r.get(0)).unwrap()
        };
        assert_eq!(pending(), 1);
        // Input must be outside the change-time tick for retained verification.
        tokio::time::sleep(std::time::Duration::from_millis(1050)).await;
        let mut cursor = Default::default();
        let result = BEFORE_SIDECAR_RECEIPT
            .scope(
                edit,
                Box::pin(drain_local_backfill(
                    &passes,
                    &f.config,
                    &f.db,
                    controls(),
                    &cancel,
                    None,
                    &mut cursor,
                )),
            )
            .await
            .unwrap();
        assert_eq!(result.failed, 1);
        assert_eq!(
            pending(),
            1,
            "a changed or replaced inode must roll back receipt updates"
        );
    }
}

#[tokio::test]
async fn writer_reuse_rechecks_installed_files_before_completion() {
    use crate::download::orchestration::generation::TEST_AFTER_WRITER_SNAPSHOT;
    use std::path::Path;

    const MEDIA: &[u8] = include_bytes!("../../../../../tests/data/media/pattern.jpg");
    fn edit_media(path: &Path) {
        // Change bytes without changing identity, length or mtime.
        let mut file = std::fs::File::options().write(true).open(path).unwrap();
        let modified = file.metadata().unwrap().modified().unwrap();
        std::io::Write::write_all(&mut file, b"edit").unwrap();
        file.set_modified(modified).unwrap();
    }
    fn replace_packet(path: &Path) {
        // Same contents at a different inode: the retained entry is gone.
        let mut packet = path.as_os_str().to_owned();
        packet.push(".xmp");
        let replacement = path.with_extension("replacement");
        std::fs::copy(&packet, &replacement).unwrap();
        std::fs::File::open(&replacement)
            .unwrap()
            .set_modified(std::fs::metadata(&packet).unwrap().modified().unwrap())
            .unwrap();
        std::fs::rename(replacement, packet).unwrap();
    }
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
        .expect(2)
        .mount(&server)
        .await;
    for edit_during_packet_hash in [true, false] {
        let mut f = Fixture::new().await;
        f.config.folder_structure_albums = Arc::from("copies");
        f.config.metadata.xmp_sidecar = true;
        f.session.records.lock().unwrap()[0]["fields"]["resOriginalRes"]["value"] = json!({"downloadURL":server.uri(),"size":MEDIA.len(),"fileChecksum":data_encoding::BASE64.encode(&Sha256::digest(MEDIA))});
        for cycle in 0..3 {
            if cycle > 0 {
                f = super::reopen_destination_fixture(f).await;
            }
            if cycle < 2 {
                f.capture(
                    vec![
                        json!({"recordName":format!("opaque-{cycle}"),"recordType":"FutureOpaque"}),
                    ],
                    &format!("basis-{cycle}"),
                )
                .await;
            }
            let client = reqwest::Client::new();
            let pass = destination_pass(&f, "A");
            let sync = crate::download::download_photos_with_sync(
                &client,
                std::slice::from_ref(&pass),
                Arc::new(f.config.clone()),
                controls(),
                CancellationToken::new(),
            );
            let sync = TEST_METADATA_WRITER_BUDGET.scope(usize::from(cycle == 0), Box::pin(sync));
            let result = if cycle == 1 && edit_during_packet_hash {
                let db = f.db.clone();
                crate::download::file::WRITER_HASH_HOOK
                    .scope(
                        Arc::new(move |path| {
                            // Pause the hash and require a write on the shared
                            // connection to finish before reading any more bytes.
                            let is_packet = path.extension().is_some_and(|ext| ext == "xmp");
                            let key = if is_packet {
                                "packet_hash_probe"
                            } else {
                                "media_hash_probe"
                            };
                            tokio::runtime::Handle::current().block_on(async {
                                tokio::time::timeout(
                                    std::time::Duration::from_secs(5),
                                    db.set_metadata(key, "committed"),
                                )
                                .await
                                .unwrap()
                                .unwrap();
                            });
                            if is_packet {
                                // The media hash is already complete.
                                edit_media(&path.with_extension(""));
                            }
                        }),
                        sync,
                    )
                    .await
            } else if cycle == 1 {
                TEST_AFTER_WRITER_SNAPSHOT.scope(replace_packet, sync).await
            } else {
                sync.await
            }
            .unwrap();
            assert_eq!(result.stats.downloaded, usize::from(cycle == 0));
            assert_eq!(result.stats.selected_work_remaining, cycle == 1);
            let conn = f.db.acquire_lock("check durable writer debt").unwrap();
            if edit_during_packet_hash && cycle > 0 {
                let markers: i64 = conn
                    .query_row(
                        "SELECT count(*) FROM metadata WHERE key IN ('packet_hash_probe','media_hash_probe') AND value='committed'",
                        [],
                        |r| r.get(0),
                    )
                    .unwrap();
                assert_eq!(markers, 2);
            }
            let pending: i64 = conn.query_row("SELECT count(*) FROM provider_active_destinations WHERE admitted=1 AND verified_metadata=0", [], |r| r.get(0)).unwrap();
            assert_eq!(pending, i64::from(cycle == 1));
            if cycle == 1 {
                assert!(result.checkpoint.sync_token_blocked);
                let key: String = conn
                    .query_row(
                        "SELECT path FROM provider_active_destinations WHERE verified_metadata=0",
                        [],
                        |r| r.get(0),
                    )
                    .unwrap();
                let path = serde_json::from_str::<super::SelectionPath>(&key)
                    .unwrap()
                    .to_path();
                if edit_during_packet_hash {
                    assert!(std::fs::read(&path).unwrap().starts_with(b"edit"));
                }
                // Restore the fixture's media for the zero-budget retry after reopen.
                std::fs::write(path, MEDIA).unwrap();
            }
        }
    }
}

// Independent processes give chrono a real host timezone change without
// changing the environment of any other test. All phases share the backup.
#[tokio::test]
async fn writer_reuse_across_timezone_and_config_changes() {
    const ROOT: &str = "KEI_TEST_WRITER_REUSE_ROOT";
    const MEDIA: &[u8] = include_bytes!("../../../../../tests/data/media/pattern.jpg");
    let Some(root) = std::env::var_os(ROOT).map(PathBuf::from) else {
        let fixture = Fixture::new().await;
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
            .expect(1)
            .mount(&server)
            .await;
        for (phase, tz) in [
            ("seed", "UTC"),
            ("timezone", "America/New_York"),
            ("rewrite", "America/New_York"),
            ("config", "America/New_York"),
        ] {
            let output = tokio::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "download::orchestration::queue_projection::tests::writer_reuse::writer_reuse_across_timezone_and_config_changes"])
                .env(ROOT, fixture.dir.path())
                .env("TZ", tz)
                .env("KEI_TEST_WRITER_REUSE_PHASE", phase)
                .env("KEI_TEST_WRITER_REUSE_URL", server.uri())
                .output().await.unwrap();
            assert!(
                output.status.success(),
                "{phase}: {} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        return;
    };

    let phase = std::env::var("KEI_TEST_WRITER_REUSE_PHASE").unwrap();
    let url = std::env::var("KEI_TEST_WRITER_REUSE_URL").unwrap();
    let mut f = Fixture::new().await;
    f.db = Arc::new(
        SqliteStateDb::open_owned(&root.join("state.db"), &owner())
            .await
            .unwrap(),
    );
    f.capture = ShadowCapture::new(f.db.clone(), owner(), "com");
    f.pass = destination_pass(&f, "A");
    f.config.state_db = Some(f.db.clone() as Arc<dyn DownloadStore>);
    f.config.directory = root.join("media").into();
    f.config.folder_structure_albums = Arc::from("copies");
    f.config.metadata.xmp_sidecar = true;
    f.config.metadata.include_albums = phase == "config";
    f.session.records.lock().unwrap()[0]["fields"]["resOriginalRes"]["value"] = json!({"downloadURL":url,"size":MEDIA.len(),"fileChecksum":data_encoding::BASE64.encode(&Sha256::digest(MEDIA))});
    f.capture(
        vec![json!({"recordName":phase,"recordType":"FutureOpaque"})],
        &phase,
    )
    .await;
    let pending = matches!(phase.as_str(), "timezone" | "config");
    let result = TEST_METADATA_WRITER_BUDGET
        .scope(
            usize::from(!pending),
            Box::pin(crate::download::download_photos_with_sync(
                &reqwest::Client::new(),
                std::slice::from_ref(&f.pass),
                Arc::new(f.config.clone()),
                controls(),
                CancellationToken::new(),
            )),
        )
        .await
        .unwrap();
    assert_eq!(result.stats.downloaded, usize::from(phase == "seed"));
    assert_eq!(result.stats.selected_work_remaining, pending);
    let (path, generation): (String,String) = f.db.acquire_lock("inspect current writer").unwrap()
        .query_row("SELECT path,generation FROM provider_active_destinations WHERE verified_metadata=?1 LIMIT 1", [!pending], |r| Ok((r.get(0)?,r.get(1)?))).unwrap();
    let mut packet = serde_json::from_str::<super::SelectionPath>(&path)
        .unwrap()
        .to_path()
        .into_os_string();
    packet.push(".xmp");
    let packet = std::fs::read_to_string(packet).unwrap();
    let time = if matches!(phase.as_str(), "seed" | "timezone") {
        "22:13:20"
    } else {
        "17:13:20"
    };
    assert!(
        packet.contains(time),
        "wrong packet time in {phase}: {packet}"
    );
    if pending {
        let candidates =
            f.db.writer_output_candidates(owner(), generation, Default::default())
                .await
                .unwrap();
        assert_eq!(
            candidates.len(),
            usize::from(phase == "timezone"),
            "changed configuration must be rejected before file I/O"
        );
    }
}
