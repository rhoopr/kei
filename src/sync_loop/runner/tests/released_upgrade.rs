//! Released-schema recovery through the production cycle owner.
use std::sync::Arc;

use base64::Engine as _;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

use crate::sync_loop::test_support::{
    FailingMetadataSetDb, full_album_page_with_download, make_full_album_with_boxed_session,
    make_run_cycle_config, make_run_cycle_download_config_builder,
    make_run_cycle_library_state_with_album, make_shared_session_for_run_cycle,
};
use crate::{download, state};

#[derive(Clone, Debug)]
struct UpgradeSession {
    records: serde_json::Value,
    quiet: bool,
}

#[async_trait::async_trait]
impl crate::icloud::photos::PhotosSession for UpgradeSession {
    async fn post(
        &self,
        url: &str,
        body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<serde_json::Value> {
        if url.contains("/changes/zone?") {
            let records = if self.quiet {
                serde_json::json!([])
            } else {
                self.records.clone()
            };
            return Ok(
                serde_json::json!({"zones":[{"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"},"syncToken":"after","moreComing":false,"records":records}]}),
            );
        }
        assert!(
            url.contains("/records/lookup?"),
            "unexpected provider call: {url}"
        );
        assert!(!self.quiet, "completed history must not hydrate again");
        let request: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(request["zoneID"]["zoneName"], "PrimarySync");
        for record in request["records"].as_array().unwrap() {
            assert!(
                ["asset-recovered", "recovered"].contains(&record["recordName"].as_str().unwrap())
            );
        }
        Ok(serde_json::json!({"records":self.records}))
    }

    fn clone_box(&self) -> Box<dyn crate::icloud::photos::PhotosSession> {
        Box::new(self.clone())
    }
}

#[tokio::test]
async fn released_v0240_failed_history_recovers_after_upgrade_and_restart() {
    // Deliberately no start_wiremock_or_skip: inability to bind must fail this
    // qualification rather than reporting an unexecuted preservation proof.
    let server = wiremock::MockServer::start().await;
    let bytes = b"\xff\xd8\xff\xe0\x00\x10JFIF\x00\x01\x01\x00\x00\x01\x00\x01\x00\x00\xff\xd9";
    let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(bytes));
    Mock::given(method("GET"))
        .and(path("/recovered.jpg"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes))
        .expect(1)
        .mount(&server)
        .await;
    let records = full_album_page_with_download(
        "PrimarySync",
        "recovered",
        "after",
        &format!("{}/recovered.jpg", server.uri()),
        bytes.len() as u64,
        &checksum,
    )["records"]
        .clone();
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("upgradeexampleinvalid.db");
    let media = dir.path().join("media");
    std::fs::create_dir(&media).unwrap();
    if let Some(binary) = std::env::var_os("KEI_TEST_RELEASED_V0240") {
        let config = dir.path().join("config.toml");
        std::fs::write(
            &config,
            format!(
                "[download]\ndirectory = {}\n",
                serde_json::to_string(&media).unwrap()
            ),
        )
        .unwrap();
        std::fs::write(&database, []).unwrap();
        let output = std::process::Command::new(binary)
            .env_clear()
            .env("ICLOUD_USERNAME", "upgrade@example.invalid")
            .env("KEI_DATA_DIR", dir.path())
            .arg("--config")
            .arg(&config)
            .arg("verify")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    } else {
        rusqlite::Connection::open(&database)
            .unwrap()
            .execute_batch(include_str!(
                "../../../../tests/data/released-v0240-schema.sql"
            ))
            .unwrap();
    }
    let conn = rusqlite::Connection::open(&database).unwrap();
    assert_eq!(
        conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i32>(0))
            .unwrap(),
        25
    );
    // Synthetic persisted facts, not an old-binary provider sync. The identity
    // mapping is explicit authoritative evidence; no orphan owner is inferred.
    conn.execute("INSERT INTO assets(library,id,version_size,checksum,filename,created_at,size_bytes,media_type,status,last_seen_at,last_error) VALUES ('PrimarySync','asset-recovered','original',?1,'photo.jpg',1700000000,?2,'photo','failed',1700000000,'interrupted before publication')",
        rusqlite::params![checksum,bytes.len() as i64]).unwrap();
    conn.execute_batch("INSERT INTO asset_master_mappings VALUES ('PrimarySync','asset-recovered','recovered',1700000000);
        INSERT INTO metadata VALUES ('sync_token:PrimarySync','before');
        INSERT INTO sync_runs(started_at,completed_at,status) VALUES (1700000000,1700000001,'complete');").unwrap();
    drop(conn);
    let original = media.join("retained-original.jpg");
    let sidecar = media.join("retained-original.xmp");
    std::fs::write(&original, b"synthetic retained original").unwrap();
    std::fs::write(&sidecar, b"synthetic independent metadata").unwrap();
    let config = make_run_cycle_config();
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
    let mut recovered_path = None;
    for cycle in 0..3 {
        {
            let inner = Arc::new(state::SqliteStateDb::open(&database).await.unwrap());
            let db: Arc<dyn download::DownloadStore> = if cycle == 0 {
                Arc::new(
                    FailingMetadataSetDb::without_set_failure(
                        inner.clone(),
                        "injected upgrade recovery failure",
                    )
                    .with_upsert_seen_failure(),
                )
            } else {
                inner.clone()
            };
            let primary = make_run_cycle_library_state_with_album(
                "PrimarySync",
                "sync_token:PrimarySync",
                make_full_album_with_boxed_session(
                    "PrimarySync",
                    Box::new(UpgradeSession {
                        records: records.clone(),
                        quiet: cycle == 2,
                    }),
                ),
            );
            let builder = make_run_cycle_download_config_builder(&media, db.clone());
            let result = crate::sync_cycle::run_cycle(
                &[&primary],
                &config,
                Some(db.as_ref()),
                false,
                &builder,
                download::DownloadControls::download_hidden(),
                &shared_session,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
            if cycle == 0 {
                assert!(
                    result.failed_count > 0 || result.stats.state_write_failures > 0,
                    "{result:?}"
                );
            } else {
                assert_eq!(result.failed_count, 0);
                assert!(!result.stats.identity_incomplete);
                assert_eq!(result.stats.downloaded, usize::from(cycle == 1));
            }
        }
        // Drop all production handles before the durable-state oracle.
        let conn = rusqlite::Connection::open(&database).unwrap();
        assert_eq!(
            conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i32>(0))
                .unwrap(),
            28
        );
        let (status, local_path): (String, Option<String>) = conn.query_row(
            "SELECT status,local_path FROM assets WHERE library='PrimarySync' AND id='asset-recovered' AND version_size='original'",
            [], |r| Ok((r.get(0)?,r.get(1)?))
        ).unwrap();
        assert_eq!(
            status,
            if cycle == 0 { "pending" } else { "downloaded" },
            "cycle {cycle}"
        );
        let token: String = conn
            .query_row(
                "SELECT value FROM metadata WHERE key='sync_token:PrimarySync'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            token,
            if cycle == 0 { "before" } else { "after" },
            "cycle {cycle}"
        );
        assert_eq!(conn.query_row("SELECT master_record_name FROM asset_master_mappings WHERE library='PrimarySync' AND asset_record_name='asset-recovered'", [], |r| r.get::<_, String>(0)).unwrap(), "recovered");
        // Retry routing resets failed to pending before attempting the write.
        // Pending remains durable work, never a falsely completed download.
        if cycle == 0 {
            assert!(local_path.is_none());
        }
        if cycle > 0 {
            assert_eq!(std::fs::read(local_path.as_ref().unwrap()).unwrap(), bytes);
            if cycle == 1 {
                recovered_path = local_path;
            } else {
                assert_eq!(local_path, recovered_path);
            }
            assert_eq!(server.received_requests().await.unwrap().len(), 1);
        }
        assert_eq!(
            std::fs::read(&original).unwrap(),
            b"synthetic retained original"
        );
        assert_eq!(
            std::fs::read(&sidecar).unwrap(),
            b"synthetic independent metadata"
        );
    }
    server.verify().await;
}
