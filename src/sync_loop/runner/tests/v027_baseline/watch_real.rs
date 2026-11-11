//! The actual runner loop with synthetic auth and a loopback Photos service.
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use wiremock::{
    Mock, Respond, ResponseTemplate,
    matchers::{method, path},
};

const PRIMARY: &str = "PrimarySync";
const PEER: &str = "SharedSync-watch-peer";

fn zone(primary: bool) -> Value {
    json!({"zoneName": if primary { PRIMARY } else { PEER }, "ownerRecordName": if primary { "_defaultOwner" } else { "peer-owner" }})
}

fn records(primary: bool, server: &str) -> Vec<Value> {
    let id = if primary {
        "watch-primary"
    } else {
        "watch-peer"
    };
    let source = zone(primary);
    let mut pair = super::pair(
        id,
        &format!("{id}.JPG"),
        &format!("{server}/{id}"),
        if primary { 81 } else { 82 },
    );
    pair[0]["zoneID"] = source.clone();
    pair[1]["zoneID"] = source.clone();
    pair[1]["fields"]["masterRef"]["value"]["zoneID"] = source;
    pair
}

#[derive(Clone)]
struct WatchProvider {
    server: String,
    stage: Arc<AtomicUsize>,
    rejected: Arc<AtomicBool>,
    primary_change: Arc<AtomicBool>,
    peer_change: Arc<AtomicBool>,
    strict_lookups: Arc<AtomicUsize>,
    peer_deltas: Arc<AtomicUsize>,
    database_checks: Arc<AtomicUsize>,
}

impl Respond for WatchProvider {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let endpoint = request.url.path();
        let stage = self.stage.load(Ordering::SeqCst);
        let primary = !endpoint.contains("/shared/");
        let source = zone(primary);
        let response = if body["query"]["recordType"] == "CheckIndexingState" {
            json!({"records":[{"fields":{"state":{"value":"FINISHED"}}}]})
        } else if endpoint.ends_with("/zones/list") {
            json!({"zones":[{"zoneID":source,"syncToken": if primary && self.rejected.load(Ordering::SeqCst) {"p-fresh"} else if primary {"p-boundary"} else {"s-boundary"},"deleted":false}]})
        } else if endpoint.ends_with("/changes/database") {
            self.database_checks.fetch_add(1, Ordering::SeqCst);
            let changed = if stage == 1 && !self.primary_change.swap(true, Ordering::SeqCst) {
                vec![json!({"zoneID":zone(true),"syncToken":"p-change"})]
            } else if stage == 3 && !self.peer_change.swap(true, Ordering::SeqCst) {
                vec![json!({"zoneID":zone(false),"syncToken":"s-change"})]
            } else {
                Vec::new()
            };
            json!({"syncToken":format!("db-{stage}"),"moreComing":false,"zones":changed})
        } else if endpoint.ends_with("/changes/zone") {
            if primary && stage == 1 && !self.rejected.swap(true, Ordering::SeqCst) {
                json!({"zones":[{"zoneID":source,"serverErrorCode":"BAD_REQUEST"}]})
            } else {
                if !primary {
                    self.peer_deltas.fetch_add(1, Ordering::SeqCst);
                }
                json!({"zones":[{"zoneID":source,"syncToken":if primary {"p-active"} else if stage == 3 {"s-advanced"} else {"s-active"},"records":[],"moreComing":false}]})
            }
        } else if endpoint.ends_with("/internal/records/query/batch") {
            let count = if primary && stage > 0 { 0 } else { 1 };
            let batch: Vec<_> = body["batch"]
                .as_array()
                .unwrap()
                .iter()
                .map(|_| json!({"records":[{"fields":{"itemCount":{"value":count}}}]}))
                .collect();
            json!({"batch":batch})
        } else if endpoint.ends_with("/records/query") {
            let kind = body["query"]["recordType"].as_str().unwrap_or("");
            let rank = body["query"]["filterBy"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|filter| filter["fieldName"] == "startRank")
                .and_then(|filter| filter["fieldValue"]["value"].as_u64())
                .unwrap_or(0);
            let media = kind.contains("CPLAssetAndMaster") && rank == 0 && (!primary || stage == 0);
            json!({"records":if media {records(primary, &self.server)} else {Vec::new()},"syncToken":if primary {"p-active"} else {"s-active"}})
        } else if endpoint.ends_with("/records/lookup") {
            self.strict_lookups.fetch_add(1, Ordering::SeqCst);
            let mut found = records(primary, &self.server);
            if primary && stage > 0 {
                for record in &mut found {
                    record["zoneID"]["ownerRecordName"] = json!("untrusted-owner");
                }
            }
            let requested = body["records"].as_array().unwrap();
            found.retain(|record| {
                requested
                    .iter()
                    .any(|item| item["recordName"] == record["recordName"])
            });
            json!({"records":found})
        } else {
            panic!("unexpected loopback endpoint {endpoint}");
        };
        ResponseTemplate::new(200).set_body_json(response)
    }
}

async fn await_waits(waits: &AtomicUsize, wanted: usize) {
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while waits.load(Ordering::SeqCst) < wanted {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the real watch loop did not reach its next idle wait");
}

#[tokio::test]
async fn real_watch_wakes_with_future_debt_and_selective_peer_progress() {
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::DEBUG)
        .finish();
    let _trace = tracing::subscriber::set_default(subscriber);
    let directory = tempfile::tempdir().unwrap();
    let media_dir = directory.path().join("media");
    std::fs::create_dir(&media_dir).unwrap();
    let server = wiremock::MockServer::start().await;
    for id in ["watch-primary", "watch-peer"] {
        Mock::given(method("GET"))
            .and(path(format!("/{id}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(super::MEDIA))
            .mount(&server)
            .await;
    }
    let provider = WatchProvider {
        server: server.uri(),
        stage: Arc::new(AtomicUsize::new(0)),
        rejected: Arc::new(AtomicBool::new(false)),
        primary_change: Arc::new(AtomicBool::new(false)),
        peer_change: Arc::new(AtomicBool::new(false)),
        strict_lookups: Arc::new(AtomicUsize::new(0)),
        peer_deltas: Arc::new(AtomicUsize::new(0)),
        database_checks: Arc::new(AtomicUsize::new(0)),
    };
    Mock::given(method("POST"))
        .respond_with(provider.clone())
        .mount(&server)
        .await;
    let username = "baseline@example.invalid";
    let namespace = crate::account::namespace(username, "com");
    let db_path = directory.path().join(format!("{namespace}.db"));
    super::seed_existing_upgrade_db(&db_path);
    let report = directory.path().join("report.json");
    let config_path = directory.path().join("watch.toml");
    let quoted = |value: &str| toml::Value::String(value.to_owned()).to_string();
    std::fs::write(&config_path, format!(
        "[auth]\nusername={}\n[download]\ndirectory={}\nfolder_structure=\"none\"\n[filters]\nlibraries=[\"all\"]\nalbums=[\"none\"]\nsmart_folders=[\"none\"]\nunfiled=true\n[watch]\ninterval=60\n[report]\njson={}\n[server]\nport=0\n[ui]\nfriendly=false\nprogress_bar=false\n",
        quoted(username), quoted(media_dir.to_str().unwrap()), quoted(report.to_str().unwrap()),
    )).unwrap();
    let globals = crate::config::GlobalArgs {
        username: Some(username.into()),
        domain: None,
        data_dir: Some(directory.path().display().to_string()),
    };
    let toml = crate::config::load_toml_config(&config_path, true).unwrap();
    let session = crate::auth::session::Session::new(
        directory.path(),
        username,
        "https://example.invalid",
        None,
    )
    .await
    .unwrap();
    let data = serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-baseline"},"webservices":{"ckdatabasews":{"url":server.uri()}}})).unwrap();
    let shutdown = CancellationToken::new();
    let waits = Arc::new(AtomicUsize::new(0));
    let control = crate::sync_loop::runner::WatchLoopTestControl {
        shutdown: shutdown.clone(),
        sleep: std::time::Duration::from_secs(1),
        waits: waits.clone(),
        auth: std::sync::Mutex::new(Some(crate::auth::AuthResult {
            session,
            data,
            requires_2fa: false,
        })),
    };
    let args = crate::sync_loop::SyncArgs {
        is_one_shot: false,
        service_mode: false,
        pw: crate::cli::PasswordArgs::default(),
        sync: crate::cli::SyncArgs::default(),
        toml_config: toml,
        config_explicitly_set: true,
        config_path,
        redact_password: Arc::new(std::sync::Mutex::new(None)),
        personality_mode: crate::personality::Mode::Off,
        friendly_request: Some(false),
        input_mode: crate::InputMode::NoInput,
    };
    let monitor = async {
        await_waits(&waits, 1).await;
        let initial_requests = server.received_requests().await.unwrap();
        for id in ["watch-primary", "watch-peer"] {
            assert!(
                initial_requests
                    .iter()
                    .any(|request| request.url.path() == format!("/{id}")),
                "initial healthy media must really download: {id}"
            );
        }
        let initial_report: Value =
            serde_json::from_slice(&std::fs::read(&report).unwrap()).unwrap();
        assert_eq!(initial_report["status"], "success");
        let originals: Vec<_> = ["watch-primary.JPG", "watch-peer.JPG"]
            .into_iter()
            .map(|name| {
                let path = media_dir.join(name);
                let bytes = std::fs::read(&path).unwrap();
                let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
                (path, bytes, modified)
            })
            .collect();
        let initial_lookups = provider.strict_lookups.load(Ordering::SeqCst);
        let initial_peer_deltas = provider.peer_deltas.load(Ordering::SeqCst);
        provider.stage.store(1, Ordering::SeqCst);
        await_waits(&waits, 2).await;
        assert!(
            provider.rejected.load(Ordering::SeqCst),
            "the real watch cycle must reject the original primary cursor"
        );
        assert_eq!(
            provider.peer_deltas.load(Ordering::SeqCst),
            initial_peer_deltas,
            "unchanged peer must remain selectively deferred"
        );
        let snapshot = || {
            rusqlite::Connection::open(&db_path).unwrap().query_row(
                "SELECT r.id,d.id,d.next_retry_at FROM source_revalidation_rounds r JOIN source_revalidation_debt d ON d.round=r.id WHERE r.library=?1 AND r.mode='catalog_v2' AND r.phase='active'",
                [PRIMARY], |row| Ok((row.get::<_, String>(0)?,row.get::<_, String>(1)?,row.get::<_, i64>(2)?)),
            ).unwrap()
        };
        let debt = snapshot();
        assert!(debt.2 > chrono::Utc::now().timestamp());
        let lookups = provider.strict_lookups.load(Ordering::SeqCst);
        assert!(
            lookups > initial_lookups,
            "the owned future debt must follow a real strict lookup"
        );
        let health_path = directory.path().join("health.json");
        let health_before: Value =
            serde_json::from_slice(&std::fs::read(&health_path).unwrap()).unwrap();
        provider.stage.store(2, Ordering::SeqCst);
        await_waits(&waits, 3).await;
        let quiet: Value = serde_json::from_slice(&std::fs::read(&report).unwrap()).unwrap();
        assert_eq!(quiet["status"], "partial_failure");
        assert_eq!(quiet["stats"]["failed"], 0);
        let quiet_health: Value =
            serde_json::from_slice(&std::fs::read(&health_path).unwrap()).unwrap();
        assert_eq!(
            quiet_health["last_success_at"], health_before["last_success_at"],
            "quiet held debt must not advance success"
        );
        assert_eq!(snapshot(), debt);
        assert_eq!(provider.strict_lookups.load(Ordering::SeqCst), lookups);
        let peer_before = provider.peer_deltas.load(Ordering::SeqCst);
        let peer_token_before: String = rusqlite::Connection::open(&db_path)
            .unwrap()
            .query_row(
                "SELECT value FROM metadata WHERE key=?1",
                [crate::sync_cycle::sync_token_key(PEER)],
                |row| row.get(0),
            )
            .unwrap();
        provider.stage.store(3, Ordering::SeqCst);
        await_waits(&waits, 4).await;
        assert!(provider.peer_change.load(Ordering::SeqCst));
        assert!(
            provider.peer_deltas.load(Ordering::SeqCst) > peer_before,
            "the selected changed peer must make provider progress"
        );
        let peer_token_after: String = rusqlite::Connection::open(&db_path)
            .unwrap()
            .query_row(
                "SELECT value FROM metadata WHERE key=?1",
                [crate::sync_cycle::sync_token_key(PEER)],
                |row| row.get(0),
            )
            .unwrap();
        assert_ne!(
            peer_token_after, peer_token_before,
            "the peer's actual source checkpoint must advance"
        );
        assert_eq!(peer_token_after, "s-advanced");
        assert_eq!(provider.strict_lookups.load(Ordering::SeqCst), lookups);
        assert_eq!(snapshot(), debt);
        let selective: Value = serde_json::from_slice(&std::fs::read(&report).unwrap()).unwrap();
        assert_eq!(selective["status"], "partial_failure");
        for (path, bytes, modified) in originals {
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
            assert_eq!(
                std::fs::metadata(&path).unwrap().modified().unwrap(),
                modified
            );
        }
        shutdown.cancel();
    };
    let loop_future = crate::sync_loop::runner::WATCH_LOOP_TEST_CONTROL
        .scope(control, crate::sync_loop::run_sync(&globals, args));
    let (result, ()) = Box::pin(tokio::time::timeout(
        std::time::Duration::from_secs(90),
        async { tokio::join!(loop_future, monitor) },
    ))
    .await
    .unwrap();
    result.unwrap();
    assert!(shutdown.is_cancelled());
    assert!(provider.database_checks.load(Ordering::SeqCst) >= 4);
}

#[derive(Clone)]
struct ReleasedCatalogProvider(WatchProvider);

#[test]
#[ignore = "stages a synthetic released36 input at an explicitly assigned runtime path"]
fn stage_released36_motion_runtime_input() {
    let root = std::path::PathBuf::from(
        std::env::var_os("KEI_MOTION_STAGE_ROOT").expect("assigned runtime root"),
    );
    let fixture = root.join("fixture/h-final-candidate");
    let username = "tester-fixture-h2@example.invalid";
    let owner = crate::state::db::account::AccountOwner::authenticated(
        username,
        "com",
        &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-h2-provider"}})).unwrap(),
    )
    .unwrap();
    let database = fixture
        .join("data")
        .join(format!("{}.db", crate::account::namespace(username, "com")));
    assert!(!database.exists());
    let stock = crate::state::SqliteStateDb::create_stock_v36_for_test(&database, &owner);
    let media = fixture.join("media/preexisting.jpg");
    std::fs::write(&media, super::MEDIA).unwrap();
    std::fs::write(fixture.join("media/preexisting.jpg.xmp"),br#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><description>preserved custom metadata</description></x:xmpmeta>"#).unwrap();
    let conn = stock.acquire_lock("released36 runtime input").unwrap();
    assert_eq!(crate::state::schema::get_schema_version(&conn).unwrap(), 36);
    conn.execute(
        "DELETE FROM metadata WHERE key=?1",
        [crate::state::db::sync_epoch::TEST_PREEXISTING_EPOCH_KEY],
    )
    .unwrap();
    conn.execute("INSERT INTO assets(library,id,version_size,checksum,filename,created_at,size_bytes,media_type,status,downloaded_at,local_path,last_seen_at) VALUES ('PrimarySync','old-preserved-master','original','old-resource','preexisting.jpg',1700000000,?1,'photo','downloaded',1700000001,?2,1700000001)",rusqlite::params![super::MEDIA.len() as i64,media.to_str().unwrap()]).unwrap();
    let input = fixture.join("input");
    std::fs::create_dir(&input).unwrap();
    conn.execute(
        "VACUUM INTO ?1",
        [input.join("released36.db").to_str().unwrap()],
    )
    .unwrap();
    eprintln!(
        "C01_RELEASED_INPUT schema=36 current_epoch_absent=true current_receipts_absent=true path={}",
        database.display()
    );
}

impl Respond for ReleasedCatalogProvider {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let primary = !request.url.path().contains("/shared/");
        if request.url.path().ends_with("/changes/zone") {
            assert_eq!(body["zones"][0]["zoneID"], zone(primary));
        } else if request.url.path().ends_with("/records/query")
            || request.url.path().ends_with("/records/lookup")
        {
            // PhotosService's provisional indexing probe precedes authenticated
            // private-zone discovery. All selected work must use the full owner.
            if primary
                && body["query"]["recordType"] == "CheckIndexingState"
                && body["zoneID"].get("ownerRecordName").is_none()
            {
                assert_eq!(body["zoneID"], json!({"zoneName": PRIMARY}));
            } else {
                assert_eq!(body["zoneID"], zone(primary));
            }
        }
        if request.url.path().ends_with("/records/lookup") {
            assert_eq!(body["zoneID"], zone(primary));
            let requested = body["records"].as_array().unwrap();
            if requested
                .iter()
                .any(|r| r["recordName"] == "released-master")
            {
                assert!(
                    !primary,
                    "the released Shared family cannot borrow Primary proof"
                );
                assert!(
                    requested
                        .iter()
                        .any(|r| r["recordName"] == "released-child")
                );
                return ResponseTemplate::new(200).set_body_json(json!({"records":[
                    {"recordName":"released-master","recordType":"CPLMaster","zoneID":zone(false),"fields":{
                        "isDeleted":{"value":1},"isExpunged":{"value":1},"dateExpunged":{"value":1700000000000_i64}}},
                    {"recordName":"released-child","recordType":"CPLAsset","zoneID":zone(false),"fields":{
                        "masterRef":{"value":{"recordName":"released-master","zoneID":zone(false)}},
                        "isDeleted":{"value":1},"isExpunged":{"value":1},"dateExpunged":{"value":1700000000000_i64}}}
                ]}));
            }
        }
        if request.url.path().ends_with("/records/query") {
            let kind = body["query"]["recordType"].as_str().unwrap_or("");
            if kind.contains("CPLAssetAndMaster") {
                let rank = body["query"]["filterBy"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|f| f["fieldName"] == "startRank")
                    .and_then(|f| f["fieldValue"]["value"].as_u64())
                    .unwrap_or(0);
                let mut found = if kind.contains("HiddenBy") && rank == 0 {
                    records(primary, &self.0.server)
                } else {
                    Vec::new()
                };
                for record in &mut found {
                    record["fields"]["isHidden"] = json!({"value":1,"type":"INT64"});
                }
                return ResponseTemplate::new(200).set_body_json(
                    json!({"records":found,"syncToken":if primary {"p-active"} else {"s-active"}}),
                );
            }
        }
        if request.url.path().ends_with("/records/lookup") {
            let names = body["records"].as_array().unwrap();
            let mut found = records(primary, &self.0.server);
            found.retain(|r| {
                names
                    .iter()
                    .any(|name| name["recordName"] == r["recordName"])
            });
            for record in &mut found {
                record["fields"]["isHidden"] = json!({"value":1,"type":"INT64"});
            }
            return ResponseTemplate::new(200).set_body_json(json!({"records":found}));
        }
        self.0.respond(request)
    }
}

#[tokio::test]
async fn released36_mixed_selections_reach_real_runner_catalog_tail_and_quiet_reopens() {
    for (libraries, unfiled) in [("shared", true), ("all", true), ("all", false)] {
        let directory = tempfile::tempdir().unwrap();
        let media = directory.path().join("media");
        std::fs::create_dir(&media).unwrap();
        let server = wiremock::MockServer::start().await;
        for id in ["watch-primary", "watch-peer"] {
            Mock::given(method("GET"))
                .and(path(format!("/{id}")))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(super::MEDIA))
                .expect(1)
                .mount(&server)
                .await;
        }
        let provider = WatchProvider {
            server: server.uri(),
            stage: Arc::new(AtomicUsize::new(0)),
            rejected: Arc::new(AtomicBool::new(false)),
            primary_change: Arc::new(AtomicBool::new(false)),
            peer_change: Arc::new(AtomicBool::new(false)),
            strict_lookups: Arc::new(AtomicUsize::new(0)),
            peer_deltas: Arc::new(AtomicUsize::new(0)),
            database_checks: Arc::new(AtomicUsize::new(0)),
        };
        Mock::given(method("POST"))
            .respond_with(ReleasedCatalogProvider(provider))
            .mount(&server)
            .await;
        let username = "baseline@example.invalid";
        let namespace = crate::account::namespace(username, "com");
        let db_path = directory.path().join(format!("{namespace}.db"));
        super::seed_existing_upgrade_db(&db_path);
        let originals = [
            ("original", "released.jpg", super::MEDIA),
            (
                "live_original",
                "released.mov",
                b"original motion".as_slice(),
            ),
        ];
        let mut snapshots = Vec::new();
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            assert_eq!(crate::state::schema::get_schema_version(&conn).unwrap(), 36);
            for (version, name, bytes) in originals {
                let file = media.join(name);
                std::fs::write(&file, bytes).unwrap();
                snapshots.push((
                    file.clone(),
                    bytes.to_vec(),
                    std::fs::metadata(&file).unwrap().modified().unwrap(),
                ));
                conn.execute("INSERT INTO assets(library,id,version_size,checksum,filename,created_at,size_bytes,media_type,status,downloaded_at,local_path,last_seen_at) VALUES (?1,'released-master',?2,'old-provider',?3,1700000000,?4,'photo','downloaded',1700000001,?5,1700000001)",
                    rusqlite::params![PEER,version,name,bytes.len() as i64,file.to_str().unwrap()]).unwrap();
            }
            conn.execute("INSERT INTO asset_master_mappings VALUES (?1,'released-child','released-master',1700000001)",[PEER]).unwrap();
            conn.execute("INSERT INTO legacy_master_state_owners VALUES (?1,'released-master','released-child',1700000001)",[PEER]).unwrap();
        }
        let config_path = directory.path().join("config.toml");
        let report = directory.path().join("report.json");
        let quoted =
            |path: &std::path::Path| toml::Value::String(path.display().to_string()).to_string();
        std::fs::write(&config_path,format!(
            "[auth]\nusername=\"{username}\"\n[download]\ndirectory={}\nfolder_structure=\"none\"\n[filters]\nlibraries=[\"{libraries}\"]\nalbums=[\"none\"]\nsmart_folders=[\"Hidden\"]\nunfiled={unfiled}\n[report]\njson={}\n[server]\nport=0\n[ui]\nfriendly=false\nprogress_bar=false\n", quoted(&media),quoted(&report))).unwrap();
        let globals = crate::config::GlobalArgs {
            username: Some(username.into()),
            domain: None,
            data_dir: Some(directory.path().display().to_string()),
        };
        let mut query_count = 0;
        for reopen in 0..3 {
            let session = crate::auth::session::Session::new(
                directory.path(),
                username,
                "https://example.invalid",
                None,
            )
            .await
            .unwrap();
            let data = serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-baseline"},"webservices":{"ckdatabasews":{"url":server.uri()}}})).unwrap();
            let control = crate::sync_loop::runner::WatchLoopTestControl {
                shutdown: CancellationToken::new(),
                sleep: std::time::Duration::from_secs(1),
                waits: Arc::new(AtomicUsize::new(0)),
                auth: std::sync::Mutex::new(Some(crate::auth::AuthResult {
                    session,
                    data,
                    requires_2fa: false,
                })),
            };
            let args = crate::sync_loop::SyncArgs {
                is_one_shot: true,
                service_mode: false,
                pw: crate::cli::PasswordArgs::default(),
                sync: crate::cli::SyncArgs::default(),
                toml_config: crate::config::load_toml_config(&config_path, true).unwrap(),
                config_explicitly_set: true,
                config_path: config_path.clone(),
                redact_password: Arc::new(std::sync::Mutex::new(None)),
                personality_mode: crate::personality::Mode::Off,
                friendly_request: Some(false),
                input_mode: crate::InputMode::NoInput,
            };
            crate::sync_loop::runner::WATCH_LOOP_TEST_CONTROL
                .scope(
                    control,
                    Box::pin(crate::sync_loop::run_sync(&globals, args)),
                )
                .await
                .unwrap();
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            let deleted:i64=conn.query_row("SELECT count(*) FROM assets WHERE library=?1 AND id='released-master' AND is_deleted=1 AND deleted_at IS NOT NULL",[PEER],|r|r.get(0)).unwrap();
            assert_eq!(
                deleted, 2,
                "whole released family must settle through real runner tail"
            );
            let debt: i64 = conn
                .query_row(
                    "SELECT count(*) FROM source_revalidation_debt WHERE id='released-master'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(debt, 0);
            for (file, bytes, mtime) in &snapshots {
                assert_eq!(&std::fs::read(file).unwrap(), bytes);
                assert_eq!(std::fs::metadata(file).unwrap().modified().unwrap(), *mtime);
            }
            for (library, id, token) in [
                (PRIMARY, "watch-primary", "p-active"),
                (PEER, "watch-peer", "s-active"),
            ] {
                let current_path = media.join("Hidden").join(format!("{id}.JPG"));
                assert_eq!(std::fs::read(&current_path).unwrap(), super::MEDIA);
                if reopen == 0 {
                    snapshots.push((
                        current_path.clone(),
                        super::MEDIA.to_vec(),
                        std::fs::metadata(&current_path)
                            .unwrap()
                            .modified()
                            .unwrap(),
                    ));
                }
                let saved: String = conn
                    .query_row(
                        "SELECT value FROM metadata WHERE key=?1",
                        [crate::sync_cycle::sync_token_key(library)],
                        |r| r.get(0),
                    )
                    .unwrap();
                assert_eq!(saved, token);
                let receipts: i64 = conn
                    .query_row(
                        "SELECT count(*) FROM download_receipts WHERE library=?1 AND child=?2",
                        rusqlite::params![library, format!("asset-{id}")],
                        |r| r.get(0),
                    )
                    .unwrap();
                assert_eq!(receipts, 1);
                let (scope, native, hash): (String, String, String) = conn.query_row(
                    "SELECT scope,native_path,source_checksum FROM download_receipts WHERE library=?1 AND child=?2",
                    rusqlite::params![library,format!("asset-{id}")],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
                ).unwrap();
                let expected_database = if library == PRIMARY {
                    "private"
                } else {
                    "shared"
                };
                let expected_owner = if library == PRIMARY {
                    "_defaultOwner"
                } else {
                    "peer-owner"
                };
                assert_eq!(
                    crate::icloud::photos::PhotoLibrary::provider_scope_projection(&scope),
                    Some((
                        library.into(),
                        expected_owner.into(),
                        expected_database.into()
                    ))
                );
                assert_eq!(
                    serde_json::from_str::<crate::state::db::provider_selection::SelectionPath>(
                        &native
                    )
                    .unwrap()
                    .to_path(),
                    current_path
                );
                assert_eq!(
                    hash,
                    crate::download::file::compute_sha256(&current_path)
                        .await
                        .unwrap()
                );
            }
            let requests = server.received_requests().await.unwrap();
            let inventory = requests
                .iter()
                .filter(|r| {
                    r.url.path().ends_with("/records/query")
                        && String::from_utf8_lossy(&r.body).contains("CPLAssetAndMaster")
                })
                .count();
            if reopen == 0 {
                query_count = inventory;
            }
            assert_eq!(
                inventory, query_count,
                "quiet reopens cannot repeat DATA inventory"
            );
            eprintln!(
                "C16_REAL_RUNNER libraries={libraries} unfiled={unfiled} reopen={reopen} deleted_renditions=2 preserved=true own_source_receipts=2"
            );
        }
        server.verify().await;
    }
}
