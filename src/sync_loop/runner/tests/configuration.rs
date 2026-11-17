use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;

use crate::sync_cycle::{
    DownloadConfigHashOutcome, ENUM_CONFIG_HASH_KEY, EnumConfigHashOutcome,
    PENDING_DOWNLOAD_CONFIG_HASH_KEY, PENDING_ENUM_CONFIG_HASH_KEY, SYNC_TOKEN_PREFIX,
    check_and_persist_enum_config_hash, check_download_config_hash_for_cycle,
    pending_zone_token_key, run_cycle,
};
use crate::sync_loop::test_support::{
    FailingMetadataSetDb, MetadataSetFailure, RunCycleDownloadConfigOptions, album_count_response,
    full_album_page_with_download, make_recording_run_cycle_download_config_builder,
    make_run_cycle_config, make_run_cycle_download_config_builder,
    make_run_cycle_download_config_builder_with_options, make_run_cycle_library_state_with_album,
    make_run_cycle_library_state_with_passes, make_shared_session_for_run_cycle, make_state_db,
};
use crate::{config, download, state};

fn fixture_owner() -> state::db::account::AccountOwner {
    state::db::account::AccountOwner::authenticated(
        "configuration@example.invalid",
        "com",
        &serde_json::from_value(serde_json::json!({"dsInfo":{"dsid":"configuration-fixture"}}))
            .unwrap(),
    )
    .unwrap()
}

async fn open_fixture_db(path: &std::path::Path) -> Arc<state::SqliteStateDb> {
    Arc::new(
        state::SqliteStateDb::open_owned(path, &fixture_owner())
            .await
            .unwrap(),
    )
}

async fn fixture_db() -> (tempfile::TempDir, Arc<state::SqliteStateDb>) {
    let directory = tempfile::tempdir().unwrap();
    let db = open_fixture_db(&directory.path().join("state.db")).await;
    (directory, db)
}

fn fixture_album(
    db: &Arc<state::SqliteStateDb>,
    zone: &str,
    name: &str,
    session: Box<dyn crate::icloud::photos::PhotosSession>,
) -> crate::icloud::photos::PhotoAlbum {
    let mut album = crate::icloud::photos::PhotoAlbum::new(
        crate::icloud::photos::PhotoAlbumConfig {
            params: Arc::new(std::collections::HashMap::new()),
            service_endpoint: Arc::from("https://example.invalid"),
            name: Arc::from(name),
            list_type: Arc::from(if name.is_empty() { "CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted" } else { "CPLContainerRelationLiveByAssetDate" }),
            obj_type: if name.is_empty() { Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted") } else { Arc::from(format!("CPLContainerRelationNotDeletedByAssetDate:{name}")) },
            query_filter: (!name.is_empty()).then(|| Arc::new(serde_json::json!([{"fieldName":"parentId","comparator":"EQUALS","fieldValue":{"type":"STRING","value":name}}]))),
            page_size: 100,
            zone_id: Arc::new(serde_json::json!({"zoneName":zone,"ownerRecordName":"_defaultOwner"})),
            retry_config: crate::retry::RetryConfig::default(),
            container_id: (!name.is_empty()).then(|| Arc::from(name)),
            cross_zone_sources: Vec::new(),
        },
        session,
    );
    album.set_shadow_capture(
        crate::icloud::photos::inbox::ShadowCapture::new(db.clone(), fixture_owner(), "com"),
        Arc::from("private"),
    );
    album
}

fn empty_fixture_album(
    db: &Arc<state::SqliteStateDb>,
    zone: &str,
    name: &str,
    token: &str,
) -> crate::icloud::photos::PhotoAlbum {
    fixture_album(
        db,
        zone,
        name,
        Box::new(ConfigBridgeSession::new(zone, token, token)),
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BridgeWait {
    Replay,
    AssetIdentity,
    PairedAsset,
    AlbumRelation,
    AlbumRelationLookup,
}

#[derive(Clone)]
struct ConfigBridgeSession {
    zone: Arc<str>,
    inventory_token: Arc<str>,
    bridge_token: Arc<str>,
    wait: BridgeWait,
    hydration_pages: usize,
    gate_active: Arc<std::sync::atomic::AtomicBool>,
    bridge_gate: Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>,
    calls: Arc<Mutex<Vec<(&'static str, String)>>>,
}

impl ConfigBridgeSession {
    fn new(zone: &str, inventory_token: &str, bridge_token: &str) -> Self {
        Self {
            zone: Arc::from(zone),
            inventory_token: Arc::from(inventory_token),
            bridge_token: Arc::from(bridge_token),
            wait: BridgeWait::Replay,
            hydration_pages: 0,
            gate_active: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            bridge_gate: None,
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

#[async_trait::async_trait]
impl crate::icloud::photos::PhotosSession for ConfigBridgeSession {
    async fn post(
        &self,
        url: &str,
        body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<serde_json::Value> {
        if url.contains("/internal/records/query/batch") {
            return Ok(album_count_response(0));
        }
        if url.contains("/records/query?") {
            self.calls.lock().unwrap().push(("query", String::new()));
            return Ok(serde_json::json!({
                "records": [],
                "syncToken": self.inventory_token.as_ref()
            }));
        }
        if url.contains("/changes/zone?") {
            let request: serde_json::Value = serde_json::from_str(&body)?;
            self.calls.lock().unwrap().push((
                "changes",
                request["zones"][0]["syncToken"]
                    .as_str()
                    .unwrap_or("")
                    .to_owned(),
            ));
            let active = self.gate_active.load(std::sync::atomic::Ordering::SeqCst);
            let cursor = request["zones"][0]["syncToken"].as_str();
            let scan =
                cursor.is_none() || cursor.is_some_and(|s| s.starts_with("synthetic-hydration-"));
            let page = cursor
                .and_then(|s| s.strip_prefix("synthetic-hydration-"))
                .map(|s| s.parse::<usize>().unwrap())
                .unwrap_or(0);
            if active && scan && page < self.hydration_pages {
                return Ok(
                    serde_json::json!({"zones":[{"zoneID":{"zoneName":self.zone.as_ref(),"ownerRecordName":"_defaultOwner"},
                    "syncToken":format!("synthetic-hydration-{}",page+1), "moreComing":true,
                    "records":(0..200).map(|i|serde_json::json!({"recordName":format!("synthetic-master-{page}-{i}"),"recordType":"CPLMaster","fields":{}})).collect::<Vec<_>>()}]}),
                );
            }
            if active
                && (self.wait == BridgeWait::Replay || scan)
                && let Some((requested, release)) = &self.bridge_gate
            {
                requested.notify_one();
                release.notified().await;
            }
            let records = if active && self.bridge_gate.is_some() && !scan {
                match self.wait {
                    BridgeWait::Replay => Vec::new(),
                    BridgeWait::AssetIdentity | BridgeWait::PairedAsset => {
                        let mut asset = serde_json::json!({
                            "recordName": "WAIT_CHILD", "recordType": "CPLAsset",
                            "fields": {"masterRef": {"value": {"recordName": "WAIT_MASTER", "zoneID":{"zoneName":self.zone.as_ref(),"ownerRecordName":"_defaultOwner"}}}}
                        });
                        if self.wait == BridgeWait::AssetIdentity {
                            asset["fields"].as_object_mut().unwrap().remove("masterRef");
                        }
                        vec![asset]
                    }
                    BridgeWait::AlbumRelation | BridgeWait::AlbumRelationLookup => {
                        vec![serde_json::json!({
                            "recordName": "WAIT_RELATION", "recordType": "CPLContainerRelation",
                            "fields": {"containerId": {"value": "synthetic-album"},
                                       "itemId": {"value": "WAIT_CHILD"}}
                        })]
                    }
                }
            } else {
                Vec::new()
            };
            return Ok(serde_json::json!({
                "zones": [{
                    "zoneID": {"zoneName": self.zone.as_ref(), "ownerRecordName": "_defaultOwner"},
                    "syncToken": self.bridge_token.as_ref(),
                    "moreComing": false,
                    "records": records
                }]
            }));
        }
        if url.contains("/records/lookup?") {
            self.calls.lock().unwrap().push(("lookup", String::new()));
            if self.wait != BridgeWait::AlbumRelation
                && self.gate_active.load(std::sync::atomic::Ordering::SeqCst)
                && let Some((requested, release)) = &self.bridge_gate
            {
                requested.notify_one();
                release.notified().await;
            }
        }
        Ok(serde_json::json!({"records": []}))
    }

    fn clone_box(&self) -> Box<dyn crate::icloud::photos::PhotosSession> {
        Box::new(self.clone())
    }
}

#[tokio::test]
async fn issue765_cancel_pending_bridge_preserves_checkpoint_then_replays() {
    Box::pin(cancelled_bridge_fixture(BridgeWait::Replay, false)).await;
}

#[tokio::test]
async fn issue958_cancel_identity_wait_preserves_checkpoint_then_replays() {
    Box::pin(cancelled_bridge_fixture(BridgeWait::AssetIdentity, false)).await;
}

#[tokio::test]
async fn issue958_cancel_paired_wait_preserves_checkpoint_then_replays() {
    Box::pin(cancelled_bridge_fixture(BridgeWait::PairedAsset, false)).await;
}

#[tokio::test]
async fn issue958_cancel_relation_scan_preserves_checkpoint_then_replays() {
    Box::pin(cancelled_bridge_fixture(BridgeWait::AlbumRelation, false)).await;
}

#[tokio::test]
async fn issue958_cancel_streaming_identity_wait_preserves_checkpoint_then_replays() {
    for wait in [BridgeWait::AssetIdentity, BridgeWait::PairedAsset] {
        Box::pin(cancelled_bridge_fixture(wait, true)).await;
    }
}

#[tokio::test]
async fn issue958_cancel_after_paged_relation_scan_preserves_then_replays() {
    Box::pin(cancelled_bridge_fixture_with_pages(
        BridgeWait::AlbumRelation,
        false,
        2,
    ))
    .await;
}

#[tokio::test]
async fn issue958_cancel_relation_lookup_preserves_checkpoint_then_replays() {
    Box::pin(cancelled_bridge_fixture(
        BridgeWait::AlbumRelationLookup,
        false,
    ))
    .await;
}

async fn cancelled_bridge_fixture(wait: BridgeWait, streaming: bool) {
    Box::pin(cancelled_bridge_fixture_with_pages(wait, streaming, 0)).await;
}

async fn cancelled_bridge_fixture_with_pages(
    wait: BridgeWait,
    streaming: bool,
    hydration_pages: usize,
) {
    use std::time::Duration;
    use tracing_subscriber::layer::SubscriberExt;

    const DRAIN_BOUND: Duration = Duration::from_secs(1);
    const CLEANUP_BOUND: Duration = Duration::from_secs(5);
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("issue765.db");
    let media = directory.path().join("media");
    std::fs::create_dir(&media).unwrap();
    let sentinel = media.join("existing.jpg");
    std::fs::write(&sentinel, b"existing synthetic media").unwrap();
    let db = open_fixture_db(&database).await;
    let marker = state::unresolved_identity_key("PrimarySync");
    db.set_metadata(ENUM_CONFIG_HASH_KEY, "old-enum-hash")
        .await
        .unwrap();
    db.set_metadata("sync_token:PrimarySync", "prior-token")
        .await
        .unwrap();
    db.set_metadata(&marker, "1").await.unwrap();
    let requested = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let mut session = ConfigBridgeSession::new("PrimarySync", "inventory-token", "bridge-token");
    session.bridge_gate = Some((requested.clone(), release.clone()));
    session.wait = wait;
    session.hydration_pages = hydration_pages;
    let gate_active = session.gate_active.clone();
    let calls = session.calls.clone();
    let album = fixture_album(
        &db,
        "PrimarySync",
        if streaming { "" } else { "synthetic-album" },
        Box::new(session.clone()),
    );
    let library = make_run_cycle_library_state_with_passes(
        "PrimarySync",
        "sync_token:PrimarySync",
        vec![crate::commands::AlbumPass {
            kind: if streaming {
                crate::commands::PassKind::Unfiled
            } else {
                crate::commands::PassKind::Album
            },
            album,
            exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
        }],
    );
    let states = [&library];
    let config = make_run_cycle_config();
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
    let builder = make_run_cycle_download_config_builder(&media, db.clone());
    let (layer, diagnostics) = crate::support::history::test_layer();
    let _default = tracing::subscriber::set_default(tracing_subscriber::registry().with(layer));
    // Keep cold callsite registration independent of a worker's missing TLS
    // subscriber. Production installs a global subscriber instead.
    let _worker_callsite_registration = tracing::Dispatch::new(tracing_subscriber::registry());
    let cancel = CancellationToken::new();
    let mut cycle = Box::pin(run_cycle(
        &states,
        &config,
        Some(db.as_ref()),
        &builder,
        download::DownloadControls::download_hidden(),
        &shared_session,
        &cancel,
    ));
    tokio::select! {
        () = requested.notified() => {}
        result = cycle.as_mut() => panic!("bridge gate was not reached: {result:?}"),
        () = tokio::time::sleep(CLEANUP_BOUND) => {
            release.notify_one();
            panic!("bridge gate was not reached within the fixture bound");
        }
    }
    cancel.cancel();
    let result = tokio::time::timeout(DRAIN_BOUND, cycle.as_mut()).await;
    let drained = result.is_ok();
    // Release even on the red path: no blocked provider task may escape this test.
    release.notify_one();
    let result = match result {
        Ok(result) => result,
        Err(_) => tokio::time::timeout(CLEANUP_BOUND, cycle.as_mut())
            .await
            .expect("released synthetic bridge must finish cleanup"),
    }
    .expect("cancelled cycle returns its checkpoint hold");
    assert!(result.stats.interrupted);
    assert!(!result.db_sync_token_advance_safe);
    assert!(!result.can_advance_database_checkpoint());
    let observations = diagnostics();
    // The streaming producer runs on a spawned task without this test's TLS
    // subscriber; its durable cycle outcome is still captured below.
    if wait != BridgeWait::Replay && !streaming {
        let phase = match wait {
            BridgeWait::AssetIdentity => "asset_identity",
            BridgeWait::PairedAsset => "paired_asset",
            BridgeWait::AlbumRelation | BridgeWait::AlbumRelationLookup => "album_relation",
            BridgeWait::Replay => panic!("replay waits do not enter hydration"),
        };
        assert!(
            observations
                .iter()
                .any(|event| event["kind"] == "support_hydration_wait_v1"
                    && event["fields"]["phase"] == phase
                    && event["fields"]["outcome"] == "cancelled")
        );
        let encoded = serde_json::to_string(&observations).unwrap();
        for private in [
            "WAIT_CHILD",
            "WAIT_MASTER",
            "WAIT_RELATION",
            "synthetic-album",
            "example.invalid",
        ] {
            assert!(!encoded.contains(private));
        }
    }
    let bridge = observations
        .into_iter()
        .find(|event| {
            event["kind"] == "support_pass_completion_v1"
                && event["fields"]["component"] == "delta_bridge"
        })
        .expect("production cycle records its cancelled bridge decision");
    assert_eq!(bridge["fields"]["interrupted"], true);
    assert_eq!(
        bridge["fields"]["completed_delta_replay"],
        wait != BridgeWait::Replay
    );
    drop(cycle);
    drop(builder);
    drop(library);
    drop(db);
    let db = open_fixture_db(&database).await;
    for (key, expected) in [
        ("sync_token:PrimarySync", "prior-token"),
        (ENUM_CONFIG_HASH_KEY, "old-enum-hash"),
        (marker.as_str(), "1"),
    ] {
        assert_eq!(
            db.get_metadata(key).await.unwrap().as_deref(),
            Some(expected)
        );
    }
    let expected_hash = download::compute_config_hash(&config);
    assert_eq!(
        db.get_metadata(PENDING_ENUM_CONFIG_HASH_KEY)
            .await
            .unwrap()
            .as_deref(),
        Some(expected_hash.as_str())
    );
    assert_eq!(
        db.get_metadata(&pending_zone_token_key(&expected_hash, "PrimarySync"))
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        std::fs::read(&sentinel).unwrap(),
        b"existing synthetic media"
    );
    let initial_calls = calls.lock().unwrap().clone();
    assert_eq!(
        initial_calls
            .iter()
            .filter(|(kind, cursor)| *kind == "changes"
                && !cursor.is_empty()
                && !cursor.starts_with("synthetic-hydration-"))
            .map(|(_, cursor)| cursor.as_str())
            .collect::<Vec<_>>(),
        ["prior-token"]
    );
    let inventory_queries = initial_calls
        .iter()
        .filter(|(kind, _)| *kind == "query")
        .count();
    assert!(inventory_queries > 0);
    if hydration_pages > 0 {
        assert_eq!(
            initial_calls
                .iter()
                .filter(|(kind, cursor)| *kind == "changes"
                    && cursor.starts_with("synthetic-hydration-"))
                .count(),
            hydration_pages
        );
    }
    gate_active.store(false, std::sync::atomic::Ordering::SeqCst);
    let initial_lookups = initial_calls
        .iter()
        .filter(|(kind, _)| *kind == "lookup")
        .count();
    assert_eq!(
        initial_lookups,
        usize::from(matches!(
            wait,
            BridgeWait::AssetIdentity
                | BridgeWait::PairedAsset
                | BridgeWait::AlbumRelation
                | BridgeWait::AlbumRelationLookup
        ))
    );
    drop(db);
    let mut replay_queries = 0;
    for replay in 0..2 {
        let db = open_fixture_db(&database).await;
        let library = make_run_cycle_library_state_with_passes(
            "PrimarySync",
            "sync_token:PrimarySync",
            vec![crate::commands::AlbumPass {
                kind: if streaming {
                    crate::commands::PassKind::Unfiled
                } else {
                    crate::commands::PassKind::Album
                },
                album: fixture_album(
                    &db,
                    "PrimarySync",
                    if streaming { "" } else { "synthetic-album" },
                    Box::new(session.clone()),
                ),
                exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
            }],
        );
        let builder = make_run_cycle_download_config_builder(&media, db.clone());
        // Reuse the exact Album/container route, releasing its synthetic gate.
        release.notify_one();
        let result = tokio::time::timeout(
            CLEANUP_BOUND,
            run_cycle(
                &[&library],
                &config,
                Some(db.as_ref()),
                &builder,
                download::DownloadControls::download_hidden(),
                &shared_session,
                &CancellationToken::new(),
            ),
        )
        .await
        .expect("valid synthetic replay is bounded")
        .unwrap();
        assert!(result.can_advance_database_checkpoint());
        assert!(!result.stats.interrupted);
        assert_eq!(result.stats.downloaded, 0);
        assert_eq!(
            db.get_metadata(ENUM_CONFIG_HASH_KEY)
                .await
                .unwrap()
                .as_deref(),
            Some(expected_hash.as_str())
        );
        assert_eq!(
            db.get_metadata(PENDING_ENUM_CONFIG_HASH_KEY).await.unwrap(),
            None
        );
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("bridge-token")
        );
        assert_eq!(db.get_metadata(&marker).await.unwrap(), None);
        let observed = calls.lock().unwrap().clone();
        assert_eq!(
            observed
                .iter()
                .filter(|(kind, _)| *kind == "lookup")
                .count(),
            initial_lookups
        );
        let queries = observed.iter().filter(|(kind, _)| *kind == "query").count();
        if replay == 0 {
            assert!(
                queries > inventory_queries,
                "recovery must reconcile inventory"
            );
            replay_queries = queries;
        } else {
            assert_eq!(
                queries, replay_queries,
                "quiet follow-up must not repeat inventory"
            );
        }
        let cursors: Vec<_> = observed
            .iter()
            .filter(|(kind, cursor)| {
                *kind == "changes"
                    && !cursor.is_empty()
                    && !cursor.starts_with("synthetic-hydration-")
            })
            .map(|(_, cursor)| cursor.as_str())
            .collect();
        let expected = if replay == 0 {
            vec!["prior-token", "prior-token"]
        } else {
            vec!["prior-token", "prior-token", "bridge-token"]
        };
        assert_eq!(cursors, expected);
        assert_eq!(
            std::fs::read(&sentinel).unwrap(),
            b"existing synthetic media"
        );
    }
    assert!(
        drained,
        "cancellation waited for the pending bridge request instead of draining"
    );
}

#[tokio::test]
async fn run_cycle_config_hash_stage_failure_forces_full_without_replacing_checkpoint() {
    let config = make_run_cycle_config();
    let current_hash = download::compute_config_hash(&config);
    assert_ne!(current_hash, "old-hash");

    let (_database_dir, inner) = fixture_db().await;
    inner
        .set_metadata(ENUM_CONFIG_HASH_KEY, "old-hash")
        .await
        .expect("seed enum hash");
    inner
        .set_metadata("sync_token:PrimarySync", "zone-tok-prev")
        .await
        .expect("seed zone token");
    inner.acquire_lock("pending configuration stage fault").unwrap().execute_batch(
        "CREATE TRIGGER fail_pending_enum BEFORE INSERT ON metadata WHEN NEW.key='pending_enum_config_hash' BEGIN SELECT RAISE(ABORT,'simulated pending hash write failure'); END;"
    ).unwrap();
    let db = inner.clone();
    let download_dir = tempfile::tempdir().expect("download tempdir");
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

    let lib_state = make_run_cycle_library_state_with_album(
        "PrimarySync",
        "sync_token:PrimarySync",
        empty_fixture_album(&db, "PrimarySync", "", "zone-tok-new"),
    );
    let states = vec![&lib_state];
    let observed_modes = Arc::new(std::sync::Mutex::new(Vec::<download::SyncMode>::new()));
    let build_download_config = make_recording_run_cycle_download_config_builder(
        download_dir.path(),
        db.clone(),
        Arc::clone(&observed_modes),
    );

    let result = run_cycle(
        &states,
        &config,
        Some(db.as_ref()),
        &build_download_config,
        download::DownloadControls::download_hidden(),
        &shared_session,
        &CancellationToken::new(),
    )
    .await
    .expect("run cycle");

    assert_eq!(result.failed_count, 0);
    assert_eq!(
        result.stats.full_enumeration_reason,
        Some(download::FullEnumerationReason::EnumConfigHashDrift)
    );
    let observed_modes = observed_modes.lock().expect("recorded modes lock").clone();
    assert!(
        !observed_modes.is_empty(),
        "staging failure must still reach source execution"
    );
    assert!(
        observed_modes
            .iter()
            .all(|mode| matches!(mode, download::SyncMode::Full)),
        "config drift must not trust a surviving old incremental token in this cycle: {observed_modes:?}"
    );
    assert!(
        !result.db_sync_token_advance_safe,
        "database precheck token must not advance until config-hash invalidation can persist safely"
    );
    assert_eq!(
        inner
            .get_metadata(ENUM_CONFIG_HASH_KEY)
            .await
            .expect("read enum hash")
            .as_deref(),
        Some("old-hash"),
        "new hash must not become active when reconciliation could not be staged"
    );
    assert_eq!(
        inner
            .get_metadata("sync_token:PrimarySync")
            .await
            .expect("read zone token")
            .as_deref(),
        Some("zone-tok-prev"),
        "failed reconciliation staging must preserve the last safe zone token"
    );
}

#[tokio::test]
async fn run_cycle_enum_config_drift_atomically_promotes_bridged_checkpoint() {
    let config = make_run_cycle_config();
    let (_database_dir, db) = fixture_db().await;
    db.set_metadata(ENUM_CONFIG_HASH_KEY, "old-enum-hash")
        .await
        .expect("seed active enum hash");
    db.set_metadata("sync_token:PrimarySync", "prior-token")
        .await
        .expect("seed prior token");
    let download_dir = tempfile::tempdir().expect("download tempdir");
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
    let album = fixture_album(
        &db,
        "PrimarySync",
        "",
        Box::new(ConfigBridgeSession::new(
            "PrimarySync",
            "inventory-token",
            "bridge-token",
        )),
    );
    let lib_state =
        make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
    let build_download_config =
        make_run_cycle_download_config_builder(download_dir.path(), db.clone());

    let result = run_cycle(
        &[&lib_state],
        &config,
        Some(db.as_ref()),
        &build_download_config,
        download::DownloadControls::download_hidden(),
        &shared_session,
        &CancellationToken::new(),
    )
    .await
    .expect("inventory plus delta bridge should complete");

    assert!(result.db_sync_token_advance_safe);
    assert_eq!(
        db.get_metadata("sync_token:PrimarySync")
            .await
            .unwrap()
            .as_deref(),
        Some("bridge-token")
    );
    let expected_hash = download::compute_config_hash(&config);
    assert_eq!(
        db.get_metadata(ENUM_CONFIG_HASH_KEY)
            .await
            .unwrap()
            .as_deref(),
        Some(expected_hash.as_str())
    );
    assert_eq!(
        db.get_metadata(PENDING_ENUM_CONFIG_HASH_KEY).await.unwrap(),
        None
    );
}

#[tokio::test]
async fn support_history_retains_token_repair_and_bridge_phases_at_info_across_restarts() {
    use tracing_subscriber::Layer;
    use tracing_subscriber::layer::SubscriberExt;

    let config = make_run_cycle_config();
    let (_database_dir, db) = fixture_db().await;
    db.set_metadata(ENUM_CONFIG_HASH_KEY, "old-enum-hash")
        .await
        .unwrap();
    db.set_metadata("sync_token:PrimarySync", "PRIVATE_PRIOR_TOKEN")
        .await
        .unwrap();
    let download_dir = tempfile::tempdir().unwrap();
    let history_dir = tempfile::tempdir().unwrap();
    let history_path = history_dir.path().join("history.json");
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
    let build_download_config =
        make_run_cycle_download_config_builder(download_dir.path(), db.clone());

    // Actual cycle orchestration: failed inventory token repair, then complete
    // inventory with a failed bridge, then successful bridge after reopening.
    for (inventory, bridge, advance) in [
        ("", "PRIVATE_UNUSED_TOKEN", false),
        ("PRIVATE_INVENTORY_TOKEN", "", false),
        ("PRIVATE_INVENTORY_TOKEN", "PRIVATE_BRIDGE_TOKEN", true),
    ] {
        let provider = ConfigBridgeSession::new("PrimarySync", inventory, bridge);
        let album = fixture_album(&db, "PrimarySync", "", Box::new(provider.clone()));
        let lib_state =
            make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
        let (recorder, guard) =
            crate::support::history::start(history_path.clone(), serde_json::json!({})).unwrap();
        let subscriber = tracing_subscriber::registry().with(recorder).with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::sink)
                .with_filter(tracing_subscriber::filter::LevelFilter::INFO),
        );
        // This current-thread test must cover spawned provider/pipeline tasks,
        // not only polls of run_cycle (see the full-sync history test).
        let _default = tracing::subscriber::set_default(subscriber);
        // Keep cold callsite registration independent of a worker's missing TLS
        // subscriber. Production installs a global subscriber instead.
        let _worker_callsite_registration = tracing::Dispatch::new(tracing_subscriber::registry());
        let result = run_cycle(
            &[&lib_state],
            &config,
            Some(db.as_ref()),
            &build_download_config,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await;
        if bridge.is_empty() {
            let error = result.expect_err("blank bridge successor must remain a provider refusal");
            assert!(format!("{error:#}").contains("Missing changes/zone successor"));
            let calls = provider.calls.lock().unwrap();
            let query = calls.iter().position(|(kind, _)| *kind == "query").unwrap();
            let bridge = calls
                .iter()
                .position(|(kind, cursor)| *kind == "changes" && cursor == "PRIVATE_PRIOR_TOKEN")
                .unwrap();
            assert!(
                query < bridge,
                "the real bridge must fail after current query coverage: {calls:?}"
            );
        } else {
            assert_eq!(result.unwrap().db_sync_token_advance_safe, advance);
        }
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some(if advance {
                "PRIVATE_BRIDGE_TOKEN"
            } else {
                "PRIVATE_PRIOR_TOKEN"
            })
        );
        assert_eq!(
            db.get_metadata(ENUM_CONFIG_HASH_KEY)
                .await
                .unwrap()
                .as_deref(),
            Some(if advance {
                download::compute_config_hash(&config)
            } else {
                "old-enum-hash".to_owned()
            })
            .as_deref()
        );
        assert_eq!(
            db.get_metadata(PENDING_ENUM_CONFIG_HASH_KEY)
                .await
                .unwrap()
                .is_some(),
            !advance
        );
        guard.finish().await;
    }
    let history: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&history_path).unwrap()).unwrap();
    let cycles = history["cycles"].as_array().unwrap();
    assert_eq!(cycles.len(), 3);
    let component = |cycle: usize, name: &str| -> &serde_json::Value {
        &cycles[cycle]["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["kind"] == "support_pass_completion_v1" && d["fields"]["component"] == name)
            .unwrap()["fields"]
    };
    assert_eq!(component(0, "initial")["same_cycle_recovery_attempts"], 1);
    assert_eq!(component(0, "initial")["same_cycle_recovery_successes"], 0);
    assert_eq!(component(0, "initial")["sync_token_receivers_blank"], 1);
    assert_eq!(component(0, "initial")["sync_token_blocked"], true);
    assert_eq!(component(1, "initial")["token_present"], true);
    assert_eq!(
        component(1, "initial")["sync_token_blocked"],
        false,
        "{}",
        cycles[1]
    );
    // Strict source parsing returns an error before bridge/combined completion
    // diagnostics. Initial query coverage is still visible, never reported as
    // a completed source replay. The durable hold is checked above.
    assert!(
        !cycles[1]["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|diagnostic| {
                diagnostic["kind"] == "support_pass_completion_v1"
                    && (diagnostic["fields"]["component"] == "combined"
                        || diagnostic["fields"]["completed_delta_replay"] == true)
            })
    );
    assert_eq!(component(2, "delta_bridge")["token_present"], true);
    assert_eq!(component(2, "combined")["sync_token_blocked"], false);
    let encoded = history.to_string();
    for private in [
        "PRIVATE",
        "PrimarySync",
        "sync_token:PrimarySync",
        download_dir.path().to_str().unwrap(),
        history_dir.path().to_str().unwrap(),
    ] {
        assert!(!encoded.contains(private), "private fixture value retained");
    }
}

#[tokio::test]
async fn run_cycle_multi_zone_reconciliation_preserves_all_active_tokens_on_partial_failure() {
    let config = make_run_cycle_config();
    let (_database_dir, db) = fixture_db().await;
    db.set_metadata(ENUM_CONFIG_HASH_KEY, "old-enum-hash")
        .await
        .unwrap();
    db.set_metadata("sync_token:PrimarySync", "primary-prior")
        .await
        .unwrap();
    db.set_metadata("sync_token:SharedSync-TEST", "shared-prior")
        .await
        .unwrap();
    let download_dir = tempfile::tempdir().expect("download tempdir");
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
    let primary = make_run_cycle_library_state_with_album(
        "PrimarySync",
        "sync_token:PrimarySync",
        fixture_album(
            &db,
            "PrimarySync",
            "",
            Box::new(ConfigBridgeSession::new(
                "PrimarySync",
                "primary-inventory",
                "primary-bridge",
            )),
        ),
    );
    let shared = make_run_cycle_library_state_with_album(
        "SharedSync-TEST",
        "sync_token:SharedSync-TEST",
        fixture_album(
            &db,
            "SharedSync-TEST",
            "",
            Box::new(ConfigBridgeSession::new("SharedSync-TEST", "", "unused")),
        ),
    );
    let build_download_config =
        make_run_cycle_download_config_builder(download_dir.path(), db.clone());

    let result = run_cycle(
        &[&primary, &shared],
        &config,
        Some(db.as_ref()),
        &build_download_config,
        download::DownloadControls::download_hidden(),
        &shared_session,
        &CancellationToken::new(),
    )
    .await
    .expect("partial multi-zone reconciliation should preserve active state");

    assert!(!result.db_sync_token_advance_safe);
    assert_eq!(
        db.get_metadata("sync_token:PrimarySync")
            .await
            .unwrap()
            .as_deref(),
        Some("primary-prior")
    );
    assert_eq!(
        db.get_metadata("sync_token:SharedSync-TEST")
            .await
            .unwrap()
            .as_deref(),
        Some("shared-prior")
    );
    assert_eq!(
        db.get_metadata(ENUM_CONFIG_HASH_KEY)
            .await
            .unwrap()
            .as_deref(),
        Some("old-enum-hash")
    );
    let expected_hash = download::compute_config_hash(&config);
    assert_eq!(
        db.get_metadata(&pending_zone_token_key(&expected_hash, "PrimarySync",))
            .await
            .unwrap()
            .as_deref(),
        Some("primary-bridge"),
        "the completed zone must retain its reconciliation checkpoint"
    );
    assert_eq!(
        db.get_metadata(&pending_zone_token_key(&expected_hash, "SharedSync-TEST",))
            .await
            .unwrap(),
        None
    );

    let primary_resume = make_run_cycle_library_state_with_album(
        "PrimarySync",
        "sync_token:PrimarySync",
        fixture_album(
            &db,
            "PrimarySync",
            "",
            Box::new(ConfigBridgeSession::new(
                "PrimarySync",
                "unused-inventory",
                "primary-after-resume",
            )),
        ),
    );
    let shared_retry = make_run_cycle_library_state_with_album(
        "SharedSync-TEST",
        "sync_token:SharedSync-TEST",
        fixture_album(
            &db,
            "SharedSync-TEST",
            "",
            Box::new(ConfigBridgeSession::new(
                "SharedSync-TEST",
                "shared-inventory",
                "shared-bridge",
            )),
        ),
    );

    let resumed = run_cycle(
        &[&primary_resume, &shared_retry],
        &config,
        Some(db.as_ref()),
        &build_download_config,
        download::DownloadControls::download_hidden(),
        &shared_session,
        &CancellationToken::new(),
    )
    .await
    .expect("unfinished zone reconciliation should resume");

    assert!(resumed.db_sync_token_advance_safe);
    assert_eq!(
        db.get_metadata("sync_token:PrimarySync")
            .await
            .unwrap()
            .as_deref(),
        Some("primary-after-resume")
    );
    assert_eq!(
        db.get_metadata("sync_token:SharedSync-TEST")
            .await
            .unwrap()
            .as_deref(),
        Some("shared-bridge")
    );
    assert_eq!(
        db.get_metadata(ENUM_CONFIG_HASH_KEY)
            .await
            .unwrap()
            .as_deref(),
        Some(expected_hash.as_str())
    );
    assert_eq!(
        db.get_metadata(&pending_zone_token_key(&expected_hash, "PrimarySync",))
            .await
            .unwrap(),
        None
    );
}

#[cfg(unix)]
#[tokio::test]
async fn run_cycle_reconciliation_rejection_preserves_source_dispatch_and_state() {
    Box::pin(external_reconciliation_lifecycle(
        ExternalReconciliationEntry::Symlink,
    ))
    .await;
}

#[cfg(unix)]
#[tokio::test]
async fn run_cycle_external_hardlink_reconciliation_restart() {
    Box::pin(external_reconciliation_lifecycle(
        ExternalReconciliationEntry::Hardlink,
    ))
    .await;
}

#[cfg(unix)]
#[tokio::test]
async fn run_cycle_external_same_size_replacement_restart() {
    Box::pin(external_reconciliation_lifecycle(
        ExternalReconciliationEntry::SameSizeReplacement,
    ))
    .await;
}

#[cfg(unix)]
#[tokio::test]
async fn run_cycle_external_moved_directory_restart() {
    Box::pin(external_reconciliation_lifecycle(
        ExternalReconciliationEntry::MovedDirectory,
    ))
    .await;
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy)]
enum ExternalReconciliationEntry {
    Symlink,
    Hardlink,
    SameSizeReplacement,
    MovedDirectory,
}

#[cfg(unix)]
async fn external_reconciliation_lifecycle(entry: ExternalReconciliationEntry) {
    use crate::state::{DownloadContextStateStore, ReconciliationStateStore};
    use sha2::{Digest, Sha256};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, UNIX_EPOCH};
    #[derive(Clone, Debug)]
    struct ReconciliationSession {
        records: serde_json::Value,
        source_calls: Arc<AtomicUsize>,
        lookup_calls: Arc<AtomicUsize>,
    }
    #[async_trait::async_trait]
    impl crate::icloud::photos::PhotosSession for ReconciliationSession {
        async fn post(
            &self,
            url: &str,
            body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<serde_json::Value> {
            let request: serde_json::Value = serde_json::from_str(&body)?;
            if url.contains("/records/lookup?") {
                self.lookup_calls.fetch_add(1, Ordering::SeqCst);
                let records: Vec<_> = request["records"].as_array().unwrap().iter().map(|wanted| {
                    self.records["records"].as_array().unwrap().iter()
                        .find(|record| record["recordName"] == wanted["recordName"])
                        .cloned().unwrap_or_else(|| serde_json::json!({"recordName":wanted["recordName"],"serverErrorCode":"UNKNOWN_ITEM"}))
                }).collect();
                return Ok(serde_json::json!({"records":records}));
            }
            self.source_calls.fetch_add(1, Ordering::SeqCst);
            if url.contains("/changes/zone?") {
                let records = if request["zones"][0]["syncToken"].is_null() {
                    self.records["records"].clone()
                } else {
                    serde_json::json!([])
                };
                return Ok(
                    serde_json::json!({"zones": [{"zoneID": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"}, "syncToken": "zone-after", "moreComing": false, "records": records}]}),
                );
            }
            if url.contains("/internal/records/query/batch") {
                return Ok(album_count_response(1));
            }
            assert!(url.contains("/records/query?"));
            let offset = request["query"]["filterBy"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|filter| filter["fieldName"] == "startRank")
                .and_then(|filter| filter["fieldValue"]["value"].as_u64())
                .unwrap_or(0);
            Ok(
                serde_json::json!({"records": if offset == 0 { self.records["records"].clone() } else { serde_json::json!([]) }, "syncToken": "zone-after"}),
            )
        }
        fn clone_box(&self) -> Box<dyn crate::icloud::photos::PhotosSession> {
            Box::new(self.clone())
        }
    }
    let config = make_run_cycle_config();
    let old_dir = tempfile::tempdir().unwrap();
    let new_dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let db_path = old_dir.path().join("state.db");
    let db = open_fixture_db(&db_path).await;
    let old_path = old_dir.path().join("photo.jpg");
    let external = outside.path().join("photo.jpg");
    // This is now a real selected transfer, so its advertised JPEG must pass
    // the same media-signature validation as a provider download.
    let mut original_bytes = vec![0u8; 1024];
    original_bytes[..8].copy_from_slice(&[0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10, 0x4a, 0x46]);
    original_bytes[1022..].copy_from_slice(&[0xff, 0xd9]);
    std::fs::write(&old_path, &original_bytes).unwrap();
    std::fs::write(&external, vec![0u8; 1024]).unwrap();
    let old_time = UNIX_EPOCH + Duration::from_secs(12345);
    std::fs::File::options()
        .write(true)
        .open(&old_path)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(old_time))
        .unwrap();
    let unrelated = new_dir.path().join("unrelated.xmp");
    std::fs::write(&unrelated, b"external user metadata").unwrap();
    let checksum = download::file::compute_sha256(&old_path).await.unwrap();
    let provider_checksum = data_encoding::BASE64.encode(&Sha256::digest(&original_bytes));
    let record = crate::test_helpers::TestAssetRecord::new("asset-BLOCKED")
        .filename("photo.jpg")
        .created_at(chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap())
        .size(1024)
        .checksum(&provider_checksum)
        .build();
    db.upsert_seen(&record).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "asset-BLOCKED",
        "original",
        &old_path,
        &checksum,
        Some(&checksum),
    )
    .await
    .unwrap();
    db.upsert_asset_master_mapping("PrimarySync", "asset-BLOCKED", "BLOCKED")
        .await
        .unwrap();
    db.set_metadata(
        ENUM_CONFIG_HASH_KEY,
        &download::compute_config_hash(&config),
    )
    .await
    .unwrap();
    db.set_metadata("sync_token:PrimarySync", "zone-before")
        .await
        .unwrap();
    let old_builder = make_run_cycle_download_config_builder(old_dir.path(), db.clone());
    let new_builder = make_run_cycle_download_config_builder(new_dir.path(), db.clone());
    let old_config = old_builder(
        download::SyncMode::Full,
        Arc::new(rustc_hash::FxHashSet::default()),
        Arc::new(download::AssetGroupings::default()),
        Arc::from("PrimarySync"),
    );
    let new_config = new_builder(
        download::SyncMode::Full,
        Arc::new(rustc_hash::FxHashSet::default()),
        Arc::new(download::AssetGroupings::default()),
        Arc::from("PrimarySync"),
    );
    let old_hash = download::hash_download_config(&old_config);
    let new_hash = download::hash_download_config(&new_config);
    db.set_metadata(download::DOWNLOAD_CONFIG_HASH_KEY, &old_hash)
        .await
        .unwrap();
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(original_bytes.clone()))
        .mount(&server)
        .await;
    let mut records = full_album_page_with_download(
        "PrimarySync",
        "BLOCKED",
        "unused",
        &format!("{}/photo.jpg", server.uri()),
        1024,
        &provider_checksum,
    );
    records["records"][1]["fields"]["masterRef"]["value"]["zoneID"]["ownerRecordName"] =
        serde_json::json!("_defaultOwner");
    records["records"][0]["fields"]["filenameEnc"]["value"] =
        serde_json::json!(data_encoding::BASE64.encode(b"photo.jpg"));
    let asset = crate::icloud::photos::PhotoAsset::new(
        records["records"][0].clone(),
        records["records"][1].clone(),
    );
    let expected = download::filter::expected_paths_for(&asset, new_config.as_ref())
        .remove(0)
        .path;
    std::fs::create_dir_all(expected.parent().unwrap()).unwrap();
    let conflicting_bytes = vec![7u8; 1024];
    match entry {
        ExternalReconciliationEntry::Symlink => {
            std::os::unix::fs::symlink(&external, &expected).unwrap();
        }
        ExternalReconciliationEntry::Hardlink => {
            std::fs::hard_link(&old_path, &expected).unwrap();
        }
        ExternalReconciliationEntry::SameSizeReplacement
        | ExternalReconciliationEntry::MovedDirectory => {
            std::fs::write(&expected, &conflicting_bytes).unwrap();
        }
    }
    drop(old_config);
    drop(new_config);
    drop(old_builder);
    drop(new_builder);
    drop(db);
    let source_calls = Arc::new(AtomicUsize::new(0));
    let lookup_calls = Arc::new(AtomicUsize::new(0));
    let provider = ReconciliationSession {
        records,
        source_calls: Arc::clone(&source_calls),
        lookup_calls: Arc::clone(&lookup_calls),
    };
    let (_session_dir, session) = make_shared_session_for_run_cycle().await;
    let moved = new_dir.path().join("externally-moved");
    let moved_photo = moved.join(expected.file_name().unwrap());
    let mut selected_path = None;
    let mut moved_selected = None;
    let mut completed_receipts = None;
    for cycle in 0..4 {
        // Release every production SQLite handle before each restart.
        let db = open_fixture_db(&db_path).await;
        let lib = make_run_cycle_library_state_with_album(
            "PrimarySync",
            "sync_token:PrimarySync",
            fixture_album(&db, "PrimarySync", "", Box::new(provider.clone())),
        );
        let base = make_run_cycle_download_config_builder(new_dir.path(), db.clone());
        let new_builder = |mode, excludes, groupings, library| {
            let mut config = (*base(mode, excludes, groupings, library)).clone();
            config.retry.max_retries = 0;
            Arc::new(config)
        };
        let lookups_before = lookup_calls.load(Ordering::SeqCst);
        let requests_before = server.received_requests().await.unwrap().len();
        let result = run_cycle(
            &[&lib],
            &config,
            Some(db.as_ref()),
            &new_builder,
            download::DownloadControls::download_hidden(),
            &session,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            result.failed_count, 0,
            "{entry:?}, cycle {cycle}: {result:?}"
        );
        assert!(
            result.db_sync_token_advance_safe,
            "{entry:?}, cycle {cycle}: {result:?}"
        );
        assert!(source_calls.load(Ordering::SeqCst) > 0);
        let transfers = usize::from(
            cycle == 0
                || (cycle == 1 && matches!(entry, ExternalReconciliationEntry::MovedDirectory)),
        );
        assert_eq!(
            result.stats.downloaded, transfers,
            "{entry:?}, cycle {cycle}: {result:?}"
        );
        assert_eq!(
            server.received_requests().await.unwrap().len() - requests_before,
            transfers
        );
        drop(base);
        drop(lib);
        drop(db);
        let reopened = open_fixture_db(&db_path).await;
        let rows = reopened.get_downloaded_page(0, 10).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id.as_ref(), "asset-BLOCKED");
        assert_eq!(rows[0].library.as_ref(), "PrimarySync");
        assert_eq!(rows[0].version_size, state::VersionSizeKey::Original);
        assert_eq!(
            rows[0].created_at.timestamp_millis(),
            record.created_at.timestamp_millis()
        );
        let published = rows[0].local_path.as_ref().unwrap();
        assert!(published.starts_with(new_dir.path()));
        assert_ne!(
            published, &expected,
            "foreign requested path is never adopted or overwritten"
        );
        assert_eq!(std::fs::read(published).unwrap(), original_bytes);
        if let Some(selected) = &selected_path {
            assert_eq!(
                published, selected,
                "missing selected peer must recover at its recorded path"
            );
        } else {
            selected_path = Some(published.clone());
        }
        {
            use std::os::unix::fs::MetadataExt;
            let published_metadata = std::fs::metadata(published).unwrap();
            assert_eq!(published_metadata.nlink(), 1);
            assert_ne!(
                published_metadata.ino(),
                std::fs::metadata(&old_path).unwrap().ino()
            );
        }
        assert_eq!(rows[0].local_checksum.as_deref(), Some(checksum.as_str()));
        assert_eq!(
            reopened
                .get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("zone-after")
        );
        assert_eq!(
            reopened
                .get_metadata(download::DOWNLOAD_CONFIG_HASH_KEY)
                .await
                .unwrap()
                .as_deref(),
            Some(new_hash.as_str())
        );
        assert_eq!(
            reopened
                .get_metadata(PENDING_DOWNLOAD_CONFIG_HASH_KEY)
                .await
                .unwrap()
                .as_deref(),
            None
        );
        let reservations = reopened.get_reconciliation_reservations().await.unwrap();
        let receipts = reopened.get_downloaded_path_records().await.unwrap();
        assert_eq!(receipts.len(), 2);
        for path in [&old_path, published] {
            let receipt = receipts
                .iter()
                .find(|receipt| receipt.local_path.as_ref() == Some(path))
                .unwrap();
            assert_eq!(receipt.id, "asset-BLOCKED");
            assert_eq!(receipt.local_checksum.as_deref(), Some(checksum.as_str()));
        }
        let receipts: Vec<_> = receipts
            .iter()
            .map(|receipt| {
                (
                    receipt.id.clone(),
                    receipt.library.clone(),
                    receipt.local_path.clone(),
                    receipt.local_checksum.clone(),
                )
            })
            .collect();
        if cycle == 1 {
            completed_receipts = Some((receipts, reservations));
        } else if cycle > 1 {
            assert_eq!(
                (receipts, reservations),
                *completed_receipts.as_ref().unwrap()
            );
            assert_eq!(
                lookup_calls.load(Ordering::SeqCst),
                lookups_before,
                "quiet reopen must not reconfirm or relocate media"
            );
        }
        assert_eq!(
            std::fs::read(&unrelated).unwrap(),
            b"external user metadata"
        );
        assert_eq!(
            std::fs::metadata(&old_path).unwrap().modified().unwrap(),
            if cycle < 2 {
                old_time
            } else {
                UNIX_EPOCH + Duration::from_secs(23456)
            }
        );
        if cycle < 2 {
            match entry {
                ExternalReconciliationEntry::Symlink => {
                    assert_eq!(std::fs::read_link(&expected).unwrap(), external)
                }
                ExternalReconciliationEntry::Hardlink => {
                    use std::os::unix::fs::MetadataExt;
                    assert_eq!(
                        std::fs::metadata(&expected).unwrap().ino(),
                        std::fs::metadata(&old_path).unwrap().ino()
                    );
                    assert_eq!(std::fs::metadata(&old_path).unwrap().nlink(), 2);
                }
                ExternalReconciliationEntry::SameSizeReplacement
                | ExternalReconciliationEntry::MovedDirectory => {
                    assert_eq!(std::fs::read(&expected).unwrap(), conflicting_bytes);
                }
            }
        } else {
            assert!(
                !expected.exists(),
                "removing the foreign path does not authorize another copy"
            );
        }
        if matches!(entry, ExternalReconciliationEntry::MovedDirectory) && cycle >= 1 {
            assert_eq!(std::fs::read(&moved_photo).unwrap(), conflicting_bytes);
            assert_eq!(
                std::fs::read(moved_selected.as_ref().unwrap()).unwrap(),
                original_bytes
            );
        }
        drop(reopened);
        if cycle == 0 && matches!(entry, ExternalReconciliationEntry::MovedDirectory) {
            moved_selected = Some(moved.join(selected_path.as_ref().unwrap().file_name().unwrap()));
            std::fs::rename(expected.parent().unwrap(), &moved).unwrap();
            std::fs::create_dir_all(expected.parent().unwrap()).unwrap();
            std::fs::write(&expected, &conflicting_bytes).unwrap();
        }
        if cycle == 1 {
            std::fs::remove_file(&expected).unwrap();
            // A same-byte atomic replacement and externally altered timestamp
            // are valid source evidence on restart; no old inode is reused.
            let replacement = old_dir.path().join("external-replacement.jpg");
            std::fs::write(&replacement, &original_bytes).unwrap();
            std::fs::File::options()
                .write(true)
                .open(&replacement)
                .unwrap()
                .set_times(
                    std::fs::FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(23456)),
                )
                .unwrap();
            std::fs::rename(&replacement, &old_path).unwrap();
        }
        assert_eq!(std::fs::read(&external).unwrap(), vec![0u8; 1024]);
        assert_eq!(std::fs::read(&old_path).unwrap(), original_bytes);
    }
}

#[tokio::test]
async fn run_cycle_download_config_hash_drift_keeps_source_incremental() {
    let config = make_run_cycle_config();
    let (_database_dir, db) = fixture_db().await;
    let old_download_dir = tempfile::tempdir().expect("old download tempdir");
    let new_download_dir = tempfile::tempdir().expect("new download tempdir");

    let old_build_download_config =
        make_run_cycle_download_config_builder(old_download_dir.path(), db.clone());
    let old_download_config = old_build_download_config(
        download::SyncMode::Full,
        Arc::new(rustc_hash::FxHashSet::default()),
        Arc::new(download::AssetGroupings::default()),
        Arc::from("PrimarySync"),
    );
    let old_hash = download::hash_download_config(&old_download_config);
    db.set_metadata(download::DOWNLOAD_CONFIG_HASH_KEY, &old_hash)
        .await
        .expect("seed old download hash");
    db.set_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"), "zone-tok-prev")
        .await
        .expect("seed zone token");

    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
    let lib_state = make_run_cycle_library_state_with_album(
        "PrimarySync",
        &format!("{SYNC_TOKEN_PREFIX}PrimarySync"),
        empty_fixture_album(&db, "PrimarySync", "", "zone-tok-new"),
    );
    let states = vec![&lib_state];
    let observed_modes = Arc::new(std::sync::Mutex::new(Vec::<download::SyncMode>::new()));
    let build_download_config = make_recording_run_cycle_download_config_builder(
        new_download_dir.path(),
        db.clone(),
        Arc::clone(&observed_modes),
    );
    let new_hash = download::hash_download_config(&build_download_config(
        download::SyncMode::Full,
        Arc::new(rustc_hash::FxHashSet::default()),
        Arc::new(download::AssetGroupings::default()),
        Arc::from("PrimarySync"),
    ));

    let result = run_cycle(
        &states,
        &config,
        Some(db.as_ref()),
        &build_download_config,
        download::DownloadControls::download_hidden(),
        &shared_session,
        &CancellationToken::new(),
    )
    .await
    .expect("run cycle");

    assert_eq!(result.failed_count, 0);
    assert_eq!(result.stats.full_enumeration_reason, None);
    let observed_modes = observed_modes.lock().expect("recorded modes lock").clone();
    assert!(
        matches!(
            observed_modes.last(),
            Some(download::SyncMode::Incremental { zone_sync_token })
                if zone_sync_token == "zone-tok-prev"
        ),
        "path-only drift must preserve incremental source tracking: {observed_modes:?}"
    );
    assert_eq!(
        db.get_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"))
            .await
            .expect("read zone token")
            .as_deref(),
        Some("zone-tok-new"),
        "the incremental source pass should refresh the selected zone token after success"
    );
    assert_eq!(
        db.get_metadata(download::DOWNLOAD_CONFIG_HASH_KEY)
            .await
            .expect("read active download hash")
            .as_deref(),
        Some(new_hash.as_str()),
        "an empty catalog completes local path reconciliation immediately"
    );
    assert_eq!(
        db.get_metadata(PENDING_DOWNLOAD_CONFIG_HASH_KEY)
            .await
            .expect("read pending download hash"),
        None
    );
}

#[tokio::test]
async fn run_cycle_date_bound_expansion_preserves_existing_media_and_reaches_steady_state() {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    #[derive(Clone, Debug)]
    struct DateBoundSession {
        records: Arc<Vec<serde_json::Value>>,
    }

    #[async_trait::async_trait]
    impl crate::icloud::photos::PhotosSession for DateBoundSession {
        async fn post(
            &self,
            url: &str,
            body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<serde_json::Value> {
            if url.contains("/internal/records/query/batch") {
                return Ok(album_count_response((self.records.len() / 2) as u64));
            }
            if url.contains("/records/lookup?") {
                let request: serde_json::Value = serde_json::from_str(&body)?;
                let records: Vec<_> = self
                    .records
                    .iter()
                    .filter(|record| {
                        request["records"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|wanted| wanted["recordName"] == record["recordName"])
                    })
                    .collect();
                return Ok(serde_json::json!({"records": records}));
            }
            if url.contains("/records/query?") {
                let request: serde_json::Value = serde_json::from_str(&body)?;
                let offset = request["query"]["filterBy"]
                    .as_array()
                    .and_then(|filters| {
                        filters.iter().find_map(|filter| {
                            (filter["fieldName"] == "startRank")
                                .then(|| filter["fieldValue"]["value"].as_u64())
                                .flatten()
                        })
                    })
                    .unwrap_or(0);
                let limit = request["resultsLimit"].as_u64().unwrap_or(0) / 2;
                let records: Vec<_> = self
                    .records
                    .chunks_exact(2)
                    .skip(offset as usize)
                    .take(limit as usize)
                    .flatten()
                    .cloned()
                    .collect();
                return Ok(serde_json::json!({
                    "records": records,
                    "syncToken": "zone-token-inventory"
                }));
            }
            if url.contains("/changes/zone?") {
                return Ok(serde_json::json!({
                    "zones": [{
                        "zoneID": {
                            "zoneName": "PrimarySync",
                            "ownerRecordName": "_defaultOwner"
                        },
                        "syncToken": "zone-token-incremental",
                        "moreComing": false,
                        "records": []
                    }]
                }));
            }
            Ok(serde_json::json!({"records": []}))
        }

        fn clone_box(&self) -> Box<dyn crate::icloud::photos::PhotosSession> {
            Box::new(self.clone())
        }
    }

    fn asset_records(
        record_name: &str,
        filename: &str,
        created: chrono::DateTime<chrono::Utc>,
        download_url: &str,
        body: &[u8],
    ) -> Vec<serde_json::Value> {
        let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(body));
        let mut page = full_album_page_with_download(
            "PrimarySync",
            record_name,
            "unused",
            download_url,
            body.len() as u64,
            &checksum,
        );
        page["records"][0]["fields"]["filenameEnc"]["value"] =
            serde_json::json!(base64::engine::general_purpose::STANDARD.encode(filename));
        page["records"][1]["fields"]["assetDate"]["value"] =
            serde_json::json!(created.timestamp_millis());
        page["records"][1]["fields"]["addedDate"]["value"] =
            serde_json::json!(created.timestamp_millis());
        page["records"][1]["fields"]["masterRef"]["value"]["zoneID"]["ownerRecordName"] =
            serde_json::json!("_defaultOwner");
        page["records"].as_array().expect("asset records").clone()
    }

    fn snapshot_files(
        root: &std::path::Path,
    ) -> std::collections::BTreeMap<std::path::PathBuf, Vec<u8>> {
        fn visit(
            root: &std::path::Path,
            dir: &std::path::Path,
            files: &mut std::collections::BTreeMap<std::path::PathBuf, Vec<u8>>,
        ) {
            for entry in std::fs::read_dir(dir).expect("read media directory") {
                let entry = entry.expect("read media entry");
                let path = entry.path();
                if path.is_dir() {
                    visit(root, &path, files);
                } else {
                    files.insert(
                        path.strip_prefix(root)
                            .expect("relative media path")
                            .to_path_buf(),
                        std::fs::read(path).expect("read media file"),
                    );
                }
            }
        }

        let mut files = std::collections::BTreeMap::new();
        visit(root, root, &mut files);
        files
    }

    let server = crate::start_wiremock_or_skip!();
    let january_body = &[
        0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00, 0x00,
        0x01, 0x00, 0x01, 0x00, 0x00, 0xFF, 0xD9,
    ];
    let april_body = &[
        0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00, 0x00,
        0x02, 0x00, 0x01, 0x00, 0x00, 0xFF, 0xD9,
    ];
    Mock::given(method("GET"))
        .and(path("/january.jpg"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(january_body)
                .insert_header("content-type", "image/jpeg"),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/april.jpg"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(april_body)
                .insert_header("content-type", "image/jpeg"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let january_created = chrono::NaiveDate::from_ymd_opt(2026, 1, 15)
        .unwrap()
        .and_hms_opt(12, 0, 0)
        .unwrap()
        .and_utc();
    let april_created = chrono::NaiveDate::from_ymd_opt(2026, 4, 15)
        .unwrap()
        .and_hms_opt(12, 0, 0)
        .unwrap()
        .and_utc();
    let mut records = asset_records(
        "APRIL",
        "april.jpg",
        april_created,
        &format!("{}/april.jpg", server.uri()),
        april_body,
    );
    records.extend(asset_records(
        "JANUARY",
        "january.jpg",
        january_created,
        &format!("{}/january.jpg", server.uri()),
        january_body,
    ));
    let (_database_dir, db) = fixture_db().await;
    let album = fixture_album(
        &db,
        "PrimarySync",
        "",
        Box::new(DateBoundSession {
            records: Arc::new(records),
        }),
    );
    let lib_state =
        make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
    let download_dir = tempfile::tempdir().expect("download tempdir");
    let old_lower_bound = config::CreatedDateFilter::CaptureDate(
        chrono::NaiveDate::from_ymd_opt(2026, 4, 1).unwrap(),
    );
    let new_lower_bound = config::CreatedDateFilter::CaptureDate(
        chrono::NaiveDate::from_ymd_opt(2026, 1, 1).unwrap(),
    );
    let upper_bound = config::CreatedDateFilter::CaptureDate(
        chrono::NaiveDate::from_ymd_opt(2026, 5, 31).unwrap(),
    );
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

    let mut config = make_run_cycle_config();
    config.filters.skip_created_before = Some(old_lower_bound);
    config.filters.skip_created_after = Some(upper_bound);
    let old_enum_hash = download::compute_config_hash(&config);
    let old_builder = make_run_cycle_download_config_builder_with_options(
        download_dir.path(),
        db.clone(),
        RunCycleDownloadConfigOptions {
            skip_created_before: Some(old_lower_bound),
            skip_created_after: Some(upper_bound),
            ..RunCycleDownloadConfigOptions::default()
        },
    );
    let first_result = run_cycle(
        &[&lib_state],
        &config,
        Some(db.as_ref()),
        &old_builder,
        download::DownloadControls::download_hidden(),
        &shared_session,
        &CancellationToken::new(),
    )
    .await
    .expect("run initial date-bound cycle");
    assert_eq!(
        first_result.stats.downloaded, 1,
        "{:#?}",
        first_result.stats
    );

    let first_snapshot = snapshot_files(download_dir.path());
    assert_eq!(first_snapshot.len(), 1);
    let april_record = db
        .get_downloaded_page(0, 10)
        .await
        .expect("read initial downloaded rows")
        .into_iter()
        .find(|record| record.id.as_ref() == "asset-APRIL")
        .expect("April downloaded row");
    let april_path = april_record.local_path.expect("April local path");
    assert_eq!(tokio::fs::read(&april_path).await.unwrap(), april_body);
    assert_eq!(
        db.get_metadata(ENUM_CONFIG_HASH_KEY)
            .await
            .expect("read initial enumeration hash")
            .as_deref(),
        Some(old_enum_hash.as_str())
    );
    db.set_metadata(
        &format!("{SYNC_TOKEN_PREFIX}PrimarySync"),
        "zone-token-old-bound",
    )
    .await
    .expect("seed prior provider checkpoint");
    let checkpoint_before_expansion = db
        .get_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"))
        .await
        .expect("read initial provider checkpoint");
    assert_eq!(
        checkpoint_before_expansion.as_deref(),
        Some("zone-token-old-bound")
    );

    let old_download_config = old_builder(
        download::SyncMode::Full,
        Arc::new(rustc_hash::FxHashSet::default()),
        Arc::new(download::AssetGroupings::default()),
        Arc::from("PrimarySync"),
    );
    db.set_metadata(
        download::DOWNLOAD_CONFIG_HASH_KEY,
        &download::hash_legacy_download_config(&old_download_config),
    )
    .await
    .expect("seed pre-fix path hash");
    drop(old_builder);

    config.filters.skip_created_before = Some(new_lower_bound);
    let current_enum_hash = download::compute_config_hash(&config);
    let current_builder = make_run_cycle_download_config_builder_with_options(
        download_dir.path(),
        db.clone(),
        RunCycleDownloadConfigOptions {
            skip_created_before: Some(new_lower_bound),
            skip_created_after: Some(upper_bound),
            ..RunCycleDownloadConfigOptions::default()
        },
    );
    let current_download_config = current_builder(
        download::SyncMode::Full,
        Arc::new(rustc_hash::FxHashSet::default()),
        Arc::new(download::AssetGroupings::default()),
        Arc::from("PrimarySync"),
    );
    let current_download_hash = download::hash_download_config(&current_download_config);
    assert_ne!(
        download::hash_legacy_download_config(&old_download_config),
        download::hash_legacy_download_config(&current_download_config)
    );

    let second_result = run_cycle(
        &[&lib_state],
        &config,
        Some(db.as_ref()),
        &current_builder,
        download::DownloadControls::download_hidden(),
        &shared_session,
        &CancellationToken::new(),
    )
    .await
    .expect("run expanded date-bound cycle");
    assert_eq!(second_result.stats.downloaded, 1);
    assert_ne!(
        second_result.stats.full_enumeration_reason,
        Some(download::FullEnumerationReason::DownloadConfigHashDrift)
    );

    let second_snapshot = snapshot_files(download_dir.path());
    assert_eq!(second_snapshot.len(), 2);
    assert!(
        first_snapshot
            .iter()
            .all(|entry| second_snapshot.get(entry.0) == Some(entry.1))
    );
    assert_eq!(tokio::fs::read(&april_path).await.unwrap(), april_body);
    let downloaded = db
        .get_downloaded_page(0, 10)
        .await
        .expect("read expanded downloaded rows");
    assert_eq!(downloaded.len(), 2);
    assert_eq!(
        downloaded
            .iter()
            .find(|record| record.id.as_ref() == "asset-APRIL")
            .and_then(|record| record.local_path.as_deref()),
        Some(april_path.as_path())
    );
    assert_eq!(
        db.get_metadata(download::DOWNLOAD_CONFIG_HASH_KEY)
            .await
            .expect("read current path hash")
            .as_deref(),
        Some(current_download_hash.as_str())
    );
    assert_eq!(
        db.get_metadata(PENDING_DOWNLOAD_CONFIG_HASH_KEY)
            .await
            .expect("read pending path hash"),
        None
    );
    assert_eq!(
        db.get_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"))
            .await
            .expect("read provider checkpoint")
            .as_deref(),
        Some("zone-token-incremental")
    );
    assert_eq!(
        db.get_metadata(ENUM_CONFIG_HASH_KEY)
            .await
            .expect("read enumeration hash")
            .as_deref(),
        Some(current_enum_hash.as_str())
    );
    assert_eq!(
        db.get_metadata(PENDING_ENUM_CONFIG_HASH_KEY)
            .await
            .expect("read pending enumeration hash"),
        None
    );

    let third_result = run_cycle(
        &[&lib_state],
        &config,
        Some(db.as_ref()),
        &current_builder,
        download::DownloadControls::download_hidden(),
        &shared_session,
        &CancellationToken::new(),
    )
    .await
    .expect("run steady-state date-bound cycle");
    assert_eq!(third_result.stats.downloaded, 0);
    assert_eq!(snapshot_files(download_dir.path()), second_snapshot);
    assert_ne!(
        third_result.stats.full_enumeration_reason,
        Some(download::FullEnumerationReason::DownloadConfigHashDrift)
    );
    assert_eq!(
        db.get_metadata(download::DOWNLOAD_CONFIG_HASH_KEY)
            .await
            .expect("read steady path hash")
            .as_deref(),
        Some(current_download_hash.as_str())
    );
    assert_eq!(
        db.get_metadata(PENDING_DOWNLOAD_CONFIG_HASH_KEY)
            .await
            .expect("read steady pending path hash"),
        None
    );
    assert_eq!(
        db.get_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"))
            .await
            .expect("read steady provider checkpoint")
            .as_deref(),
        Some("zone-token-incremental")
    );
    assert_eq!(
        db.get_metadata(ENUM_CONFIG_HASH_KEY)
            .await
            .expect("read steady enumeration hash")
            .as_deref(),
        Some(current_enum_hash.as_str())
    );
    assert_eq!(
        db.get_metadata(PENDING_ENUM_CONFIG_HASH_KEY)
            .await
            .expect("read steady pending enumeration hash"),
        None
    );
}

#[tokio::test]
async fn run_cycle_multi_pass_persists_base_download_config_hash() {
    let config = make_run_cycle_config();
    let (_database_dir, db) = fixture_db().await;
    let download_dir = tempfile::tempdir().expect("download tempdir");
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
    let passes = vec![
        crate::commands::AlbumPass {
            kind: crate::commands::PassKind::Album,
            album: empty_fixture_album(&db, "PrimarySync", "Vacation", "zone-tok"),
            exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
        },
        crate::commands::AlbumPass {
            kind: crate::commands::PassKind::Album,
            album: empty_fixture_album(&db, "PrimarySync", "Family", "zone-tok"),
            exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
        },
        crate::commands::AlbumPass {
            kind: crate::commands::PassKind::Unfiled,
            album: empty_fixture_album(&db, "PrimarySync", "", "zone-tok"),
            exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
        },
    ];
    let lib_state = make_run_cycle_library_state_with_passes(
        "PrimarySync",
        &format!("{SYNC_TOKEN_PREFIX}PrimarySync"),
        passes.clone(),
    );
    let options = RunCycleDownloadConfigOptions {
        per_pass_paths: true,
        ..RunCycleDownloadConfigOptions::default()
    };
    let build_download_config = make_run_cycle_download_config_builder_with_options(
        download_dir.path(),
        db.clone(),
        options,
    );
    let base_config = build_download_config(
        download::SyncMode::Full,
        Arc::new(rustc_hash::FxHashSet::default()),
        Arc::new(download::AssetGroupings::default()),
        Arc::from("PrimarySync"),
    );
    let base_hash = download::hash_download_config(&base_config);
    let pass_hashes: Vec<String> = passes
        .iter()
        .map(|pass| download::hash_download_config(&base_config.with_pass(pass)))
        .collect();

    let result = run_cycle(
        &[&lib_state],
        &config,
        Some(db.as_ref()),
        &build_download_config,
        download::DownloadControls::download_hidden(),
        &shared_session,
        &CancellationToken::new(),
    )
    .await
    .expect("multi-pass cycle should complete");

    assert_eq!(result.failed_count, 0);
    let stored_hash = db
        .get_metadata(download::DOWNLOAD_CONFIG_HASH_KEY)
        .await
        .expect("read stored download hash")
        .expect("download hash should be persisted");
    assert_eq!(
        stored_hash, base_hash,
        "global download config hash must be the base run-level hash"
    );
    assert!(
        pass_hashes.iter().take(2).all(|hash| hash != &stored_hash),
        "album-expanded folder templates must not overwrite the global hash: {pass_hashes:?}"
    );
}

#[tokio::test]
async fn unchanged_multi_pass_second_cycle_is_not_download_config_hash_drift() {
    let config = make_run_cycle_config();
    let (_database_dir, db) = fixture_db().await;
    let download_dir = tempfile::tempdir().expect("download tempdir");
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
    let options = RunCycleDownloadConfigOptions {
        per_pass_paths: true,
        ..RunCycleDownloadConfigOptions::default()
    };
    let first_state = make_run_cycle_library_state_with_passes(
        "PrimarySync",
        &format!("{SYNC_TOKEN_PREFIX}PrimarySync"),
        vec![
            crate::commands::AlbumPass {
                kind: crate::commands::PassKind::Album,
                album: empty_fixture_album(&db, "PrimarySync", "Vacation", "zone-tok-1"),
                exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
            },
            crate::commands::AlbumPass {
                kind: crate::commands::PassKind::Unfiled,
                album: empty_fixture_album(&db, "PrimarySync", "", "zone-tok-1"),
                exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
            },
        ],
    );
    let first_builder = make_run_cycle_download_config_builder_with_options(
        download_dir.path(),
        db.clone(),
        options,
    );
    let first_result = run_cycle(
        &[&first_state],
        &config,
        Some(db.as_ref()),
        &first_builder,
        download::DownloadControls::download_hidden(),
        &shared_session,
        &CancellationToken::new(),
    )
    .await
    .expect("first multi-pass cycle should complete");
    assert_eq!(first_result.failed_count, 0);

    let second_state = make_run_cycle_library_state_with_passes(
        "PrimarySync",
        &format!("{SYNC_TOKEN_PREFIX}PrimarySync"),
        vec![crate::commands::AlbumPass {
            kind: crate::commands::PassKind::Unfiled,
            album: empty_fixture_album(&db, "PrimarySync", "", "zone-tok-2"),
            exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
        }],
    );
    let observed_modes = Arc::new(std::sync::Mutex::new(Vec::<download::SyncMode>::new()));
    let base_builder = make_run_cycle_download_config_builder_with_options(
        download_dir.path(),
        db.clone(),
        options,
    );
    let second_builder = {
        let observed_modes = Arc::clone(&observed_modes);
        move |sync_mode: download::SyncMode,
              exclude_asset_ids: Arc<rustc_hash::FxHashSet<String>>,
              asset_groupings: Arc<download::AssetGroupings>,
              library: Arc<str>| {
            observed_modes
                .lock()
                .expect("recorded modes lock")
                .push(sync_mode.clone());
            base_builder(sync_mode, exclude_asset_ids, asset_groupings, library)
        }
    };
    let second_result = run_cycle(
        &[&second_state],
        &config,
        Some(db.as_ref()),
        &second_builder,
        download::DownloadControls::download_hidden(),
        &shared_session,
        &CancellationToken::new(),
    )
    .await
    .expect("second cycle should complete");

    assert_ne!(
        second_result.stats.full_enumeration_reason,
        Some(download::FullEnumerationReason::DownloadConfigHashDrift),
        "pass-expanded hash from first run must not force path-drift reconciliation"
    );
    assert!(
        observed_modes
            .lock()
            .expect("recorded modes lock")
            .iter()
            .any(|mode| matches!(mode, download::SyncMode::Incremental { .. })),
        "unchanged config with a stored token should reach the incremental decision path"
    );
}

// ── check_and_persist_enum_config_hash ─────────────────────────────

#[tokio::test]
async fn enum_config_hash_initial_persists_only() {
    let db = state::SqliteStateDb::open_in_memory().expect("open in-memory state DB");
    db.set_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"), "tok-abc")
        .await
        .expect("set token");

    let outcome = check_and_persist_enum_config_hash(&db, "hash-1").await;

    assert_eq!(outcome, EnumConfigHashOutcome::Initial);
    assert_eq!(
        db.get_metadata(ENUM_CONFIG_HASH_KEY)
            .await
            .unwrap()
            .as_deref(),
        Some("hash-1"),
    );
    // First run must NOT clear pre-existing sync tokens.
    assert_eq!(
        db.get_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"))
            .await
            .unwrap()
            .as_deref(),
        Some("tok-abc"),
    );
}

#[tokio::test]
async fn enum_config_hash_drift_stages_reconciliation_and_preserves_tokens() {
    let db = state::SqliteStateDb::open_in_memory().expect("open in-memory state DB");
    db.set_metadata(ENUM_CONFIG_HASH_KEY, "old-hash")
        .await
        .expect("seed old hash");
    db.set_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"), "tok-primary")
        .await
        .expect("seed primary token");
    db.set_metadata(
        &format!("{SYNC_TOKEN_PREFIX}SharedSync-AAAA1111"),
        "tok-shared",
    )
    .await
    .expect("seed shared token");

    let outcome = check_and_persist_enum_config_hash(&db, "new-hash").await;

    assert_eq!(outcome, EnumConfigHashOutcome::Changed);
    assert_eq!(
        db.get_metadata(ENUM_CONFIG_HASH_KEY)
            .await
            .unwrap()
            .as_deref(),
        Some("old-hash"),
    );
    assert_eq!(
        db.get_metadata(PENDING_ENUM_CONFIG_HASH_KEY)
            .await
            .unwrap()
            .as_deref(),
        Some("new-hash")
    );
    assert_eq!(
        db.get_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"))
            .await
            .unwrap()
            .as_deref(),
        Some("tok-primary")
    );
    assert_eq!(
        db.get_metadata(&format!("{SYNC_TOKEN_PREFIX}SharedSync-AAAA1111"))
            .await
            .unwrap()
            .as_deref(),
        Some("tok-shared")
    );
}

#[tokio::test]
async fn enum_config_hash_stage_failure_keeps_old_hash_and_tokens() {
    let inner = make_state_db();
    inner
        .set_metadata(ENUM_CONFIG_HASH_KEY, "old-hash")
        .await
        .expect("seed enum hash");
    inner
        .set_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"), "old-zone-token")
        .await
        .expect("seed zone token");
    let db: Arc<dyn download::DownloadStore> = Arc::new(FailingMetadataSetDb::new(
        Arc::clone(&inner),
        MetadataSetFailure::Exact(PENDING_ENUM_CONFIG_HASH_KEY),
        "simulated pending hash write failure",
    ));

    let outcome = check_and_persist_enum_config_hash(db.as_ref(), "new-hash").await;

    assert_eq!(outcome, EnumConfigHashOutcome::ChangedTokenPurgeFailed);
    assert_eq!(
        inner
            .get_metadata(ENUM_CONFIG_HASH_KEY)
            .await
            .expect("read enum hash")
            .as_deref(),
        Some("old-hash"),
        "new hash must not become active before reconciliation"
    );
    assert_eq!(
        inner
            .get_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"))
            .await
            .expect("read zone token")
            .as_deref(),
        Some("old-zone-token"),
        "the last safe token must survive a failed reconciliation-stage write"
    );
}

#[tokio::test]
async fn enum_config_hash_unchanged_is_noop() {
    let db = state::SqliteStateDb::open_in_memory().expect("open in-memory state DB");
    db.set_metadata(ENUM_CONFIG_HASH_KEY, "stable-hash")
        .await
        .expect("seed stable hash");
    db.set_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"), "tok-keep")
        .await
        .expect("seed token");

    let outcome = check_and_persist_enum_config_hash(&db, "stable-hash").await;

    assert_eq!(outcome, EnumConfigHashOutcome::Unchanged);
    assert_eq!(
        db.get_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"))
            .await
            .unwrap()
            .as_deref(),
        Some("tok-keep"),
    );
}

#[tokio::test]
async fn enum_config_revert_clears_pending_reconciliation() {
    let db = state::SqliteStateDb::open_in_memory().expect("open in-memory state DB");
    db.set_metadata(ENUM_CONFIG_HASH_KEY, "active-hash")
        .await
        .unwrap();
    db.set_metadata(PENDING_ENUM_CONFIG_HASH_KEY, "abandoned-hash")
        .await
        .unwrap();
    db.set_metadata(
        &pending_zone_token_key("abandoned-hash", "PrimarySync"),
        "candidate-token",
    )
    .await
    .unwrap();

    let outcome = check_and_persist_enum_config_hash(&db, "active-hash").await;

    assert_eq!(outcome, EnumConfigHashOutcome::Unchanged);
    assert_eq!(
        db.get_metadata(PENDING_ENUM_CONFIG_HASH_KEY).await.unwrap(),
        None
    );
    assert_eq!(
        db.get_metadata(&pending_zone_token_key("abandoned-hash", "PrimarySync"))
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn enum_config_new_drift_discards_superseded_zone_candidates() {
    let db = state::SqliteStateDb::open_in_memory().expect("open in-memory state DB");
    db.set_metadata(ENUM_CONFIG_HASH_KEY, "active-hash")
        .await
        .unwrap();
    db.set_metadata(PENDING_ENUM_CONFIG_HASH_KEY, "superseded-hash")
        .await
        .unwrap();
    db.set_metadata(
        &pending_zone_token_key("superseded-hash", "PrimarySync"),
        "candidate-token",
    )
    .await
    .unwrap();

    let outcome = check_and_persist_enum_config_hash(&db, "replacement-hash").await;

    assert_eq!(outcome, EnumConfigHashOutcome::Changed);
    assert_eq!(
        db.get_metadata(PENDING_ENUM_CONFIG_HASH_KEY)
            .await
            .unwrap()
            .as_deref(),
        Some("replacement-hash")
    );
    assert_eq!(
        db.get_metadata(&pending_zone_token_key("superseded-hash", "PrimarySync"))
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn download_config_hash_drift_stages_reconciliation_without_clearing_token() {
    let db = state::SqliteStateDb::open_in_memory().expect("open in-memory state DB");
    db.set_metadata(download::DOWNLOAD_CONFIG_HASH_KEY, "old-download-hash")
        .await
        .expect("seed old path hash");
    db.set_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"), "tok-keep")
        .await
        .expect("seed token");

    let outcome = check_download_config_hash_for_cycle(
        &db,
        "current-download-hash",
        "legacy-current-download-hash",
    )
    .await;

    assert_eq!(outcome, DownloadConfigHashOutcome::Changed);
    assert_eq!(
        db.get_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"))
            .await
            .unwrap(),
        Some("tok-keep".to_string()),
        "path drift must preserve the active provider cursor"
    );
    assert_eq!(
        db.get_metadata(download::DOWNLOAD_CONFIG_HASH_KEY)
            .await
            .unwrap(),
        Some("old-download-hash".to_string()),
        "the active path hash changes only after reconciliation"
    );
    assert_eq!(
        db.get_metadata(PENDING_DOWNLOAD_CONFIG_HASH_KEY)
            .await
            .unwrap(),
        Some("current-download-hash".to_string()),
        "the candidate path hash must be durable while reconciliation runs"
    );
}

#[tokio::test]
async fn download_config_hash_initial_persists_current_hash() {
    let db = state::SqliteStateDb::open_in_memory().expect("open in-memory state DB");
    db.set_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"), "tok-keep")
        .await
        .expect("seed token");

    let outcome = check_download_config_hash_for_cycle(
        &db,
        "current-download-hash",
        "legacy-current-download-hash",
    )
    .await;

    assert_eq!(outcome, DownloadConfigHashOutcome::Unchanged);
    assert_eq!(
        db.get_metadata(download::DOWNLOAD_CONFIG_HASH_KEY)
            .await
            .unwrap()
            .as_deref(),
        Some("current-download-hash"),
        "first observation should persist the stable run-level path hash"
    );
    assert_eq!(
        db.get_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"))
            .await
            .unwrap()
            .as_deref(),
        Some("tok-keep"),
        "initial hash persistence must not purge existing tokens"
    );
}

#[tokio::test]
async fn download_config_revert_clears_pending_reconciliation() {
    let db = state::SqliteStateDb::open_in_memory().expect("open in-memory state DB");
    db.set_metadata(download::DOWNLOAD_CONFIG_HASH_KEY, "active-hash")
        .await
        .unwrap();
    db.set_metadata(PENDING_DOWNLOAD_CONFIG_HASH_KEY, "abandoned-hash")
        .await
        .unwrap();

    let outcome =
        check_download_config_hash_for_cycle(&db, "active-hash", "legacy-active-hash").await;

    assert_eq!(outcome, DownloadConfigHashOutcome::Unchanged);
    assert_eq!(
        db.get_metadata(PENDING_DOWNLOAD_CONFIG_HASH_KEY)
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn download_config_legacy_hash_migrates_without_reconciliation() {
    let db = state::SqliteStateDb::open_in_memory().expect("open in-memory state DB");
    db.set_metadata(download::DOWNLOAD_CONFIG_HASH_KEY, "legacy-current-hash")
        .await
        .unwrap();
    db.set_metadata(PENDING_DOWNLOAD_CONFIG_HASH_KEY, "abandoned-hash")
        .await
        .unwrap();
    db.set_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"), "tok-keep")
        .await
        .unwrap();

    let outcome =
        check_download_config_hash_for_cycle(&db, "current-path-hash", "legacy-current-hash").await;

    assert_eq!(outcome, DownloadConfigHashOutcome::Unchanged);
    assert_eq!(
        db.get_metadata(download::DOWNLOAD_CONFIG_HASH_KEY)
            .await
            .unwrap()
            .as_deref(),
        Some("current-path-hash")
    );
    assert_eq!(
        db.get_metadata(PENDING_DOWNLOAD_CONFIG_HASH_KEY)
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        db.get_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"))
            .await
            .unwrap()
            .as_deref(),
        Some("tok-keep")
    );
}

#[tokio::test]
async fn unresolved_identity_inventory_requires_a_delta_bridge() {
    use crate::state::{SparseAttemptOutcome, SparseEvidence, SparseIdentityStore, SparseSourceId};
    for (prior_token, sparse) in [None, Some("retained-before")]
        .into_iter()
        .flat_map(|token| [false, true].map(|sparse| (token, sparse)))
    {
        let (_database_dir, inner) = fixture_db().await;
        let marker = state::unresolved_identity_key("PrimarySync");
        if sparse {
            let key = SparseEvidence::new(
                r#"[1,"private-child","SharedSync-absent","private-owner"]"#.into(),
            );
            let row = inner
                .observe_sparse_identity(
                    "PrimarySync",
                    &SparseSourceId::new("private-source"),
                    &key,
                    chrono::Utc::now(),
                )
                .await
                .unwrap();
            inner
                .record_sparse_attempt(
                    &row,
                    SparseAttemptOutcome::Unresolved(key),
                    chrono::Utc::now(),
                )
                .await
                .unwrap();
        }
        let blocked = prior_token.is_none() || sparse;
        inner.set_metadata(&marker, "1").await.unwrap();
        inner
            .set_metadata(crate::sync_cycle::ENUM_CONFIG_HASH_KEY, "old-config")
            .await
            .unwrap();
        if let Some(token) = prior_token {
            inner
                .set_metadata("sync_token:PrimarySync", token)
                .await
                .unwrap();
        }
        let dir = tempfile::tempdir().unwrap();
        let album = fixture_album(
            &inner,
            "PrimarySync",
            "",
            Box::new(ConfigBridgeSession::new(
                "PrimarySync",
                "inventory-only",
                "bridged-after",
            )),
        );
        let library =
            make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
        let builder = make_run_cycle_download_config_builder(dir.path(), inner.clone());
        let config = make_run_cycle_config();
        let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
        for _ in 0..2 {
            let result = run_cycle(
                &[&library],
                &config,
                Some(inner.as_ref()),
                &builder,
                download::DownloadControls::download_hidden(),
                &shared_session,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
            assert_eq!(result.failed_count > 0, blocked);
            assert_eq!(
                inner.get_metadata(&marker).await.unwrap().is_some(),
                blocked
            );
            assert_eq!(
                inner
                    .get_metadata("sync_token:PrimarySync")
                    .await
                    .unwrap()
                    .as_deref(),
                if sparse {
                    prior_token
                } else {
                    prior_token.map(|_| "bridged-after")
                }
            );
            assert_eq!(result.stats.downloaded, 0);
            assert_eq!(
                inner.sparse_identities("PrimarySync").await.unwrap().len(),
                usize::from(sparse)
            );
            if blocked {
                assert_eq!(
                    inner
                        .get_metadata(crate::sync_cycle::ENUM_CONFIG_HASH_KEY)
                        .await
                        .unwrap()
                        .as_deref(),
                    Some("old-config")
                );
            }
        }
    }
}

#[tokio::test]
async fn inventory_coverage_survives_failed_cursor_commit_and_quiet_reopen() {
    use crate::icloud::photos::PhotosSession;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Clone)]
    struct CountedBridge {
        inner: ConfigBridgeSession,
        queries: Arc<AtomicUsize>,
    }
    #[async_trait::async_trait]
    impl PhotosSession for CountedBridge {
        async fn post(
            &self,
            url: &str,
            body: String,
            headers: &[(&str, &str)],
        ) -> anyhow::Result<serde_json::Value> {
            if url.contains("/records/query?") {
                self.queries.fetch_add(1, Ordering::SeqCst);
            }
            self.inner.post(url, body, headers).await
        }
        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(self.clone())
        }
    }

    for drift in [false, true] {
        let config = make_run_cycle_config();
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("state.db");
        let media = directory.path().join("media");
        std::fs::create_dir(&media).unwrap();
        let kept = media.join("previous.jpg");
        std::fs::write(
            &kept,
            include_bytes!("../../../../tests/data/media/pattern.jpg"),
        )
        .unwrap();
        let previous_bytes = std::fs::read(&kept).unwrap();
        {
            let db = crate::sync_loop::test_support::open_owned_cycle_db(&database).await;
            let record = crate::test_helpers::TestAssetRecord::new("asset-PREVIOUS")
                .filename("previous.jpg")
                .size(previous_bytes.len() as u64)
                .build();
            let checksum = download::file::compute_sha256(&kept).await.unwrap();
            db.upsert_seen(&record).await.unwrap();
            db.upsert_asset_master_mapping("PrimarySync", "asset-PREVIOUS", "PREVIOUS")
                .await
                .unwrap();
            db.mark_downloaded(
                "PrimarySync",
                "asset-PREVIOUS",
                "original",
                &kept,
                &checksum,
                Some(&checksum),
            )
            .await
            .unwrap();
            let initial_hash = if drift {
                "old-enumeration-generation".to_string()
            } else {
                download::compute_config_hash(&config)
            };
            db.set_metadata(ENUM_CONFIG_HASH_KEY, &initial_hash)
                .await
                .unwrap();
            db.set_metadata("sync_token:PrimarySync", "preserved-prior")
                .await
                .unwrap();
            db.set_metadata(
                "recent_selection_recovery:PrimarySync",
                "unparsed completed receipt",
            )
            .await
            .unwrap();
            db.set_metadata("inventory_reconciliation_required:PrimarySync", "1")
                .await
                .unwrap();
        }
        let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
        for cycle in 0..3 {
            let db = crate::sync_loop::test_support::open_owned_cycle_db(&database).await;
            if cycle == 0 {
                db.acquire_lock("recent bridge cursor commit fault")
                    .unwrap()
                    .execute_batch(
                        "CREATE TRIGGER fail_recent_cursor BEFORE UPDATE ON metadata
                 WHEN NEW.key='sync_token:PrimarySync' AND NEW.value='bridge-source'
                 BEGIN SELECT RAISE(ABORT,'synthetic checkpoint commit fault'); END;",
                    )
                    .unwrap();
            }
            let queries = Arc::new(AtomicUsize::new(0));
            let album = crate::sync_loop::test_support::make_owned_cycle_album(
                &db,
                "PrimarySync",
                "private",
                Box::new(CountedBridge {
                    inner: ConfigBridgeSession::new(
                        "PrimarySync",
                        "inventory-rank",
                        "bridge-source",
                    ),
                    queries: queries.clone(),
                }),
            );
            let library = make_run_cycle_library_state_with_album(
                "PrimarySync",
                "sync_token:PrimarySync",
                album,
            );
            let builder = make_run_cycle_download_config_builder_with_options(
                &media,
                db.clone(),
                RunCycleDownloadConfigOptions {
                    ..RunCycleDownloadConfigOptions::default()
                },
            );
            let result = run_cycle(
                &[&library],
                &config,
                Some(db.as_ref()),
                &builder,
                download::DownloadControls::download_hidden(),
                &shared_session,
                &CancellationToken::new(),
            )
            .await;
            if cycle == 0 {
                assert!(
                    queries.load(Ordering::SeqCst) > 0,
                    "fault case must reach current inventory: {result:#?}"
                );
                assert!(
                    result.is_err()
                        || result
                            .as_ref()
                            .is_ok_and(|result| !result.db_sync_token_advance_safe),
                    "injected commit fault must be reached: {result:#?}"
                );
                assert_eq!(
                    db.get_metadata("sync_token:PrimarySync")
                        .await
                        .unwrap()
                        .as_deref(),
                    Some("preserved-prior")
                );
                db.acquire_lock("remove synthetic checkpoint fault")
                    .unwrap()
                    .execute_batch("DROP TRIGGER fail_recent_cursor")
                    .unwrap();
                let raw = db
                    .get_metadata("recent_selection_recovery:PrimarySync")
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(raw, "unparsed completed receipt");
                assert_eq!(
                    db.get_metadata("inventory_reconciliation_required:PrimarySync")
                        .await
                        .unwrap()
                        .is_some(),
                    drift,
                    "a staged configuration must activate before consuming its inventory request"
                );
            } else {
                let result = result.unwrap();
                assert_eq!(result.failed_count, 0, "cycle {cycle}: {result:?}");
                assert_eq!(
                    db.get_metadata("sync_token:PrimarySync")
                        .await
                        .unwrap()
                        .as_deref(),
                    Some("bridge-source")
                );
                assert_eq!(
                    db.get_metadata(ENUM_CONFIG_HASH_KEY)
                        .await
                        .unwrap()
                        .as_deref(),
                    Some(download::compute_config_hash(&config).as_str())
                );
                if cycle == 1 {
                    assert_eq!(
                        queries.load(Ordering::SeqCst) > 0,
                        drift,
                        "only unactivated configuration repeats its guarded inventory"
                    );
                }
                if cycle == 2 {
                    assert_eq!(
                        queries.load(Ordering::SeqCst),
                        0,
                        "qualified quiet reopened bridge must not query inventory again"
                    );
                }
                assert_eq!(result.stats.downloaded, 0);
            }
            assert_eq!(std::fs::read(&kept).unwrap(), previous_bytes);
            assert_eq!(db.get_downloaded_page(0, 10).await.unwrap().len(), 1);
        }
    }
}

#[tokio::test]
async fn retained_checkpoint_candidate_changed_at_promotion_preserves_concurrent_state() {
    let config = make_run_cycle_config();
    let (_database_dir, inner) = fixture_db().await;
    inner
        .set_metadata(ENUM_CONFIG_HASH_KEY, "old-enum-hash")
        .await
        .unwrap();
    inner
        .set_metadata("sync_token:PrimarySync", "prior-token")
        .await
        .unwrap();
    let candidate_key =
        pending_zone_token_key(&download::compute_config_hash(&config), "PrimarySync");
    let wrapper =
        FailingMetadataSetDb::without_set_failure(inner.clone(), "candidate promotion mutation")
            .with_completed_candidate_mutation(
                candidate_key.clone(),
                "independent-candidate".into(),
            );
    let db: Arc<dyn download::DownloadStore> = Arc::new(wrapper);
    let dir = tempfile::tempdir().unwrap();
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let library = make_run_cycle_library_state_with_album(
        "PrimarySync",
        "sync_token:PrimarySync",
        fixture_album(
            &inner,
            "PrimarySync",
            "",
            Box::new(ConfigBridgeSession::new(
                "PrimarySync",
                "inventory-token",
                "bridge-token",
            )),
        ),
    );
    // Only the cycle's candidate read is intercepted. Selection and execution
    // retain the exact native database captured by the authenticated album.
    let builder = make_run_cycle_download_config_builder(dir.path(), inner.clone());
    let result = run_cycle(
        &[&library],
        &config,
        Some(db.as_ref()),
        &builder,
        download::DownloadControls::download_hidden(),
        &shared,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(!result.db_sync_token_advance_safe);
    assert_eq!(
        inner
            .get_metadata("sync_token:PrimarySync")
            .await
            .unwrap()
            .as_deref(),
        Some("prior-token")
    );
    assert_eq!(
        inner
            .get_metadata(ENUM_CONFIG_HASH_KEY)
            .await
            .unwrap()
            .as_deref(),
        Some("old-enum-hash")
    );
    assert_eq!(
        inner.get_metadata(&candidate_key).await.unwrap().as_deref(),
        Some("independent-candidate")
    );
    assert_eq!(
        inner
            .get_metadata(PENDING_ENUM_CONFIG_HASH_KEY)
            .await
            .unwrap()
            .as_deref(),
        Some(download::compute_config_hash(&config).as_str())
    );
}

#[cfg(target_os = "linux")]
mod late_io;

#[tokio::test]
async fn issue956_stale_mapping_confirms_current_pair_then_commits_and_reopens_quietly() {
    Box::pin(assert_issue956_pair_confirmation_reopens_quietly(None)).await;
}

#[tokio::test]
async fn issue956_missing_old_master_confirms_current_pair_then_commits_and_reopens_quietly() {
    for code in ["NOT_FOUND", "UNKNOWN_ITEM"] {
        Box::pin(assert_issue956_pair_confirmation_reopens_quietly(Some(
            code,
        )))
        .await;
    }
}

async fn assert_issue956_pair_confirmation_reopens_quietly(old_master_error: Option<&'static str>) {
    use crate::icloud::photos::{PhotoAsset, PhotosSession};
    use serde_json::{Value, json};
    use sha2::Digest;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Clone)]
    struct CurrentPair {
        records: Vec<Value>,
        lookups: Arc<AtomicUsize>,
        requested: Arc<Mutex<Vec<String>>>,
        queries: Arc<AtomicUsize>,
        old_master_error: Option<&'static str>,
    }
    #[async_trait::async_trait]
    impl PhotosSession for CurrentPair {
        async fn post(&self, url: &str, body: String, _: &[(&str, &str)]) -> anyhow::Result<Value> {
            let request: Value = serde_json::from_str(&body)?;
            if url.contains("/records/lookup?") {
                self.lookups.fetch_add(1, Ordering::SeqCst);
                let wanted = request["records"].as_array().unwrap();
                self.requested.lock().unwrap().extend(
                    wanted
                        .iter()
                        .map(|item| item["recordName"].as_str().unwrap().to_owned()),
                );
                let mut records: Vec<_> = self
                    .records
                    .iter()
                    .filter(|record| {
                        wanted
                            .iter()
                            .any(|item| item["recordName"] == record["recordName"])
                    })
                    .cloned()
                    .collect();
                if let Some(code) = self.old_master_error
                    && wanted.iter().any(|item| item["recordName"] == "PREVIOUS")
                {
                    records.push(json!({"recordName":"PREVIOUS", "serverErrorCode":code}));
                }
                return Ok(json!({"records":records}));
            }
            if url.contains("/changes/zone?") {
                let mut child = self.records[1].clone();
                child["fields"].as_object_mut().unwrap().remove("masterRef");
                let records = if request["zones"][0]["syncToken"] == "prior-token" {
                    vec![child]
                } else {
                    Vec::new()
                };
                return Ok(
                    json!({"zones":[{"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"},
                    "syncToken":"current-token","moreComing":false,"records":records}]}),
                );
            }
            if url.contains("/internal/records/query/batch") {
                return Ok(album_count_response(1));
            }
            assert!(url.contains("/records/query?"));
            self.queries.fetch_add(1, Ordering::SeqCst);
            let offset = request["query"]["filterBy"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|filter| filter["fieldName"] == "startRank")
                .and_then(|filter| filter["fieldValue"]["value"].as_u64())
                .unwrap_or(0);
            Ok(
                json!({"records": if offset == 0 { self.records.clone() } else { Vec::new() }, "syncToken":"inventory-token"}),
            )
        }
        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(self.clone())
        }
    }

    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("state.db");
    let media = directory.path().join("media");
    let kept = media.join("2023/11/14/photo.JPG");
    std::fs::create_dir_all(kept.parent().unwrap()).unwrap();
    let original = vec![0u8; 1024];
    let provider_checksum = data_encoding::BASE64.encode(&sha2::Sha256::digest(&original));
    std::fs::write(&kept, &original).unwrap();
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(original.clone()))
        .expect(0)
        .mount(&server)
        .await;
    let download_url = format!("{}/unexpected-download", server.uri());
    let mut records = full_album_page_with_download(
        "PrimarySync",
        "CURRENT",
        "inventory-token",
        &download_url,
        original.len() as u64,
        &provider_checksum,
    )["records"]
        .as_array()
        .unwrap()
        .clone();
    records[0]["fields"]["filenameEnc"]["value"] =
        json!(data_encoding::BASE64.encode(b"photo.JPG"));
    records[1]["fields"]["masterRef"]["value"]["zoneID"]["ownerRecordName"] =
        json!("_defaultOwner");
    let photo = PhotoAsset::new(records[0].clone(), records[1].clone());
    let config = make_run_cycle_config();
    let hash = download::compute_config_hash(&config);
    let marker = state::unresolved_identity_key("PrimarySync");
    {
        let db = open_fixture_db(&database).await;
        let record = crate::test_helpers::TestAssetRecord::new("asset-CURRENT")
            .filename("photo.JPG")
            .size(1024)
            .checksum(&provider_checksum)
            .created_at(photo.created())
            .added_at(photo.added_date())
            .metadata(photo.metadata().clone())
            .build();
        db.upsert_seen(&record).await.unwrap();
        let checksum = download::file::compute_sha256(&kept).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "asset-CURRENT",
            "original",
            &kept,
            &checksum,
            Some(&checksum),
        )
        .await
        .unwrap();
        db.upsert_asset_master_mapping("PrimarySync", "asset-CURRENT", "CURRENT")
            .await
            .unwrap();
        db.set_metadata(ENUM_CONFIG_HASH_KEY, &hash).await.unwrap();
        db.set_metadata("sync_token:PrimarySync", "current-token")
            .await
            .unwrap();
    }
    let lookups = Arc::new(AtomicUsize::new(0));
    let queries = Arc::new(AtomicUsize::new(0));
    let provider = CurrentPair {
        records,
        lookups: lookups.clone(),
        requested: Arc::new(Mutex::new(Vec::new())),
        queries: queries.clone(),
        old_master_error,
    };
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
    // A cursor and catalog row are not current-selection coverage. Establish
    // the native inventory anchor through a real cycle before making the
    // durable mapping stale and exercising sparse delta identity recovery.
    {
        let db = open_fixture_db(&database).await;
        let library = make_run_cycle_library_state_with_album(
            "PrimarySync",
            "sync_token:PrimarySync",
            fixture_album(&db, "PrimarySync", "", Box::new(provider.clone())),
        );
        let builder = make_run_cycle_download_config_builder(&media, db.clone());
        let result = run_cycle(
            &[&library],
            &config,
            Some(db.as_ref()),
            &builder,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(result.can_advance_database_checkpoint(), "{result:?}");
        assert_eq!(result.stats.downloaded, 0);
        assert!(queries.load(Ordering::SeqCst) > 0);
        assert_eq!(std::fs::read(&kept).unwrap(), original);
        db.upsert_asset_master_mapping("PrimarySync", "asset-CURRENT", "PREVIOUS")
            .await
            .unwrap();
        db.set_metadata("sync_token:PrimarySync", "prior-token")
            .await
            .unwrap();
        db.set_metadata(&marker, "1").await.unwrap();
    }
    lookups.store(0, Ordering::SeqCst);
    queries.store(0, Ordering::SeqCst);
    provider.requested.lock().unwrap().clear();
    let mut recovered_lookups = None;
    let mut recovered_queries = None;
    for cycle in 0..2 {
        let db = open_fixture_db(&database).await;
        let library = make_run_cycle_library_state_with_album(
            "PrimarySync",
            "sync_token:PrimarySync",
            fixture_album(&db, "PrimarySync", "", Box::new(provider.clone())),
        );
        let builder = make_run_cycle_download_config_builder(&media, db.clone());
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            run_cycle(
                &[&library],
                &config,
                Some(db.as_ref()),
                &builder,
                download::DownloadControls::download_hidden(),
                &shared_session,
                &CancellationToken::new(),
            ),
        )
        .await
        .expect("synthetic cycle must finish within its bound")
        .unwrap();
        assert!(
            outcome.can_advance_database_checkpoint(),
            "cycle {cycle}, lookups={}, queries={}: {outcome:?}",
            lookups.load(Ordering::SeqCst),
            queries.load(Ordering::SeqCst)
        );
        assert_eq!(outcome.stats.downloaded, 0, "existing media must be reused");
        if cycle == 0 {
            let requested = provider.requested.lock().unwrap();
            assert!(
                requested.iter().any(|name| name == "PREVIOUS"),
                "stale mapping must exercise the old-master lookup: {requested:?}"
            );
            assert!(
                requested.iter().any(|name| name == "CURRENT"),
                "recovery must confirm the current master: {requested:?}"
            );
            recovered_lookups = Some(lookups.load(Ordering::SeqCst));
            assert!(
                queries.load(Ordering::SeqCst) > 0,
                "stale identity recovery must refresh current selection coverage"
            );
            recovered_queries = Some(queries.load(Ordering::SeqCst));
        } else {
            assert_eq!(
                Some(lookups.load(Ordering::SeqCst)),
                recovered_lookups,
                "quiet reopen must not repeat identity work"
            );
            assert_eq!(
                Some(queries.load(Ordering::SeqCst)),
                recovered_queries,
                "quiet reopen must reuse recovered current-selection coverage"
            );
        }
        assert_eq!(
            db.get_master_record_name_for_asset("PrimarySync", "asset-CURRENT")
                .await
                .unwrap()
                .as_deref(),
            Some("CURRENT")
        );
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("current-token")
        );
        assert_eq!(
            db.get_metadata(ENUM_CONFIG_HASH_KEY)
                .await
                .unwrap()
                .as_deref(),
            Some(hash.as_str())
        );
        assert_eq!(
            db.get_metadata(PENDING_ENUM_CONFIG_HASH_KEY).await.unwrap(),
            None
        );
        assert_eq!(db.get_metadata(&marker).await.unwrap(), None);
        assert_eq!(std::fs::read(&kept).unwrap(), original);
    }
}
