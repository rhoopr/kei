//! Released-schema recovery through the production cycle owner.
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use base64::Engine as _;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

use crate::sync_loop::test_support::{
    album_count_response, full_album_page_with_download, make_run_cycle_config,
    make_run_cycle_download_config_builder, make_run_cycle_library_state_with_passes,
    make_shared_session_for_run_cycle, run_cycle_account_owner,
};
use crate::{download, state};

#[derive(Clone, Debug)]
struct UpgradeSession {
    records: serde_json::Value,
    quiet: bool,
    inventory_queries: Arc<AtomicUsize>,
    delta_queries: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl crate::icloud::photos::PhotosSession for UpgradeSession {
    async fn post(
        &self,
        url: &str,
        body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<serde_json::Value> {
        if url.contains("/zones/list") {
            return Ok(
                serde_json::json!({"zones":[{"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"},"syncToken":"fresh-boundary","deleted":false}]}),
            );
        }
        let request: serde_json::Value = serde_json::from_str(&body)?;
        if request["query"]["recordType"] == "CheckIndexingState" {
            return Ok(
                serde_json::json!({"records":[{"recordName":"index-state","recordType":"CheckIndexingState","fields":{"state":{"value":"FINISHED"}}}]}),
            );
        }
        if url.contains("/internal/records/query/batch") {
            return Ok(album_count_response(1));
        }
        if url.contains("/records/query?") {
            assert!(!self.quiet, "completed history must not inventory again");
            self.inventory_queries.fetch_add(1, Ordering::SeqCst);
            return Ok(
                serde_json::json!({"records":self.records,"syncToken":"query-not-checkpoint"}),
            );
        }
        if url.contains("/changes/zone?") {
            self.delta_queries.fetch_add(1, Ordering::SeqCst);
            assert_ne!(
                request["zones"][0]["syncToken"], "before",
                "released cursor must not cross the v27 provider boundary"
            );
            return Ok(
                serde_json::json!({"zones":[{"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"},"syncToken":"after","moreComing":false,"records":[]}]}),
            );
        }
        assert!(
            url.contains("/records/lookup?"),
            "unexpected provider call: {url}"
        );
        assert!(!self.quiet, "completed history must not hydrate again");
        assert_eq!(request["zoneID"]["zoneName"], "PrimarySync");
        let mut selected = Vec::new();
        for record in request["records"].as_array().unwrap() {
            let name = record["recordName"].as_str().unwrap();
            assert!(["asset-recovered", "recovered"].contains(&name));
            selected.push(
                self.records
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|value| value["recordName"] == name)
                    .expect("requested fixture record")
                    .clone(),
            );
        }
        Ok(serde_json::json!({"records":selected}))
    }

    fn clone_box(&self) -> Box<dyn crate::icloud::photos::PhotosSession> {
        Box::new(self.clone())
    }
}

#[tokio::test]
async fn released_v0240_failed_history_recovers_after_upgrade_and_restart() {
    let (trace, _trace_guard) = crate::test_helpers::TracingCapture::install();
    let phase =
        std::env::var("KEI_TEST_RELEASED_HISTORY_PHASE").unwrap_or_else(|_| "component".to_owned());
    assert!(
        matches!(phase.as_str(), "component" | "prefix" | "due"),
        "unknown released history test phase"
    );
    let temporary = (phase == "component").then(|| tempfile::tempdir().unwrap());
    let fixture = if let Some(temporary) = &temporary {
        temporary.path().join("history")
    } else {
        std::path::PathBuf::from(
            std::env::var_os("KEI_TEST_RELEASED_HISTORY_PATH")
                .expect("persistent released history fixture path"),
        )
    };
    let due = phase == "due";
    // Deliberately no start_wiremock_or_skip: inability to bind must fail this
    // qualification rather than reporting an unexecuted preservation proof.
    let server = wiremock::MockServer::start().await;
    if !due {
        std::fs::create_dir(&fixture).expect("create the new fixture at its final path");
    }
    let bytes = b"\xff\xd8\xff\xe0\x00\x10JFIF\x00\x01\x01\x00\x00\x01\x00\x01\x00\x00\xff\xd9";
    let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(bytes));
    let mut generation = Sha256::new();
    generation.update(b"kei-provider-generation-v1\0");
    generation.update(Sha256::digest(bytes));
    let staging_generation = data_encoding::BASE32_NOPAD.encode(&generation.finalize());
    Mock::given(method("GET"))
        .and(path("/recovered.jpg"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes))
        .expect(u64::from(!due))
        .mount(&server)
        .await;
    let mut records = full_album_page_with_download(
        "PrimarySync",
        "recovered",
        "after",
        &format!("{}/recovered.jpg", server.uri()),
        bytes.len() as u64,
        &checksum,
    )["records"]
        .clone();
    let zone = serde_json::json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"});
    for record in records.as_array_mut().unwrap() {
        record["zoneID"] = zone.clone();
        if record["recordType"] == "CPLAsset" {
            record["fields"]["masterRef"]["value"]["zoneID"] = zone.clone();
        }
    }
    let legacy = fixture.join("upgradeexampleinvalid.db");
    let database = fixture.join("adopted.db");
    let media = fixture.join("media");
    let original = media.join("retained-original.jpg");
    let sidecar = media.join("retained-original.xmp");
    if !due {
        std::fs::create_dir(&media).unwrap();
        if let Some(binary) = std::env::var_os("KEI_TEST_RELEASED_V0240") {
            let config = fixture.join("config.toml");
            std::fs::write(
                &config,
                format!(
                    "[download]\ndirectory = {}\n",
                    serde_json::to_string(&media).unwrap()
                ),
            )
            .unwrap();
            std::fs::write(&legacy, []).unwrap();
            let output = std::process::Command::new(binary)
                .env_clear()
                .env("ICLOUD_USERNAME", "upgrade@example.invalid")
                .env("KEI_DATA_DIR", &fixture)
                .arg("--config")
                .arg(&config)
                .arg("verify")
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
        } else {
            rusqlite::Connection::open(&legacy)
                .unwrap()
                .execute_batch(include_str!(
                    "../../../../tests/data/released-v0240-schema.sql"
                ))
                .unwrap();
        }
        let conn = rusqlite::Connection::open(&legacy).unwrap();
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
        crate::state::db::account::adopt_legacy(
            &legacy,
            &database,
            &run_cycle_account_owner(),
            "test@example.com",
            "com",
        )
        .await
        .unwrap();
        std::fs::write(&original, b"synthetic retained original").unwrap();
        std::fs::write(&sidecar, b"synthetic independent metadata").unwrap();
    }
    let mut config = make_run_cycle_config();
    config.filters.selection.albums = crate::selection::AlbumSelector::None;
    config.filters.selection.unfiled = true;
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
    let evidence_path = fixture.join("prefix-evidence.json");
    let mtime = |path: &std::path::Path| {
        std::fs::metadata(path)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
            .to_string()
    };
    let original_mtime = mtime(&original);
    let sidecar_mtime = mtime(&sidecar);
    let legacy_hash = format!("{:x}", Sha256::digest(std::fs::read(&legacy).unwrap()));
    let evidence: Option<serde_json::Value> = due.then(|| {
        serde_json::from_slice(&std::fs::read(&evidence_path).expect("completed prefix evidence"))
            .unwrap()
    });
    let mut recovered_path = evidence
        .as_ref()
        .map(|v| v["recovered_path"].as_str().unwrap().to_owned());
    let mut recovered_receipt = evidence
        .as_ref()
        .map(|v| v["receipt"].as_str().unwrap().to_owned());
    let mut held_deadline = evidence.as_ref().map(|v| v["deadline"].as_i64().unwrap());
    if let Some(deadline) = held_deadline {
        assert!(
            chrono::Utc::now().timestamp() >= deadline,
            "the real retry deadline has not elapsed"
        );
        let conn = rusqlite::Connection::open(&database).unwrap();
        let epoch = crate::state::db::sync_epoch::read_library(&conn, "PrimarySync")
            .unwrap()
            .unwrap();
        assert_eq!(epoch.state, "pending");
        let token: Option<String> = conn
            .query_row(
                "SELECT value FROM metadata WHERE key='sync_token:v27:PrimarySync'",
                [],
                |row| row.get(0),
            )
            .ok();
        assert_eq!(token, None, "the prefix must not have checkpoint credit");
        assert_eq!(
            epoch.next_attempt_at, deadline,
            "the persisted natural deadline must remain unchanged"
        );
    }
    for cycle in if due { 3..5 } else { 0..3 } {
        {
            use crate::commands::{
                build_collection_context, pass_scope_for_zone, resolve_passes_for_scope,
                zone_name_set,
            };
            let owner = run_cycle_account_owner();
            let db = Arc::new(
                state::SqliteStateDb::open_owned(&database, &owner)
                    .await
                    .unwrap(),
            );
            if cycle == 0 {
                db.acquire_lock("inject recovered task temp ownership failure").unwrap().execute_batch(&format!(
                    "CREATE TRIGGER fail_recovered_temp BEFORE INSERT ON owned_temp_files WHEN instr(CAST(NEW.path AS TEXT),'{staging_generation}')>0 AND EXISTS(SELECT 1 FROM assets WHERE library='PrimarySync' AND id='asset-recovered' AND version_size='original' AND status='pending') BEGIN SELECT RAISE(ABORT,'injected recovered temp ownership failure'); END;
                     CREATE TRIGGER fail_recovered_failure_write BEFORE UPDATE OF status ON assets WHEN OLD.library='PrimarySync' AND OLD.id='asset-recovered' AND OLD.version_size='original' AND OLD.status='pending' AND NEW.status='failed' BEGIN SELECT RAISE(ABORT,'injected recovered failure-state write failure'); END;"
                )).unwrap();
            }
            let inventory_queries = Arc::new(AtomicUsize::new(0));
            let delta_queries = Arc::new(AtomicUsize::new(0));
            let mut library = crate::icloud::photos::PhotoLibrary::new(
                "https://example.invalid/database/1/photos/production/private".into(),
                Arc::default(),
                Box::new(UpgradeSession {
                    records: records.clone(),
                    quiet: cycle == 2 || cycle == 4,
                    inventory_queries: inventory_queries.clone(),
                    delta_queries: delta_queries.clone(),
                }),
                Arc::new(zone.clone()),
                "private".into(),
                crate::retry::RetryConfig::default(),
            )
            .await
            .unwrap();
            library.shadow_capture = Some(crate::icloud::photos::inbox::ShadowCapture::new(
                db.clone(),
                owner,
                "com",
            ));
            let selection = &config.filters.selection;
            let libraries = std::slice::from_ref(&library);
            let zones = zone_name_set(libraries);
            let context = build_collection_context(selection, libraries)
                .await
                .unwrap();
            let scope = pass_scope_for_zone(selection, "PrimarySync", &zones, &zones);
            let plan = resolve_passes_for_scope(&library, selection, scope, &context, &[])
                .await
                .unwrap();
            let mut primary = make_run_cycle_library_state_with_passes(
                "PrimarySync",
                &crate::sync_cycle::sync_token_key("PrimarySync"),
                plan.passes,
            );
            primary.library = library;
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
            if cycle == 3 {
                assert!(
                    inventory_queries.load(Ordering::SeqCst) > 0,
                    "the due cycle must run its own full inventory"
                );
                assert!(
                    delta_queries.load(Ordering::SeqCst) > 0,
                    "the due cycle must run its own changes bridge"
                );
            }
            if cycle == 0 {
                let events = trace.events();
                let temp_failures = events
                    .iter()
                    .filter(|event| {
                        event.message() == Some("Download failed")
                            && event.field("asset_id") == Some("asset-recovered")
                            && event.field("error")
                                == Some("Could not record temporary-file ownership")
                    })
                    .count();
                assert!(
                    temp_failures > 0,
                    "the exact recovered task must reach and fail temp ownership: {events:?}"
                );
                assert!(
                    events
                        .iter()
                        .any(|event| event.message() == Some("Failed to mark failure")
                            && event.field("asset_id") == Some("asset-recovered")
                            && event.field("error").is_some_and(|error| error
                                .contains("injected recovered failure-state write failure"))),
                    "the pending row must be retained by the same scoped database-fault episode: {events:?}"
                );
                assert_eq!(
                    server.received_requests().await.unwrap().len(),
                    0,
                    "the ownership fault must occur before the recovered GET"
                );
            }
            if cycle == 0 {
                db.acquire_lock("remove injected temp ownership fault")
                    .unwrap()
                    .execute_batch("DROP TRIGGER fail_recovered_temp; DROP TRIGGER fail_recovered_failure_write")
                    .unwrap();
            }
            if cycle == 0 {
                assert!(
                    result.failed_count > 0 || result.stats.state_write_failures > 0,
                    "{result:?}"
                );
            } else {
                assert_eq!(result.failed_count, 0);
                assert!(!result.stats.identity_incomplete);
                assert_eq!(
                    result.stats.downloaded,
                    usize::from(cycle == 1),
                    "cycle {cycle}: {result:?}"
                );
            }
        }
        // Drop all production handles before the durable-state oracle.
        let conn = rusqlite::Connection::open(&database).unwrap();
        assert_eq!(
            conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i32>(0))
                .unwrap(),
            crate::state::schema::SCHEMA_VERSION
        );
        let (status, local_path): (String, Option<String>) = conn.query_row(
            "SELECT status,local_path FROM assets WHERE library='PrimarySync' AND id='asset-recovered' AND version_size='original'",
            [], |r| Ok((r.get(0)?,r.get(1)?))
        ).unwrap();
        if cycle == 0 {
            let admitted: i64 = conn.query_row(
                "SELECT COUNT(*) FROM provider_active_destinations WHERE library='PrimarySync' AND child='asset-recovered' AND admitted=1",
                [], |row| row.get(0),
            ).unwrap();
            assert_eq!(
                admitted, 1,
                "the recovered task must have an owner-admitted destination"
            );
        }
        assert_eq!(
            status,
            if cycle == 0 { "pending" } else { "downloaded" },
            "cycle {cycle}"
        );
        let token: Option<String> = conn
            .query_row(
                "SELECT value FROM metadata WHERE key='sync_token:v27:PrimarySync'",
                [],
                |r| r.get(0),
            )
            .ok();
        let epoch = crate::state::db::sync_epoch::read_library(&conn, "PrimarySync")
            .unwrap()
            .unwrap();
        if due {
            assert_eq!(
                token.as_deref(),
                Some("after"),
                "the due baseline must advance its own checkpoint"
            );
            assert_eq!(
                epoch.state, "complete",
                "the due baseline must complete the epoch"
            );
        } else {
            assert_eq!(
                token, None,
                "retrying held work must not grant epoch checkpoint credit, cycle {cycle}"
            );
            assert_eq!(epoch.state, "pending");
            assert!(epoch.binding.is_none());
            if let Some(deadline) = held_deadline {
                assert_eq!(
                    epoch.next_attempt_at, deadline,
                    "retry and quiet reopen must not change the epoch deadline"
                );
            } else {
                assert!(epoch.next_attempt_at > chrono::Utc::now().timestamp());
                held_deadline = Some(epoch.next_attempt_at);
            }
        }
        assert_eq!(conn.query_row("SELECT master_record_name FROM asset_master_mappings WHERE library='PrimarySync' AND asset_record_name='asset-recovered'", [], |r| r.get::<_, String>(0)).unwrap(), "recovered");
        // Retry routing resets failed to pending before attempting the write.
        // Pending remains durable work, never a falsely completed download.
        if cycle == 0 {
            assert!(local_path.is_none());
            let receipts: i64 = conn.query_row("SELECT COUNT(*) FROM download_receipts WHERE library='PrimarySync' AND state_id='asset-recovered' AND version_size='original'", [], |row| row.get(0)).unwrap();
            assert_eq!(receipts, 0);
        }
        if cycle > 0 {
            assert_eq!(std::fs::read(local_path.as_ref().unwrap()).unwrap(), bytes);
            if cycle == 1 {
                recovered_path = local_path;
            } else {
                assert_eq!(local_path, recovered_path);
            }
            assert_eq!(
                server.received_requests().await.unwrap().len(),
                usize::from(!due)
            );
            let receipt: String = conn.query_row("SELECT json_array(account_key,provider_key,scope,library,child,master,state_id,version_size,provider_checksum,provider_size,native_path,source_checksum,local_checksum,proof_hash) FROM download_receipts WHERE library='PrimarySync' AND child='asset-recovered' AND master='recovered' AND state_id='asset-recovered' AND version_size='original' AND provider_checksum=?1 AND provider_size=?2", rusqlite::params![checksum,bytes.len() as i64], |row| row.get(0)).unwrap();
            if let Some(previous) = &recovered_receipt {
                assert_eq!(
                    &receipt, previous,
                    "quiet reopen must retain the same receipt"
                );
            } else {
                recovered_receipt = Some(receipt);
            }
        }
        assert_eq!(
            std::fs::read(&original).unwrap(),
            b"synthetic retained original"
        );
        assert_eq!(
            std::fs::read(&sidecar).unwrap(),
            b"synthetic independent metadata"
        );
        assert_eq!(mtime(&original), original_mtime);
        assert_eq!(mtime(&sidecar), sidecar_mtime);
        assert_eq!(
            format!("{:x}", Sha256::digest(std::fs::read(&legacy).unwrap())),
            legacy_hash
        );
        let run_history: (i64, i64, String) = conn
            .query_row(
                "SELECT started_at,completed_at,status FROM sync_runs WHERE started_at=1700000000",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(run_history, (1700000000, 1700000001, "complete".to_owned()));
        if let Some(evidence) = &evidence {
            for (key, path) in [
                ("original_mtime", original.as_path()),
                ("sidecar_mtime", sidecar.as_path()),
                (
                    "recovered_mtime",
                    std::path::Path::new(recovered_path.as_ref().unwrap()),
                ),
            ] {
                let modified = std::fs::metadata(path)
                    .unwrap()
                    .modified()
                    .unwrap()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
                    .to_string();
                assert_eq!(
                    evidence[key].as_str(),
                    Some(modified.as_str()),
                    "{key} must remain unchanged"
                );
            }
            let legacy_hash = format!("{:x}", Sha256::digest(std::fs::read(&legacy).unwrap()));
            assert_eq!(evidence["legacy_hash"].as_str(), Some(legacy_hash.as_str()));
        }
    }
    server.verify().await;
    if !due {
        let path = recovered_path.as_ref().unwrap();
        let evidence = serde_json::json!({
            "deadline": held_deadline.unwrap(),
            "recovered_path": path,
            "receipt": recovered_receipt.unwrap(),
            "original_mtime": mtime(&original),
            "sidecar_mtime": mtime(&sidecar),
            "recovered_mtime": mtime(std::path::Path::new(path)),
            "legacy_hash": format!("{:x}", Sha256::digest(std::fs::read(&legacy).unwrap())),
        });
        std::fs::write(
            &evidence_path,
            serde_json::to_vec_pretty(&evidence).unwrap(),
        )
        .unwrap();
        eprintln!(
            "{} deadline={}",
            if temporary.is_some() {
                "RELEASED_HISTORY_PREFIX_COMPONENT"
            } else {
                "RELEASED_HISTORY_PREFIX_ONLY"
            },
            held_deadline.unwrap()
        );
        return;
    }
    eprintln!(
        "RELEASED_HISTORY_NATURAL_DUE_COMPLETE deadline={}",
        held_deadline.unwrap()
    );
    // Exercise the same authenticated migration boundary with reconstructed
    // history. Each case is independent and owns its server, database and
    // expected tree.
    for case in [
        super::v027_baseline::released_schema::ReleasedSchemaCase::ForeignOwnerNegative,
        super::v027_baseline::released_schema::ReleasedSchemaCase::CurrentIdentityReuse,
        super::v027_baseline::released_schema::ReleasedSchemaCase::TailCorruption,
    ] {
        super::v027_baseline::PRESERVE_SHARED_CLAIMS
            .scope(
                true,
                super::v027_baseline::released_schema::exercise_released_schema(case),
            )
            .await;
    }
}

/// Populated released state from the genuine schema-28 constructor. Every row is
/// synthetic; the production adoption and cycle own enrollment, reuse,
/// publication and checkpoint advancement.
#[tokio::test]
async fn released_populated_same_identity_reuse_28() {
    super::v027_baseline::PRESERVE_SHARED_CLAIMS
        .scope(
            true,
            super::v027_baseline::released_schema::exercise_released_populated(
                super::v027_baseline::released_schema::ReleasedPopulatedCase::SameIdentityReuse28,
            ),
        )
        .await;
}

/// Populated released state from the genuine schema-36 constructor with the
/// same-identity still and motion both reused locally.
#[tokio::test]
async fn released_populated_same_identity_reuse_36() {
    super::v027_baseline::PRESERVE_SHARED_CLAIMS
        .scope(
            true,
            super::v027_baseline::released_schema::exercise_released_populated(
                super::v027_baseline::released_schema::ReleasedPopulatedCase::SameIdentityReuse36,
            ),
        )
        .await;
}

/// Populated released state from the genuine schema-36 constructor with a
/// recorded failed live companion. Every row is synthetic; the production
/// adoption and cycle own enrollment, local reuse, expired-URL refresh and
/// publish at the original companion stem.
///
#[tokio::test]
async fn released_populated_expired_companion_refresh_36() {
    super::v027_baseline::PRESERVE_SHARED_CLAIMS
        .scope(
            true,
            super::v027_baseline::released_schema::exercise_released_populated(
                super::v027_baseline::released_schema::ReleasedPopulatedCase::MotionRefresh36,
            ),
        )
        .await;
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
                    dir.path(),
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
            crate::state::schema::SCHEMA_VERSION
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
