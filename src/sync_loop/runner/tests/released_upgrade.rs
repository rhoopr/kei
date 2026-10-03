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

/// The schema comes from v0.24.0; all catalogue rows and operations below are
/// synthetic. This exercises the production refresh tail, not historical sync.
#[cfg(feature = "xmp")]
#[tokio::test]
async fn released_v0240_metadata_policy_selection_restart_preserves_siblings() {
    use std::time::{Duration, UNIX_EPOCH};
    use xmp_toolkit::{XmpMeta, xmp_ns};

    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("released.db");
    let conn = rusqlite::Connection::open(&database).unwrap();
    conn.execute_batch(include_str!(
        "../../../../tests/data/released-v0240-schema.sql"
    ))
    .unwrap();
    assert_eq!(
        conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i32>(0))
            .unwrap(),
        25
    );
    let bytes = include_bytes!("../../../../tests/data/media/pattern.jpg");
    let checksum = format!("{:x}", Sha256::digest(bytes));
    let provider_checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(bytes));
    let fixtures = [
        ("PrimarySync", "same", "primary.jpg", 3, false, false),
        ("SharedSync-synthetic", "same", "shared.jpg", 5, true, false),
        ("PrimarySync", "deleted", "deleted.jpg", 1, true, true),
    ];
    let time = UNIX_EPOCH + Duration::from_secs(12345);
    for (library, id, filename, rating, hidden, deleted) in fixtures {
        let path = dir.path().join(filename);
        std::fs::write(&path, bytes).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(time))
            .unwrap();
        conn.execute("INSERT INTO assets(library,id,version_size,checksum,filename,created_at,size_bytes,media_type,status,local_path,last_seen_at,local_checksum,download_checksum,rating,title,is_hidden,is_deleted,deleted_at,metadata_hash,metadata_write_failed_at) VALUES (?1,?2,'original',?3,?4,1700000000,?5,'photo','downloaded',?6,1700000000,?7,?7,?8,?4,?9,?10,?11,?4,1700000001)",
            rusqlite::params![library,id,provider_checksum,filename,bytes.len() as i64,path.to_str().unwrap(),checksum,rating,hidden,deleted,deleted.then_some(1700000001)]).unwrap();
        conn.execute("INSERT INTO asset_metadata_paths(library,id,version_size,local_path,provider_checksum,local_checksum,download_checksum,metadata_write_failed_at,source_checksum) VALUES (?1,?2,'original',?3,?4,?5,?5,1700000001,?5)",
            rusqlite::params![library,id,path.to_str().unwrap(),provider_checksum,checksum]).unwrap();
        // Explicit identities avoid inferring an owner from a released row.
        conn.execute(
            "INSERT INTO asset_master_mappings VALUES (?1,?2,?3,1700000000)",
            rusqlite::params![library, id, format!("master-{library}")],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO asset_metadata_capture_revisions VALUES (?1,?2,1,1700000000)",
            rusqlite::params![library, id],
        )
        .unwrap();
    }
    conn.execute_batch("INSERT INTO metadata VALUES ('sync_token:PrimarySync','primary-before'),('sync_token:SharedSync-synthetic','shared-before');").unwrap();
    let immutable_rows = |connection: &rusqlite::Connection| {
        let mut stmt = connection.prepare("SELECT library,id,version_size,checksum,filename,created_at,size_bytes,status,local_path,local_checksum,download_checksum,rating,title,is_hidden,is_deleted,deleted_at,metadata_hash FROM assets ORDER BY library,id").unwrap();
        stmt.query_map([], |row| {
            (0..17)
                .map(|index| row.get::<_, rusqlite::types::Value>(index))
                .collect::<Result<Vec<_>, _>>()
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
    };
    let before = immutable_rows(&conn);
    drop(conn);
    let unrelated = dir.path().join("unrelated.xmp");
    std::fs::write(&unrelated, b"independent user metadata").unwrap();
    let deleted_sidecar = dir.path().join("deleted.jpg.xmp");
    std::fs::write(
        &deleted_sidecar,
        b"deleted sibling metadata must remain frozen",
    )
    .unwrap();
    let mut completed_packets = std::collections::BTreeMap::new();
    for cycle in 0..5 {
        let metadata = crate::config::MetadataConfig {
            xmp_sidecar: cycle > 0,
            ..Default::default()
        };
        let scope = if cycle < 3 {
            vec!["PrimarySync"]
        } else {
            vec!["PrimarySync", "SharedSync-synthetic"]
        };
        {
            let db = state::SqliteStateDb::open(&database).await.unwrap();
            assert_eq!(
                download::drain_pending_metadata_rewrites(
                    &db,
                    &metadata,
                    download::CaptureTimestampRepair::Preserve,
                    &scope,
                    Arc::from(".metadata-tmp"),
                    &CancellationToken::new(),
                )
                .await,
                0,
                "cycle {cycle}"
            );
        }
        let conn = rusqlite::Connection::open(&database).unwrap();
        assert_eq!(
            conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i32>(0))
                .unwrap(),
            28
        );
        assert_eq!(immutable_rows(&conn), before, "cycle {cycle}");
        for (library, id, filename, rating, _, deleted) in fixtures {
            let complete = !deleted
                && if library == "PrimarySync" {
                    cycle > 0
                } else {
                    cycle >= 3
                };
            let marker: Option<i64> = conn
                .query_row(
                    "SELECT metadata_write_failed_at FROM assets WHERE library=?1 AND id=?2",
                    [library, id],
                    |row| row.get(0),
                )
                .unwrap();
            let receipt_marker: Option<i64> = conn.query_row("SELECT metadata_write_failed_at FROM asset_metadata_paths WHERE library=?1 AND id=?2", [library,id], |row| row.get(0)).unwrap();
            assert_eq!(
                marker,
                if complete { None } else { Some(1700000001) },
                "{library}/{id}, cycle {cycle}"
            );
            assert_eq!(receipt_marker, marker);
            assert_eq!(conn.query_row("SELECT master_record_name FROM asset_master_mappings WHERE library=?1 AND asset_record_name=?2", [library,id], |row| row.get::<_,String>(0)).unwrap(), format!("master-{library}"));
            assert_eq!(conn.query_row("SELECT revision FROM asset_metadata_capture_revisions WHERE library=?1 AND asset_id=?2", [library,id], |row| row.get::<_,i64>(0)).unwrap(), 1);
            let path = dir.path().join(filename);
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
            assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), time);
            let sidecar = dir.path().join(format!("{filename}.xmp"));
            if deleted {
                assert_eq!(
                    std::fs::read(&sidecar).unwrap(),
                    b"deleted sibling metadata must remain frozen"
                );
            } else if complete {
                let packet = std::fs::read_to_string(&sidecar).unwrap();
                let xmp: XmpMeta = packet.parse().unwrap();
                assert_eq!(
                    xmp.property(xmp_ns::XMP, "Rating").unwrap().value,
                    rating.to_string()
                );
                assert!(
                    xmp.property(xmp_ns::DC, "title[1]")
                        .unwrap()
                        .value
                        .contains(filename)
                );
                let observed = (
                    packet,
                    std::fs::metadata(&sidecar).unwrap().modified().unwrap(),
                );
                if let Some(previous) = completed_packets.get(filename) {
                    assert_eq!(&observed, previous, "completed publication must stay quiet");
                } else {
                    completed_packets.insert(filename, observed);
                }
            } else {
                assert!(!sidecar.exists(), "unselected debt must remain unwritten");
            }
        }
        // Refreshing local metadata does not advance provider checkpoints.
        for (library, token) in [
            ("PrimarySync", "primary-before"),
            ("SharedSync-synthetic", "shared-before"),
        ] {
            assert_eq!(
                conn.query_row(
                    "SELECT value FROM metadata WHERE key=?1",
                    [format!("sync_token:{library}")],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
                token
            );
        }
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM metadata_capture_retries", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap(),
            0
        );
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM asset_metadata_paths", [], |row| row
                .get::<_, i64>(
                0
            ))
            .unwrap(),
            3
        );
        assert_eq!(
            std::fs::read(&unrelated).unwrap(),
            b"independent user metadata"
        );
        assert_eq!(
            std::fs::read_dir(dir.path())
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().contains("metadata-tmp"))
                .count(),
            0
        );
    }
}
