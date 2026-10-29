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
    full_album_page_with_download, make_empty_full_album, make_full_album_with_boxed_session,
    make_incremental_album, make_named_empty_full_album,
    make_recording_run_cycle_download_config_builder, make_run_cycle_config,
    make_run_cycle_download_config_builder, make_run_cycle_download_config_builder_with_options,
    make_run_cycle_library_state_with_album, make_run_cycle_library_state_with_passes,
    make_shared_session_for_run_cycle, make_state_db,
};
use crate::{config, download, state};

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
    omit_source_completion: bool,
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
            omit_source_completion: false,
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
                    serde_json::json!({"zones":[{"zoneID":{"zoneName":self.zone.as_ref()},
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
                        // A real sparse source supplies durable retry identity
                        // before either identity or paired-master lookup waits.
                        let mut asset = crate::test_helpers::sparse_shared_asset_record();
                        asset["recordName"] = serde_json::json!("WAIT_CHILD");
                        if self.wait == BridgeWait::PairedAsset {
                            asset["fields"]["masterRef"] =
                                serde_json::json!({"value": {"recordName": "WAIT_MASTER"}});
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
            self.calls.lock().unwrap().push(("lookup", body.clone()));
            if self.wait != BridgeWait::AlbumRelation
                && self.gate_active.load(std::sync::atomic::Ordering::SeqCst)
                && let Some((requested, release)) = &self.bridge_gate
            {
                requested.notify_one();
                release.notified().await;
            }
            if !self.gate_active.load(std::sync::atomic::Ordering::SeqCst)
                && !self.omit_source_completion
                && matches!(
                    self.wait,
                    BridgeWait::AssetIdentity | BridgeWait::PairedAsset
                )
            {
                let request: serde_json::Value = serde_json::from_str(&body)?;
                let records: Vec<_> = request["records"].as_array().into_iter().flatten()
                    .filter(|record| record["recordName"] == "WAIT_CHILD")
                    .map(|_| serde_json::json!({"recordName":"WAIT_CHILD","serverErrorCode":"UNKNOWN_ITEM"}))
                    .collect();
                return Ok(serde_json::json!({"records":records}));
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
        false,
        false,
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
    Box::pin(cancelled_bridge_fixture_with_pages(
        wait, streaming, 0, false, false,
    ))
    .await;
}

#[tokio::test]
async fn cancellation_replay_with_unattributed_current_marker_remains_held_across_reopens() {
    Box::pin(cancelled_bridge_fixture_with_pages(
        BridgeWait::Replay,
        false,
        0,
        true,
        false,
    ))
    .await;
}

#[tokio::test]
async fn cancellation_replay_with_missing_source_proof_retains_retry_across_reopens() {
    Box::pin(cancelled_bridge_fixture_with_pages(
        BridgeWait::AssetIdentity,
        false,
        0,
        false,
        true,
    ))
    .await;
}

async fn cancelled_bridge_fixture_with_pages(
    wait: BridgeWait,
    streaming: bool,
    hydration_pages: usize,
    unattributed_current: bool,
    omit_source_completion: bool,
) {
    use crate::state::SparseIdentityStore;
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
    let db = Arc::new(state::SqliteStateDb::open(&database).await.unwrap());
    let marker = state::unresolved_identity_key("PrimarySync");
    let historical_marker = format!("{}PrimarySync", state::HISTORICAL_IDENTITY_PREFIX);
    db.set_metadata(&historical_marker, "1").await.unwrap();
    db.set_metadata(ENUM_CONFIG_HASH_KEY, "old-enum-hash")
        .await
        .unwrap();
    db.set_metadata("sync_token:v27:PrimarySync", "prior-token")
        .await
        .unwrap();
    if unattributed_current {
        // An unattributed current marker is real debt. Empty replay cannot
        // invent source identity or completion evidence to clear it.
        db.set_metadata(&marker, "1").await.unwrap();
    }
    let requested = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let mut session = ConfigBridgeSession::new("PrimarySync", "inventory-token", "bridge-token");
    session.bridge_gate = Some((requested.clone(), release.clone()));
    session.wait = wait;
    session.hydration_pages = hydration_pages;
    session.omit_source_completion = omit_source_completion;
    let gate_active = session.gate_active.clone();
    let calls = session.calls.clone();
    let album = crate::icloud::photos::PhotoAlbum::new(
        crate::icloud::photos::PhotoAlbumConfig {
            params: Arc::new(std::collections::HashMap::new()),
            service_endpoint: Arc::from("https://example.invalid"),
            name: Arc::from("Synthetic album"),
            list_type: Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
            obj_type: Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
            query_filter: None,
            page_size: 100,
            zone_id: Arc::new(serde_json::json!({"zoneName":"PrimarySync"})),
            retry_config: crate::retry::RetryConfig::default(),
            container_id: (!streaming).then(|| Arc::from("synthetic-album")),
            cross_zone_sources: Vec::new(),
        },
        Box::new(session),
    );
    let library = make_run_cycle_library_state_with_passes(
        "PrimarySync",
        "sync_token:v27:PrimarySync",
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
        false,
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
    drop(db);
    let db = Arc::new(state::SqliteStateDb::open(&database).await.unwrap());
    for (key, expected) in [
        ("sync_token:v27:PrimarySync", "prior-token"),
        (ENUM_CONFIG_HASH_KEY, "old-enum-hash"),
        (historical_marker.as_str(), "1"),
    ] {
        assert_eq!(
            db.get_metadata(key).await.unwrap().as_deref(),
            Some(expected)
        );
    }
    let observed_sparse = matches!(wait, BridgeWait::AssetIdentity | BridgeWait::PairedAsset);
    assert_eq!(
        db.get_metadata(&marker).await.unwrap().as_deref(),
        (unattributed_current || observed_sparse).then_some("1")
    );
    let sparse = db.sparse_identities("PrimarySync").await.unwrap();
    assert_eq!(sparse.len(), usize::from(observed_sparse));
    if observed_sparse {
        assert_eq!(sparse[0].source.as_str(), "WAIT_CHILD");
    }
    let mut previous_sparse_generation = sparse.first().map(|row| row.generation);
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
    let mut prior_call_count = initial_calls.len();
    let mut prior_query_count = inventory_queries;
    for replay in 0..2 {
        let db = Arc::new(state::SqliteStateDb::open(&database).await.unwrap());
        let builder = make_run_cycle_download_config_builder(&media, db.clone());
        // Reuse the exact Album/container route, releasing its synthetic gate.
        release.notify_one();
        let result = tokio::time::timeout(
            CLEANUP_BOUND,
            run_cycle(
                &[&library],
                &config,
                Some(db.as_ref()),
                false,
                &builder,
                download::DownloadControls::download_hidden(),
                &shared_session,
                &CancellationToken::new(),
            ),
        )
        .await
        .expect("valid synthetic replay is bounded")
        .unwrap();
        if unattributed_current || omit_source_completion {
            assert!(
                !result.can_advance_database_checkpoint(),
                "replay {replay}: {result:?}"
            );
            assert!(!result.db_sync_token_advance_safe);
            assert!(!result.stats.interrupted);
            assert_eq!(result.stats.downloaded, 0);
            assert_eq!(
                db.get_metadata("sync_token:v27:PrimarySync")
                    .await
                    .unwrap()
                    .as_deref(),
                Some("prior-token")
            );
            assert_eq!(
                db.get_metadata(ENUM_CONFIG_HASH_KEY)
                    .await
                    .unwrap()
                    .as_deref(),
                Some("old-enum-hash")
            );
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
                db.get_metadata(&marker).await.unwrap().as_deref(),
                Some("1")
            );
            assert_eq!(
                db.get_metadata(&historical_marker)
                    .await
                    .unwrap()
                    .as_deref(),
                Some("1")
            );
            let retained = db.sparse_identities("PrimarySync").await.unwrap();
            assert_eq!(retained.len(), sparse.len());
            if observed_sparse {
                assert_eq!(retained[0].source, sparse[0].source);
                let previous = previous_sparse_generation.unwrap();
                assert!(
                    retained[0].generation > previous,
                    "retry must retain a fresh generation fence"
                );
                previous_sparse_generation = Some(retained[0].generation);
                assert_eq!(retained[0].original, sparse[0].original);
                assert_eq!(retained[0].observed, sparse[0].observed);
            }
            let observed = calls.lock().unwrap().clone();
            let cursors: Vec<_> = observed
                .iter()
                .filter(|(kind, cursor)| {
                    *kind == "changes"
                        && !cursor.is_empty()
                        && !cursor.starts_with("synthetic-hydration-")
                })
                .map(|(_, cursor)| cursor.as_str())
                .collect();
            assert_eq!(cursors, vec!["prior-token"; replay + 2]);
            let queries = observed.iter().filter(|(kind, _)| *kind == "query").count();
            assert!(
                queries > prior_query_count,
                "each held replay must reconcile inventory"
            );
            prior_query_count = queries;
            if observed_sparse {
                let retried = observed.iter().skip(prior_call_count).any(|(kind, body)| {
                    *kind == "lookup"
                        && serde_json::from_str::<serde_json::Value>(body)
                            .ok()
                            .is_some_and(|request| {
                                request["records"].as_array().is_some_and(|records| {
                                    records
                                        .iter()
                                        .any(|record| record["recordName"] == "WAIT_CHILD")
                                })
                            })
                });
                assert!(
                    retried,
                    "retained source must be queried, not inferred from an empty replay"
                );
            }
            prior_call_count = observed.len();
            assert_eq!(
                std::fs::read(&sentinel).unwrap(),
                b"existing synthetic media"
            );
            continue;
        }
        assert!(
            result.can_advance_database_checkpoint(),
            "{wait:?} streaming={streaming} replay={replay}: {result:?}"
        );
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
            db.get_metadata("sync_token:v27:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("bridge-token")
        );
        assert_eq!(db.get_metadata(&marker).await.unwrap(), None);
        assert_eq!(
            db.get_metadata(&historical_marker)
                .await
                .unwrap()
                .as_deref(),
            Some("1")
        );
        assert!(
            db.sparse_identities("PrimarySync")
                .await
                .unwrap()
                .is_empty()
        );
        let observed = calls.lock().unwrap().clone();
        if observed_sparse {
            assert!(
                observed
                    .iter()
                    .skip(initial_calls.len())
                    .any(|(kind, body)| {
                        *kind == "lookup"
                            && serde_json::from_str::<serde_json::Value>(body)
                                .ok()
                                .is_some_and(|request| {
                                    request["records"].as_array().is_some_and(|records| {
                                        records
                                            .iter()
                                            .any(|record| record["recordName"] == "WAIT_CHILD")
                                    })
                                })
                    }),
                "completion must query the retained source"
            );
        }
        assert_eq!(
            observed
                .iter()
                .filter(|(kind, _)| *kind == "lookup")
                .count(),
            initial_lookups + usize::from(observed_sparse)
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

    let inner = make_state_db();
    inner
        .set_metadata(ENUM_CONFIG_HASH_KEY, "old-hash")
        .await
        .expect("seed enum hash");
    inner
        .set_metadata("sync_token:PrimarySync", "zone-tok-prev")
        .await
        .expect("seed zone token");
    let db: Arc<dyn download::DownloadStore> = Arc::new(FailingMetadataSetDb::new(
        Arc::clone(&inner),
        MetadataSetFailure::Exact(PENDING_ENUM_CONFIG_HASH_KEY),
        "simulated pending hash write failure",
    ));
    let download_dir = tempfile::tempdir().expect("download tempdir");
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

    let lib_state = make_run_cycle_library_state_with_album(
        "PrimarySync",
        "sync_token:PrimarySync",
        make_empty_full_album("zone-tok-new"),
    );
    let states = vec![&lib_state];
    let observed_modes = Arc::new(std::sync::Mutex::new(Vec::<download::SyncMode>::new()));
    let build_download_config = make_recording_run_cycle_download_config_builder(
        download_dir.path(),
        Arc::clone(&db),
        Arc::clone(&observed_modes),
    );

    let result = run_cycle(
        &states,
        &config,
        Some(db.as_ref()),
        false,
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
    let db = make_state_db();
    db.set_metadata(ENUM_CONFIG_HASH_KEY, "old-enum-hash")
        .await
        .expect("seed active enum hash");
    db.set_metadata("sync_token:PrimarySync", "prior-token")
        .await
        .expect("seed prior token");
    let download_dir = tempfile::tempdir().expect("download tempdir");
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
    let album = make_full_album_with_boxed_session(
        "PrimarySync",
        Box::new(ConfigBridgeSession::new(
            "PrimarySync",
            "inventory-token",
            "bridge-token",
        )),
    );
    let lib_state =
        make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
    let build_download_config =
        make_run_cycle_download_config_builder(download_dir.path(), Arc::clone(&db));

    let result = run_cycle(
        &[&lib_state],
        &config,
        Some(db.as_ref()),
        false,
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
    let db = make_state_db();
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
        make_run_cycle_download_config_builder(download_dir.path(), Arc::clone(&db));

    // Actual cycle orchestration: failed inventory token repair, then complete
    // inventory with a failed bridge, then successful bridge after reopening.
    for (inventory, bridge, advance) in [
        ("", "PRIVATE_UNUSED_TOKEN", false),
        ("PRIVATE_INVENTORY_TOKEN", "", false),
        ("PRIVATE_INVENTORY_TOKEN", "PRIVATE_BRIDGE_TOKEN", true),
    ] {
        let album = make_full_album_with_boxed_session(
            "PrimarySync",
            Box::new(ConfigBridgeSession::new("PrimarySync", inventory, bridge)),
        );
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
            false,
            &build_download_config,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(result.db_sync_token_advance_safe, advance);
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
    assert_eq!(component(1, "delta_bridge")["token_present"], true);
    assert_eq!(component(1, "delta_bridge")["token_usable"], true);
    assert_eq!(
        component(1, "delta_bridge")["completed_delta_replay"],
        false
    );
    assert_eq!(component(1, "delta_bridge")["phase"], "full");
    assert_eq!(component(1, "combined")["sync_token_blocked"], true);
    assert_eq!(
        component(1, "combined")["sync_token_blocked_reason"],
        "inventory_delta_bridge_failed"
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
    let db = make_state_db();
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
        make_full_album_with_boxed_session(
            "PrimarySync",
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
        make_full_album_with_boxed_session(
            "SharedSync-TEST",
            Box::new(ConfigBridgeSession::new("SharedSync-TEST", "", "unused")),
        ),
    );
    let build_download_config =
        make_run_cycle_download_config_builder(download_dir.path(), Arc::clone(&db));

    let result = run_cycle(
        &[&primary, &shared],
        &config,
        Some(db.as_ref()),
        false,
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
        make_full_album_with_boxed_session(
            "PrimarySync",
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
        make_full_album_with_boxed_session(
            "SharedSync-TEST",
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
        false,
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
    use std::sync::atomic::{AtomicUsize, Ordering};
    #[derive(Clone, Debug)]
    struct ReconciliationSession {
        source_calls: Arc<AtomicUsize>,
        lookup_calls: Arc<AtomicUsize>,
    }
    #[async_trait::async_trait]
    impl crate::icloud::photos::PhotosSession for ReconciliationSession {
        async fn post(
            &self,
            url: &str,
            _body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<serde_json::Value> {
            if url.contains("/records/lookup?") {
                self.lookup_calls.fetch_add(1, Ordering::SeqCst);
            }
            self.source_calls.fetch_add(1, Ordering::SeqCst);
            if url.contains("/changes/zone?") {
                return Ok(
                    serde_json::json!({"zones": [{"zoneID": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"}, "syncToken": "zone-after", "moreComing": false, "records": []}]}),
                );
            }
            if url.contains("/internal/records/query/batch") {
                return Ok(album_count_response(0));
            }
            Ok(serde_json::json!({"records": [], "syncToken": "zone-after"}))
        }
        fn clone_box(&self) -> Box<dyn crate::icloud::photos::PhotosSession> {
            Box::new(self.clone())
        }
    }
    let config = make_run_cycle_config();
    let dir = tempfile::tempdir().unwrap();
    let old_root = dir.path().join("old");
    let new_root = dir.path().join("new");
    let planned_root = new_root.join("planned");
    let moved_root = new_root.join("moved");
    std::fs::create_dir_all(&old_root).unwrap();
    std::fs::create_dir_all(&planned_root).unwrap();
    let old_path = old_root.join("old.jpg");
    let external = dir.path().join("external.jpg");
    let unrelated = new_root.join("unrelated.xmp");
    std::fs::write(&unrelated, b"unrelated user metadata").unwrap();
    std::fs::write(&old_path, b"preserved historical file").unwrap();
    std::fs::write(&external, b"foreign external file").unwrap();
    let db_path = dir.path().join("state.db");
    let db = Arc::new(state::SqliteStateDb::open(&db_path).await.unwrap());
    let historical = crate::test_helpers::TestAssetRecord::new("HISTORICAL")
        .filename("old.jpg")
        .size(25)
        .checksum("historical-provider")
        .build();
    db.upsert_seen(&historical).await.unwrap();
    let historical_checksum = download::file::compute_sha256(&old_path).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "HISTORICAL",
        "original",
        &old_path,
        &historical_checksum,
        None,
    )
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
    db.set_metadata(download::DOWNLOAD_CONFIG_HASH_KEY, "prior-path-config")
        .await
        .unwrap();
    // A real planned and prepared layout is the authorization for recovery;
    // path configuration drift alone cannot create one.
    let op = download::prepared_layout_with_refresh(
        &db,
        &planned_root,
        "photo.jpg",
        "PrimarySync",
        false,
    )
    .await;
    let expected = op.members[0].destination.as_ref().unwrap().to_path();
    let stage = op.members[0].stage.as_ref().unwrap().to_path();
    let staged_bytes = std::fs::read(&stage).unwrap();
    match entry {
        ExternalReconciliationEntry::Symlink => {
            std::os::unix::fs::symlink(&external, &expected).unwrap()
        }
        ExternalReconciliationEntry::Hardlink => std::fs::hard_link(&old_path, &expected).unwrap(),
        ExternalReconciliationEntry::SameSizeReplacement
        | ExternalReconciliationEntry::MovedDirectory => {
            std::fs::write(&expected, vec![7u8; staged_bytes.len()]).unwrap()
        }
    }
    let source_calls = Arc::new(AtomicUsize::new(0));
    let lookup_calls = Arc::new(AtomicUsize::new(0));
    let album = make_full_album_with_boxed_session(
        "PrimarySync",
        Box::new(ReconciliationSession {
            source_calls: Arc::clone(&source_calls),
            lookup_calls: Arc::clone(&lookup_calls),
        }),
    );
    let lib =
        make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
    let (_session_dir, session) = make_shared_session_for_run_cycle().await;
    drop(db);
    let mut completed_binding = None;
    let mut completed_mtime = None;
    for cycle in 0..4 {
        let db = Arc::new(state::SqliteStateDb::open(&db_path).await.unwrap());
        let builder = make_run_cycle_download_config_builder(&new_root, db.clone());
        let expected_hash = download::hash_download_config(&builder(
            download::SyncMode::Full,
            Arc::new(rustc_hash::FxHashSet::default()),
            Arc::new(download::AssetGroupings::default()),
            Arc::from("PrimarySync"),
        ));
        let lookups_before = lookup_calls.load(Ordering::SeqCst);
        let result = run_cycle(
            &[&lib],
            &config,
            Some(db.as_ref()),
            false,
            &builder,
            download::DownloadControls::download_hidden(),
            &session,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        if cycle < 2 {
            assert!(
                result.failed_count > 0,
                "{entry:?} cycle {cycle}: {result:?}"
            );
            assert!(!result.db_sync_token_advance_safe);
            assert_eq!(source_calls.load(Ordering::SeqCst), 0);
            assert_eq!(
                db.get_metadata("sync_token:PrimarySync")
                    .await
                    .unwrap()
                    .as_deref(),
                Some("zone-before")
            );
            let pending = db
                .primary_layout_operations("PrimarySync".into())
                .await
                .unwrap();
            assert_eq!(pending.len(), 1);
            assert_eq!(pending[0].operation, op.operation);
            assert_eq!(pending[0].source_library, "PrimarySync");
            assert_eq!(
                pending[0].members[0]
                    .destination
                    .as_ref()
                    .unwrap()
                    .to_path(),
                expected
            );
            assert_eq!(
                db.get_metadata(PENDING_DOWNLOAD_CONFIG_HASH_KEY)
                    .await
                    .unwrap()
                    .as_deref(),
                Some(expected_hash.as_str())
            );
            assert_eq!(
                db.get_metadata(download::DOWNLOAD_CONFIG_HASH_KEY)
                    .await
                    .unwrap()
                    .as_deref(),
                Some("prior-path-config")
            );
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
                }
                ExternalReconciliationEntry::SameSizeReplacement
                | ExternalReconciliationEntry::MovedDirectory => assert_eq!(
                    std::fs::read(&expected).unwrap(),
                    vec![7u8; staged_bytes.len()]
                ),
            }
            let retained_stage =
                if cycle == 1 && matches!(entry, ExternalReconciliationEntry::MovedDirectory) {
                    moved_root.join(stage.strip_prefix(&planned_root).unwrap())
                } else {
                    stage.clone()
                };
            assert_eq!(std::fs::read(retained_stage).unwrap(), staged_bytes);
        } else {
            assert_eq!(
                result.failed_count, 0,
                "{entry:?} cycle {cycle}: {result:?}"
            );
            assert!(result.db_sync_token_advance_safe);
            assert_eq!(
                db.get_metadata("sync_token:PrimarySync")
                    .await
                    .unwrap()
                    .as_deref(),
                Some("zone-after")
            );
            assert_eq!(
                db.get_metadata(download::DOWNLOAD_CONFIG_HASH_KEY)
                    .await
                    .unwrap()
                    .as_deref(),
                Some(expected_hash.as_str())
            );
            assert_eq!(
                db.get_metadata(PENDING_DOWNLOAD_CONFIG_HASH_KEY)
                    .await
                    .unwrap(),
                None
            );
            assert!(
                db.primary_layout_operations("PrimarySync".into())
                    .await
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(std::fs::read(&expected).unwrap(), staged_bytes);
            let binding = db
                .primary_layout_binding(op.family.clone())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(binding.family, op.family);
            assert_eq!(binding.source_library, op.source_library);
            assert_eq!(binding.child, op.child);
            assert_eq!(binding.generation, op.generation + 1);
            assert!(
                binding
                    .files
                    .iter()
                    .any(|file| file.path.to_path() == expected)
            );
            let evidence = serde_json::to_vec(&binding).unwrap();
            let mtime = std::fs::metadata(&expected).unwrap().modified().unwrap();
            if cycle == 3 {
                assert_eq!(result.stats.downloaded, 0);
                assert_eq!(lookup_calls.load(Ordering::SeqCst), lookups_before);
                assert_eq!(completed_binding.as_ref(), Some(&evidence));
                assert_eq!(completed_mtime, Some(mtime));
            } else {
                completed_binding = Some(evidence);
                completed_mtime = Some(mtime);
            }
        }
        let rows = db.get_downloaded_page(0, 10).await.unwrap();
        let historical_row = rows
            .iter()
            .find(|row| row.id.as_ref() == "HISTORICAL")
            .unwrap();
        assert_eq!(
            historical_row.local_path.as_deref(),
            Some(old_path.as_path())
        );
        assert_eq!(
            historical_row.local_checksum.as_deref(),
            Some(historical_checksum.as_str())
        );
        assert_eq!(
            std::fs::read(&old_path).unwrap(),
            b"preserved historical file"
        );
        assert_eq!(std::fs::read(&external).unwrap(), b"foreign external file");
        assert_eq!(
            std::fs::read(&unrelated).unwrap(),
            b"unrelated user metadata"
        );
        if cycle == 1 && matches!(entry, ExternalReconciliationEntry::MovedDirectory) {
            assert_eq!(
                std::fs::read(moved_root.join(expected.file_name().unwrap())).unwrap(),
                vec![7u8; staged_bytes.len()]
            );
        }
        drop(builder);
        drop(db);
        if cycle == 0 && matches!(entry, ExternalReconciliationEntry::MovedDirectory) {
            std::fs::rename(&planned_root, &moved_root).unwrap();
            std::fs::create_dir(&planned_root).unwrap();
            std::fs::write(&expected, vec![7u8; staged_bytes.len()]).unwrap();
        }
        if cycle == 1 {
            std::fs::remove_file(&expected).unwrap();
            if matches!(entry, ExternalReconciliationEntry::MovedDirectory) {
                std::fs::remove_dir(&planned_root).unwrap();
                std::fs::rename(&moved_root, &planned_root).unwrap();
                std::fs::remove_file(&expected).unwrap();
            }
        }
    }
}

#[tokio::test]
async fn root_drift_without_recorded_operation_does_not_backfill_historical_file() {
    #[derive(Clone, Debug)]
    struct HistoricalSession {
        records: serde_json::Value,
    }
    #[async_trait::async_trait]
    impl crate::icloud::photos::PhotosSession for HistoricalSession {
        async fn post(
            &self,
            url: &str,
            _body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<serde_json::Value> {
            if url.contains("/records/lookup?") {
                return Ok(self.records.clone());
            }
            if url.contains("/changes/zone?") {
                return Ok(
                    serde_json::json!({"zones": [{"zoneID": {"zoneName":"PrimarySync", "ownerRecordName":"_defaultOwner"}, "syncToken":"zone-after", "moreComing":false, "records":[]}]}),
                );
            }
            if url.contains("/internal/records/query/batch") {
                return Ok(album_count_response(0));
            }
            Ok(serde_json::json!({"records": [], "syncToken": "zone-after"}))
        }
        fn clone_box(&self) -> Box<dyn crate::icloud::photos::PhotosSession> {
            Box::new(self.clone())
        }
    }
    let config = make_run_cycle_config();
    let old = tempfile::tempdir().unwrap();
    let new = tempfile::tempdir().unwrap();
    let db_path = old.path().join("state.db");
    let source = old.path().join("old.jpg");
    let foreign = new
        .path()
        .join(crate::sync_loop::test_support::run_cycle_expected_date_dir())
        .join("old.jpg");
    std::fs::create_dir_all(foreign.parent().unwrap()).unwrap();
    std::fs::write(&source, b"historical media").unwrap();
    std::fs::write(&foreign, b"foreign occupant").unwrap();
    let db = Arc::new(state::SqliteStateDb::open(&db_path).await.unwrap());
    let record = crate::test_helpers::TestAssetRecord::new("HISTORICAL")
        .filename("old.jpg")
        .size(16)
        .checksum("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
        .build();
    db.upsert_seen(&record).await.unwrap();
    let checksum = download::file::compute_sha256(&source).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "HISTORICAL",
        "original",
        &source,
        &checksum,
        None,
    )
    .await
    .unwrap();
    db.upsert_asset_master_mapping("PrimarySync", "asset-HISTORICAL", "HISTORICAL")
        .await
        .unwrap();
    db.set_metadata(
        ENUM_CONFIG_HASH_KEY,
        &download::compute_config_hash(&config),
    )
    .await
    .unwrap();
    db.set_metadata(download::DOWNLOAD_CONFIG_HASH_KEY, "prior-path-config")
        .await
        .unwrap();
    db.set_metadata("sync_token:PrimarySync", "zone-before")
        .await
        .unwrap();
    drop(db);
    let lib = make_run_cycle_library_state_with_album(
        "PrimarySync",
        "sync_token:PrimarySync",
        make_full_album_with_boxed_session(
            "PrimarySync",
            Box::new(HistoricalSession {
                records: {
                    let mut records = full_album_page_with_download(
                        "PrimarySync",
                        "HISTORICAL",
                        "unused",
                        "https://p01.icloud-content.com/old.jpg",
                        16,
                        "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
                    );
                    records["records"][0]["fields"]["filenameEnc"]["value"] =
                        serde_json::json!("b2xkLmpwZw==");
                    records
                },
            }),
        ),
    );
    let (_session_dir, session) = make_shared_session_for_run_cycle().await;
    for _ in 0..2 {
        let db = Arc::new(state::SqliteStateDb::open(&db_path).await.unwrap());
        let builder = make_run_cycle_download_config_builder(new.path(), db.clone());
        let result = run_cycle(
            &[&lib],
            &config,
            Some(db.as_ref()),
            false,
            &builder,
            download::DownloadControls::download_hidden(),
            &session,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(result.failed_count, 0);
        assert!(result.db_sync_token_advance_safe, "{result:?}");
        assert_eq!(result.stats.downloaded, 0);
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("zone-after")
        );
        assert!(
            db.primary_layout_operations("PrimarySync".into())
                .await
                .unwrap()
                .is_empty()
        );
        let rows = db.get_downloaded_page(0, 10).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].local_path.as_deref(), Some(source.as_path()));
        assert_eq!(rows[0].local_checksum.as_deref(), Some(checksum.as_str()));
        assert_eq!(std::fs::read(&source).unwrap(), b"historical media");
        assert_eq!(std::fs::read(&foreign).unwrap(), b"foreign occupant");
        assert_eq!(
            std::fs::read_dir(foreign.parent().unwrap())
                .unwrap()
                .count(),
            1
        );
    }
}

#[tokio::test]
async fn run_cycle_download_config_hash_drift_keeps_source_incremental() {
    let config = make_run_cycle_config();
    let db = make_state_db();
    let old_download_dir = tempfile::tempdir().expect("old download tempdir");
    let new_download_dir = tempfile::tempdir().expect("new download tempdir");

    let old_build_download_config =
        make_run_cycle_download_config_builder(old_download_dir.path(), Arc::clone(&db));
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
        make_incremental_album("zone-tok-new"),
    );
    let states = vec![&lib_state];
    let observed_modes = Arc::new(std::sync::Mutex::new(Vec::<download::SyncMode>::new()));
    let build_download_config = make_recording_run_cycle_download_config_builder(
        new_download_dir.path(),
        Arc::clone(&db),
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
        false,
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
                return Ok(serde_json::json!({"records": self.records.as_ref()}));
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
    let album = make_full_album_with_boxed_session(
        "PrimarySync",
        Box::new(DateBoundSession {
            records: Arc::new(records),
        }),
    );
    let lib_state =
        make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
    let db = make_state_db();
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
        Arc::clone(&db),
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
        false,
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
        Arc::clone(&db),
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
        false,
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
        false,
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
    let db = make_state_db();
    let download_dir = tempfile::tempdir().expect("download tempdir");
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
    let passes = vec![
        crate::commands::AlbumPass {
            kind: crate::commands::PassKind::Album,
            album: make_named_empty_full_album("PrimarySync", "Vacation", "zone-tok"),
            exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
        },
        crate::commands::AlbumPass {
            kind: crate::commands::PassKind::Album,
            album: make_named_empty_full_album("PrimarySync", "Family", "zone-tok"),
            exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
        },
        crate::commands::AlbumPass {
            kind: crate::commands::PassKind::Unfiled,
            album: make_named_empty_full_album("PrimarySync", "", "zone-tok"),
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
        Arc::clone(&db),
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
        false,
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
    let db = make_state_db();
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
                album: make_named_empty_full_album("PrimarySync", "Vacation", "zone-tok-1"),
                exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
            },
            crate::commands::AlbumPass {
                kind: crate::commands::PassKind::Unfiled,
                album: make_named_empty_full_album("PrimarySync", "", "zone-tok-1"),
                exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
            },
        ],
    );
    let first_builder = make_run_cycle_download_config_builder_with_options(
        download_dir.path(),
        Arc::clone(&db),
        options,
    );
    let first_result = run_cycle(
        &[&first_state],
        &config,
        Some(db.as_ref()),
        false,
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
            album: make_empty_full_album("zone-tok-2"),
            exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
        }],
    );
    let observed_modes = Arc::new(std::sync::Mutex::new(Vec::<download::SyncMode>::new()));
    let base_builder = make_run_cycle_download_config_builder_with_options(
        download_dir.path(),
        Arc::clone(&db),
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
        false,
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
    use crate::state::{SparseAttemptOutcome, SparseEvidence, SparseSourceId};
    for (prior_token, sparse) in [None, Some("retained-before")]
        .into_iter()
        .flat_map(|token| [false, true].map(|sparse| (token, sparse)))
    {
        let inner = make_state_db();
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
        let album = make_full_album_with_boxed_session(
            "PrimarySync",
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
                false,
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
async fn recent_exact_eof_bridge_receipt_survives_failed_cursor_commit_and_quiet_reopen() {
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
        let mut config = make_run_cycle_config();
        config.filters.recent = Some(2);
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
            let db = state::SqliteStateDb::open(&database).await.unwrap();
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
        }
        let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
        for cycle in 0..3 {
            let db = Arc::new(state::SqliteStateDb::open(&database).await.unwrap());
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
            let album = make_full_album_with_boxed_session(
                "PrimarySync",
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
                    recent: Some(2),
                    ..RunCycleDownloadConfigOptions::default()
                },
            );
            let result = run_cycle(
                &[&library],
                &config,
                Some(db.as_ref()),
                false,
                &builder,
                download::DownloadControls::download_hidden(),
                &shared_session,
                &CancellationToken::new(),
            )
            .await;
            if cycle == 0 {
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
                assert_eq!(
                    serde_json::from_str::<serde_json::Value>(&raw).unwrap()["complete"],
                    true,
                    "an early completed receipt still cannot match the uncommitted prior cursor"
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
                if cycle == 1 && !drift {
                    assert!(
                        queries.load(Ordering::SeqCst) > 0,
                        "uncommitted receipt must retry inventory"
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
    let inner = make_state_db();
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
        make_full_album_with_boxed_session(
            "PrimarySync",
            Box::new(ConfigBridgeSession::new(
                "PrimarySync",
                "inventory-token",
                "bridge-token",
            )),
        ),
    );
    let builder = make_run_cycle_download_config_builder(dir.path(), db.clone());
    let result = run_cycle(
        &[&library],
        &config,
        Some(db.as_ref()),
        false,
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Clone)]
    struct CurrentPair {
        records: Vec<Value>,
        lookups: Arc<AtomicUsize>,
        queries: Arc<AtomicUsize>,
        old_master_error: Option<&'static str>,
    }
    #[async_trait::async_trait]
    impl PhotosSession for CurrentPair {
        async fn post(&self, url: &str, body: String, _: &[(&str, &str)]) -> anyhow::Result<Value> {
            let request: Value = serde_json::from_str(&body)?;
            if url.contains("/records/lookup?") {
                assert!(self.lookups.fetch_add(1, Ordering::SeqCst) < 2);
                let wanted = request["records"].as_array().unwrap();
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
            Ok(json!({"records":[],"syncToken":"inventory-token"}))
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
        "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
    )["records"]
        .as_array()
        .unwrap()
        .clone();
    records[0]["fields"]["filenameEnc"]["value"] = json!("photo.JPG");
    let photo = PhotoAsset::new(records[0].clone(), records[1].clone());
    let config = make_run_cycle_config();
    let hash = download::compute_config_hash(&config);
    let marker = state::unresolved_identity_key("PrimarySync");
    {
        let db = state::SqliteStateDb::open(&database).await.unwrap();
        let record = crate::test_helpers::TestAssetRecord::new("asset-CURRENT")
            .filename("photo.JPG")
            .size(1024)
            .checksum("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
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
        db.upsert_asset_master_mapping("PrimarySync", "asset-CURRENT", "PREVIOUS")
            .await
            .unwrap();
        db.set_metadata(ENUM_CONFIG_HASH_KEY, &hash).await.unwrap();
        db.set_metadata("sync_token:PrimarySync", "prior-token")
            .await
            .unwrap();
        db.set_metadata(&marker, "1").await.unwrap();
    }
    let lookups = Arc::new(AtomicUsize::new(0));
    let queries = Arc::new(AtomicUsize::new(0));
    let album = make_full_album_with_boxed_session(
        "PrimarySync",
        Box::new(CurrentPair {
            records,
            lookups: lookups.clone(),
            queries: queries.clone(),
            old_master_error,
        }),
    );
    let library =
        make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
    for cycle in 0..2 {
        let db = Arc::new(state::SqliteStateDb::open(&database).await.unwrap());
        let builder = make_run_cycle_download_config_builder(&media, db.clone());
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            run_cycle(
                &[&library],
                &config,
                Some(db.as_ref()),
                false,
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
        assert_eq!(
            lookups.load(Ordering::SeqCst),
            2,
            "quiet reopen must not repeat identity work"
        );
        assert_eq!(
            queries.load(Ordering::SeqCst),
            0,
            "recovery must not require inventory"
        );
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
