//! Preservation owner-read acceptance on a genuine v36 fixture: opaque
//! immutable original evidence, exact recorded path claims, real normal cycle,
//! and prework owner/epoch/attempt/scope binding.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::SystemTime;

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

use crate::icloud::photos::{PhotoLibrary, PhotosSession};
use crate::state::SqliteStateDb;
use crate::sync_cycle::{run_cycle, sync_token_key};
use crate::sync_loop::test_support::{
    album_count_response, make_run_cycle_download_config_builder,
    make_run_cycle_library_state_with_passes, make_shared_session_for_run_cycle,
};

use super::{MEDIA, OLD_TOKEN, ZONE, baseline_owner, pair, zone};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Variant {
    MissingPath,
    MalformedFiles,
    MissingProof,
    Traversal,
}

#[derive(Clone)]
struct ProtectedSession {
    records: Arc<Vec<Value>>,
    zone: Value,
    cycle: usize,
    inventories: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl PhotosSession for ProtectedSession {
    async fn post(
        &self,
        url: &str,
        body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        let request: Value = serde_json::from_str(&body)?;
        if url.contains("/zones/list") {
            return Ok(json!({"zones":[{"zoneID":self.zone,
                "syncToken":"preservation-boundary","deleted":false}]}));
        }
        if request["query"]["recordType"] == "CheckIndexingState" {
            return Ok(json!({"records":[{"recordName":"index",
                "recordType":"CheckIndexingState",
                "fields":{"state":{"value":"FINISHED"}}}]}));
        }
        if url.contains("/internal/records/query/batch") {
            return Ok(album_count_response(self.records.len() as u64 / 2));
        }
        if url.contains("/records/query?") {
            self.inventories.fetch_add(1, Ordering::SeqCst);
            let offset = request["query"]["filterBy"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|filter| filter["fieldName"] == "startRank")
                .and_then(|filter| filter["fieldValue"]["value"].as_u64())
                .unwrap_or(0);
            let records = if offset == 0 {
                self.records.as_ref().clone()
            } else {
                Vec::new()
            };
            return Ok(json!({"records":records,"syncToken":"preservation-query"}));
        }
        if url.contains("/records/lookup?") {
            let requested = request["records"].as_array().cloned().unwrap_or_default();
            let records: Vec<_> = self
                .records
                .iter()
                .filter(|record| {
                    requested
                        .iter()
                        .any(|query| query["recordName"] == record["recordName"])
                })
                .cloned()
                .collect();
            return Ok(json!({"records":records}));
        }
        if url.contains("/changes/zone?") {
            return Ok(json!({"zones":[{"zoneID":self.zone,
                "records":[],
                "syncToken":format!("preservation-after-{}", self.cycle),
                "moreComing":false}]}));
        }
        anyhow::bail!("unexpected preservation owner endpoint: {url}")
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

fn make_app_config() -> crate::config::Config {
    let data_dir = tempfile::tempdir().unwrap();
    let globals = crate::config::GlobalArgs {
        username: Some("baseline@example.invalid".to_string()),
        domain: None,
        data_dir: Some(data_dir.path().to_string_lossy().into_owned()),
    };
    let mut sync = crate::cli::SyncArgs::default();
    sync.config_overrides.albums = vec!["none".to_string()];
    sync.config_overrides.unfiled = Some(true);
    crate::config::Config::build(&globals, &crate::cli::PasswordArgs::default(), sync, None)
        .unwrap()
}

fn history(db: &std::path::Path) -> Vec<Vec<String>> {
    let conn = rusqlite::Connection::open(db).unwrap();
    let mut rows: Vec<Vec<String>> = conn
        .prepare(
            "SELECT library,asset_id,evidence_version,files,prepared_at,active_generation \
             FROM unattributed_legacy ORDER BY library,asset_id",
        )
        .unwrap()
        .query_map([], |row| {
            Ok((0..6)
                .map(|index| match row.get_ref(index).unwrap() {
                    rusqlite::types::ValueRef::Null => "null".to_string(),
                    rusqlite::types::ValueRef::Integer(value) => value.to_string(),
                    rusqlite::types::ValueRef::Text(value) => {
                        String::from_utf8_lossy(value).into_owned()
                    }
                    rusqlite::types::ValueRef::Real(value) => value.to_string(),
                    rusqlite::types::ValueRef::Blob(value) => data_encoding::HEXLOWER.encode(value),
                })
                .collect())
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    rows.extend(
        conn.prepare(
            "SELECT library,asset_id,path_key,local_path \
             FROM unattributed_legacy_paths ORDER BY library,asset_id,path_key",
        )
        .unwrap()
        .query_map([], |row| {
            Ok((0..4)
                .map(|index| match row.get_ref(index).unwrap() {
                    rusqlite::types::ValueRef::Null => "null".to_string(),
                    rusqlite::types::ValueRef::Text(value) => {
                        String::from_utf8_lossy(value).into_owned()
                    }
                    _ => "other".to_string(),
                })
                .collect())
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap(),
    );
    rows
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    db_path: PathBuf,
    preserved: Vec<PathBuf>,
    mtimes: Vec<SystemTime>,
    history: Vec<Vec<String>>,
}

async fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    std::fs::create_dir_all(&root).unwrap();
    let source = dir.path().join("released.db");
    let db_path = dir.path().join("state.db");
    let preserved = root.join("preserved.JPG");
    let preserved_sidecar = root.join("preserved.JPG.xmp");
    let custom = root.join("custom-notes.xmp");
    for path in [&preserved, &preserved_sidecar, &custom] {
        std::fs::write(path, b"opaque preservation bytes").unwrap();
    }
    let mtimes = [&preserved, &preserved_sidecar, &custom]
        .map(|path| std::fs::metadata(path).unwrap().modified().unwrap());
    {
        let conn = rusqlite::Connection::open(&source).unwrap();
        crate::state::schema::migrate_genuine_v36(&conn).unwrap();
        let files = json!([
            {"root": root, "path": preserved, "sha256": null, "size": null},
            {"root": root, "path": preserved_sidecar, "sha256": null, "size": null},
            {"root": root, "path": custom, "sha256": null, "size": null},
        ])
        .to_string();
        conn.execute(
            "INSERT INTO unattributed_legacy(library,asset_id,evidence_version,original_evidence,files,prepared_at,active_generation) \
             VALUES(?1,'legacy-preserved',1,'opaque preservation evidence',?2,1700000000,NULL)",
            rusqlite::params![ZONE, files],
        )
        .unwrap();
        for path in [&preserved, &preserved_sidecar, &custom] {
            conn.execute(
                "INSERT INTO unattributed_legacy_paths VALUES(?1,'legacy-preserved',?2,?3)",
                rusqlite::params![
                    ZONE,
                    crate::fs_util::confined_path_key(path).unwrap(),
                    path.to_str().unwrap()
                ],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO metadata VALUES (?1,?2)",
            rusqlite::params![format!("sync_token:{ZONE}"), OLD_TOKEN],
        )
        .unwrap();
    }
    crate::state::db::account::adopt_legacy(
        &source,
        &db_path,
        &baseline_owner(),
        "baseline@example.invalid",
        "com",
    )
    .await
    .unwrap();
    let history = history(&db_path);
    Fixture {
        _dir: dir,
        root,
        db_path,
        preserved: vec![preserved, preserved_sidecar, custom],
        mtimes: mtimes.to_vec(),
        history,
    }
}

fn insert_bad_row(fixture: &Fixture, variant: Variant) {
    let conn = rusqlite::Connection::open(&fixture.db_path).unwrap();
    let bad = fixture.root.join("bad-preserved.JPG");
    let bad_sidecar = fixture.root.join("bad-preserved.JPG.xmp");
    let bad_custom = fixture.root.join("bad-custom.xmp");
    let traversal = fixture.root.join("..").join("escape.JPG");
    let files =
        if variant == Variant::MalformedFiles {
            "[invalid".to_string()
        } else {
            let paths: Vec<&PathBuf> = match variant {
                Variant::Traversal => vec![&bad, &bad_sidecar, &traversal],
                _ => vec![&bad, &bad_sidecar, &bad_custom],
            };
            json!(paths
            .into_iter()
            .map(|path| json!({"root": fixture.root, "path": path, "sha256": null, "size": null}))
            .collect::<Vec<_>>())
            .to_string()
        };
    let active = (variant == Variant::MissingProof).then_some(1i64);
    conn.execute(
        "INSERT INTO unattributed_legacy(library,asset_id,evidence_version,original_evidence,files,prepared_at,active_generation) \
         VALUES(?1,'legacy-bad',1,'opaque bad evidence',?2,1700000001,?3)",
        rusqlite::params![ZONE, files, active],
    )
    .unwrap();
    let claims: Vec<PathBuf> = match variant {
        Variant::MissingPath => vec![bad.clone(), bad_sidecar.clone()],
        Variant::Traversal => vec![bad.clone(), bad_sidecar.clone(), traversal.clone()],
        _ => vec![bad.clone(), bad_sidecar.clone(), bad_custom.clone()],
    };
    for path in claims {
        let key = crate::fs_util::confined_path_key(&path)
            .unwrap_or_else(|_error| "invalid-traversal".to_owned());
        conn.execute(
            "INSERT INTO unattributed_legacy_paths VALUES(?1,'legacy-bad',?2,?3)",
            rusqlite::params![ZONE, key, path.to_str().unwrap()],
        )
        .unwrap();
    }
}

async fn run_cycle_at(
    fixture: &Fixture,
    cycle: usize,
    extra_zone: bool,
) -> (crate::sync_cycle::CycleResult, Arc<SqliteStateDb>, String) {
    let server = crate::test_helpers::start_wiremock_or_skip("preservation_hold_owner")
        .await
        .expect("preservation acceptance server");
    let expected_gets = if cycle == 0 { 1 } else { 0 };
    for endpoint in ["/current", "/new"] {
        Mock::given(method("GET"))
            .and(path(endpoint))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
            .expect(expected_gets)
            .mount(&server)
            .await;
    }
    let current = pair(
        "current-master",
        "current.JPG",
        &format!("{}/current", server.uri()),
        1,
    );
    let new_item = pair("new-master", "new.JPG", &format!("{}/new", server.uri()), 2);
    let mut inventory = current;
    inventory.extend(new_item);
    let db = Arc::new(
        SqliteStateDb::open_owned(&fixture.db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let config = make_app_config();
    let token_key = sync_token_key(ZONE);
    let zone_json = if extra_zone {
        json!({"zoneName":ZONE,"ownerRecordName":"_defaultOwner","zoneType":"REGULAR_CUSTOM_ZONE"})
    } else {
        zone()
    };
    let mut library = PhotoLibrary::new(
        "https://synthetic.example/database/1/photos/production/shared".to_string(),
        Arc::new(std::collections::HashMap::new()),
        Box::new(ProtectedSession {
            records: Arc::new(inventory),
            zone: zone_json.clone(),
            cycle,
            inventories: Arc::new(AtomicUsize::new(0)),
        }),
        Arc::new(zone_json),
        "shared".into(),
        crate::retry::RetryConfig::default(),
    )
    .await
    .unwrap();
    library.shadow_capture = Some(crate::icloud::photos::inbox::ShadowCapture::new(
        Arc::clone(&db),
        baseline_owner(),
        "com",
    ));
    let baseline_scope = library.baseline_scope();
    let selection = &config.filters.selection;
    let selected_zones = crate::commands::zone_name_set(std::slice::from_ref(&library));
    let collection_zones = selected_zones.clone();
    let resolved_scope =
        crate::commands::pass_scope_for_zone(selection, ZONE, &selected_zones, &collection_zones);
    let collection_context =
        crate::commands::build_collection_context(selection, std::slice::from_ref(&library))
            .await
            .unwrap();
    let plan = crate::commands::resolve_passes_for_scope(
        &library,
        selection,
        resolved_scope,
        &collection_context,
        &[],
    )
    .await
    .unwrap();
    let mut selected = make_run_cycle_library_state_with_passes(ZONE, &token_key, plan.passes);
    selected.library = library;
    let base = make_run_cycle_download_config_builder(&fixture.root, db.clone());
    let builder = |mode, exclude, groups, zone| {
        let mut config = (*base(mode, exclude, groups, zone)).clone();
        config.folder_structure = "none".into();
        config.file_match_policy = crate::types::FileMatchPolicy::NameId7;
        Arc::new(config)
    };
    let result = run_cycle(
        &[&selected],
        &config,
        Some(db.as_ref()),
        false,
        &builder,
        crate::download::DownloadControls::download_hidden(),
        &shared,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    (result, db, baseline_scope)
}

async fn identity(db: &SqliteStateDb) -> (String, String, String, String, String, String) {
    let epoch = db.sync_epoch_id().await.unwrap().unwrap();
    let attempt = {
        let conn = db.acquire_lock("preservation prework attempt").unwrap();
        crate::state::db::sync_epoch::read_library(&conn, ZONE)
            .unwrap()
            .unwrap()
            .attempt
    };
    let scope = db
        .get_metadata(&format!("download_ledger_scope:{ZONE}"))
        .await
        .unwrap()
        .unwrap();
    let (zone_name, owner, database) = PhotoLibrary::provider_scope_projection(&scope).unwrap();
    (epoch, attempt, scope, zone_name, owner, database)
}

#[tokio::test]
async fn preservation_owner_reads_exact_genuine36_retained_claims() {
    let fixture = fixture().await;
    let (baseline, db, baseline_scope) = run_cycle_at(&fixture, 0, false).await;
    assert_eq!(baseline.stats.downloaded, 2, "current media");
    assert!(baseline.stats.unattributed_legacy_assets >= 1);
    assert!(baseline.stats.unattributed_legacy_pending >= 1);
    let (epoch, attempt, scope, zone_name, owner, database) = identity(&db).await;
    let (quiet, db, _) = run_cycle_at(&fixture, 1, false).await;
    assert_eq!(quiet.stats.downloaded, 0, "quiet normal cycle");
    assert_eq!(history(&fixture.db_path), fixture.history);
    for (path, mtime) in fixture.preserved.iter().zip(&fixture.mtimes) {
        assert_eq!(
            std::fs::metadata(path).unwrap().modified().unwrap(),
            *mtime,
            "protected mtime"
        );
    }
    let proof = db
        .preservation_hold_test(
            ZONE,
            baseline_owner(),
            &epoch,
            &attempt,
            &scope,
            &zone_name,
            &owner,
            &database,
            &baseline_scope,
        )
        .await
        .unwrap();
    assert_eq!(proof, Ok((1, 3)));
}

#[tokio::test]
async fn preservation_owner_reads_full_zone_with_extra_projection() {
    let fixture = fixture().await;
    let (_, db, baseline_scope) = run_cycle_at(&fixture, 0, true).await;
    let (epoch, attempt, scope, zone_name, owner, database) = identity(&db).await;
    let proof = db
        .preservation_hold_test(
            ZONE,
            baseline_owner(),
            &epoch,
            &attempt,
            &scope,
            &zone_name,
            &owner,
            &database,
            &baseline_scope,
        )
        .await
        .unwrap();
    assert_eq!(proof, Ok((1, 3)));
}

async fn refusal(variant: Variant) -> &'static str {
    let fixture = fixture().await;
    let (_, db, baseline_scope) = run_cycle_at(&fixture, 0, false).await;
    insert_bad_row(&fixture, variant);
    let (epoch, attempt, scope, zone_name, owner, database) = identity(&db).await;
    db.preservation_hold_test(
        ZONE,
        baseline_owner(),
        &epoch,
        &attempt,
        &scope,
        &zone_name,
        &owner,
        &database,
        &baseline_scope,
    )
    .await
    .unwrap()
    .unwrap_err()
}

#[tokio::test]
async fn preservation_owner_refuses_missing_path_and_malformed_files() {
    assert_eq!(refusal(Variant::MissingPath).await, "path_set_mismatch");
    assert_eq!(refusal(Variant::MalformedFiles).await, "malformed_files");
    assert_eq!(
        refusal(Variant::MissingProof).await,
        "proof_journal_missing"
    );
    assert_eq!(refusal(Variant::Traversal).await, "path_escape");
}

#[tokio::test]
async fn preservation_owner_refuses_wrong_owner_projection_epoch_attempt() {
    let fixture = fixture().await;
    let (_, db, baseline_scope) = run_cycle_at(&fixture, 0, false).await;
    let (epoch, attempt, scope, zone_name, owner, database) = identity(&db).await;
    let wrong_login = serde_json::from_value(json!({"dsInfo":{"dsid":"other-provider"}})).unwrap();
    let wrong_owner = crate::state::db::account::AccountOwner::authenticated(
        "baseline@example.invalid",
        "com",
        &wrong_login,
    )
    .unwrap();
    assert_eq!(
        db.preservation_hold_test(
            ZONE,
            wrong_owner,
            &epoch,
            &attempt,
            &scope,
            &zone_name,
            &owner,
            &database,
            &baseline_scope,
        )
        .await
        .unwrap(),
        Err("owner_mismatch")
    );
    assert_eq!(
        db.preservation_hold_test(
            ZONE,
            baseline_owner(),
            &epoch,
            &attempt,
            &scope,
            "OtherZone",
            &owner,
            &database,
            &baseline_scope,
        )
        .await
        .unwrap(),
        Err("scope_projection_mismatch")
    );
    assert_eq!(
        db.preservation_hold_test(
            ZONE,
            baseline_owner(),
            "other-epoch",
            &attempt,
            &scope,
            &zone_name,
            &owner,
            &database,
            &baseline_scope,
        )
        .await
        .unwrap(),
        Err("epoch_changed")
    );
    assert_eq!(
        db.preservation_hold_test(
            ZONE,
            baseline_owner(),
            &epoch,
            "other-attempt",
            &scope,
            &zone_name,
            &owner,
            &database,
            &baseline_scope,
        )
        .await
        .unwrap(),
        Err("attempt_changed")
    );
}

#[tokio::test]
async fn preservation_owner_reads_one_transaction_snapshot() {
    let fixture = fixture().await;
    let (_, db, baseline_scope) = run_cycle_at(&fixture, 0, false).await;
    let (epoch, attempt, scope, zone_name, owner, database) = identity(&db).await;
    let db_path = fixture.db_path.clone();
    SqliteStateDb::set_preservation_interleave(Box::new(move || {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let path = db_path
            .parent()
            .unwrap()
            .join("photos")
            .join("new-preserved.JPG");
        conn.execute(
            "INSERT INTO unattributed_legacy(library,asset_id,evidence_version,original_evidence,files,prepared_at,active_generation) \
             VALUES(?1,'legacy-preserved-2',1,'opaque second evidence',?2,1700000001,NULL)",
            rusqlite::params![
                ZONE,
                json!([{"root": db_path.parent().unwrap().join("photos"), "path": path, "sha256": null, "size": null}]).to_string()
            ],
        )
        .unwrap();
        let key = crate::fs_util::confined_path_key(&path).unwrap();
        conn.execute(
            "INSERT INTO unattributed_legacy_paths VALUES(?1,'legacy-preserved-2',?2,?3)",
            rusqlite::params![ZONE, key, path.to_str().unwrap()],
        )
        .unwrap();
    }));
    let open = db
        .preservation_hold_test(
            ZONE,
            baseline_owner(),
            &epoch,
            &attempt,
            &scope,
            &zone_name,
            &owner,
            &database,
            &baseline_scope,
        )
        .await
        .unwrap();
    assert_eq!(open, Ok((1, 3)), "open transaction keeps its snapshot");
    let fresh = db
        .preservation_hold_test(
            ZONE,
            baseline_owner(),
            &epoch,
            &attempt,
            &scope,
            &zone_name,
            &owner,
            &database,
            &baseline_scope,
        )
        .await
        .unwrap();
    assert_eq!(
        fresh,
        Ok((2, 4)),
        "fresh read sees the second committed row"
    );
}

#[tokio::test]
async fn preservation_owner_propagates_a_read_error() {
    let fixture = fixture().await;
    let (_, db, baseline_scope) = run_cycle_at(&fixture, 0, false).await;
    let (epoch, attempt, scope, zone_name, owner, database) = identity(&db).await;
    let conn = rusqlite::Connection::open(&fixture.db_path).unwrap();
    conn.execute_batch("ALTER TABLE unattributed_legacy RENAME TO unattributed_legacy_hidden")
        .unwrap();
    drop(conn);
    let result = db
        .preservation_hold_test(
            ZONE,
            baseline_owner(),
            &epoch,
            &attempt,
            &scope,
            &zone_name,
            &owner,
            &database,
            &baseline_scope,
        )
        .await;
    assert!(result.is_err(), "SQL/read errors must propagate StateError");
}
