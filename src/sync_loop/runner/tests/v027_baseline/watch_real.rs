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
