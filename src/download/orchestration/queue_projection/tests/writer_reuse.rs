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
