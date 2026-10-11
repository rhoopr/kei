//! Valid retained deltas composed with current Hidden refreshes retain source provenance.
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::commands::{AlbumPass, PassKind};
use crate::icloud::photos::{PhotoAlbum, PhotoAlbumConfig, PhotosSession};
use crate::sync_cycle::{
    ENUM_CONFIG_HASH_KEY, PENDING_ENUM_CONFIG_HASH_KEY, pending_zone_token_key, run_cycle,
};
use crate::sync_loop::test_support::{
    album_count_response, make_named_full_album_with_boxed_session, make_run_cycle_config,
    make_run_cycle_download_config_builder, make_run_cycle_library_state_with_passes,
    make_shared_session_for_run_cycle,
};
use crate::{download, state};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MixedFault {
    MissingSuccessor,
    FailedSmartRefresh,
    Cancellation,
    CheckpointWrite,
    StalePlan,
}

#[derive(Clone)]
struct MixedSession {
    smart: bool,
    fault: Option<MixedFault>,
    changes: Arc<AtomicUsize>,
    queries: Arc<AtomicUsize>,
    cancel: CancellationToken,
    expected_cursor: &'static str,
}

#[async_trait::async_trait]
impl PhotosSession for MixedSession {
    async fn post(
        &self,
        url: &str,
        body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        if url.contains("/internal/records/query/batch?") {
            return Ok(album_count_response(0));
        }
        if url.contains("/records/query?") {
            self.queries.fetch_add(1, Ordering::SeqCst);
            if self.smart && self.changes.load(Ordering::SeqCst) > 0 {
                if self.fault == Some(MixedFault::FailedSmartRefresh) {
                    return Ok(json!({"records": null, "syncToken": "smart-query-eof"}));
                }
                if self.fault == Some(MixedFault::Cancellation) {
                    self.cancel.cancel();
                }
            }
            // The unanimous inventory EOF cannot stand in for the delta successor.
            return Ok(json!({
                "records": [],
                "syncToken": "inventory-query-eof"
            }));
        }
        if url.contains("/changes/zone?") {
            assert!(!self.smart, "source delta must use the Unfiled pass");
            let request: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(request["zones"][0]["syncToken"], self.expected_cursor);
            self.changes.fetch_add(1, Ordering::SeqCst);
            let mut zone = json!({
                "zoneID": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"},
                "syncToken": "validated-delta-successor",
                "moreComing": false,
                "records": []
            });
            if self.fault == Some(MixedFault::MissingSuccessor) {
                zone.as_object_mut().unwrap().remove("syncToken");
            }
            return Ok(json!({"zones": [zone]}));
        }
        anyhow::bail!("unexpected mixed-delta fixture request")
    }

    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

fn smart_album(zone: &str, name: &str, session: Box<dyn PhotosSession>) -> PhotoAlbum {
    let definition = crate::icloud::photos::smart_folders::smart_folders()
        .into_iter()
        .find(|(candidate, _)| *candidate == name)
        .unwrap()
        .1;
    PhotoAlbum::new(
        PhotoAlbumConfig {
            params: Arc::new(std::collections::HashMap::new()),
            service_endpoint: Arc::from("https://example.com"),
            name: Arc::from(name),
            list_type: Arc::from(definition.list_type),
            obj_type: Arc::from(definition.obj_type),
            query_filter: definition.query_filter,
            page_size: 100,
            zone_id: Arc::new(json!({"zoneName": zone})),
            retry_config: crate::retry::RetryConfig::default(),
            container_id: None,
            cross_zone_sources: Vec::new(),
        },
        session,
    )
}

#[tokio::test]
async fn mixed_valid_delta_hidden_refresh_promotes_inventory_bridge_then_quiet_reopen() {
    Box::pin(mixed_lifecycle(true, None)).await;
}

#[tokio::test]
async fn mixed_valid_delta_hidden_refresh_clears_retained_identity_marker_without_extra_replay() {
    Box::pin(mixed_lifecycle(false, None)).await;
}

#[tokio::test]
async fn mixed_delta_hidden_refresh_preserves_vetoes_then_recovers_after_reopen() {
    for fault in [
        MixedFault::MissingSuccessor,
        MixedFault::FailedSmartRefresh,
        MixedFault::Cancellation,
        MixedFault::CheckpointWrite,
        MixedFault::StalePlan,
    ] {
        Box::pin(mixed_lifecycle(true, Some(fault))).await;
    }
}

async fn mixed_lifecycle(drift: bool, fault: Option<MixedFault>) {
    let root = tempfile::tempdir().unwrap();
    let database = root.path().join("state.db");
    let media = root.path().join("media");
    std::fs::create_dir(&media).unwrap();
    let kept = media.join("historical.jpg");
    let bytes = include_bytes!("../../../../tests/data/media/pattern.jpg");
    std::fs::write(&kept, bytes).unwrap();
    let modified = std::fs::metadata(&kept).unwrap().modified().unwrap();
    let config = make_run_cycle_config();
    let current_hash = download::compute_config_hash(&config);
    let initial_hash = if drift {
        "old-enum-generation"
    } else {
        &current_hash
    };
    let marker = state::unresolved_identity_key("PrimarySync");
    {
        let db = state::SqliteStateDb::open(&database).await.unwrap();
        let checksum = download::file::compute_sha256(&kept).await.unwrap();
        db.upsert_seen(
            &crate::test_helpers::TestAssetRecord::new("asset-HISTORY")
                .filename("historical.jpg")
                .size(bytes.len() as u64)
                .build(),
        )
        .await
        .unwrap();
        db.upsert_asset_master_mapping("PrimarySync", "asset-HISTORY", "HISTORY")
            .await
            .unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "asset-HISTORY",
            "original",
            &kept,
            &checksum,
            Some(&checksum),
        )
        .await
        .unwrap();
        db.set_metadata(ENUM_CONFIG_HASH_KEY, initial_hash)
            .await
            .unwrap();
        db.set_metadata("sync_token:PrimarySync", "retained-valid-cursor")
            .await
            .unwrap();
        db.set_metadata(&marker, "1").await.unwrap();
    }
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    // Every iteration releases all SQLite handles. A fault adds a retained-state
    // cycle before successful recovery and two unchanged steady-state cycles.
    let recovery_cycle = usize::from(fault.is_some());
    for cycle in 0..=recovery_cycle + 2 {
        let held = cycle < recovery_cycle;
        let resumes_candidate =
            fault == Some(MixedFault::CheckpointWrite) && cycle == recovery_cycle;
        let active_fault = if held { fault } else { None };
        let db = Arc::new(state::SqliteStateDb::open(&database).await.unwrap());
        if active_fault == Some(MixedFault::CheckpointWrite) {
            db.acquire_lock("mixed checkpoint fault")
                .unwrap()
                .execute_batch(
                    "CREATE TRIGGER mixed_checkpoint_fault BEFORE UPDATE ON metadata
                     WHEN NEW.key='sync_token:PrimarySync' AND NEW.value='validated-delta-successor'
                     BEGIN SELECT RAISE(ABORT,'synthetic mixed checkpoint failure'); END;",
                )
                .unwrap();
        }
        let changes = Arc::new(AtomicUsize::new(0));
        let unfiled_queries = Arc::new(AtomicUsize::new(0));
        let smart_queries = Arc::new(AtomicUsize::new(0));
        let cancel = CancellationToken::new();
        let unfiled = MixedSession {
            smart: false,
            fault: active_fault,
            changes: changes.clone(),
            queries: unfiled_queries.clone(),
            cancel: cancel.clone(),
            expected_cursor: if cycle <= recovery_cycle && !resumes_candidate {
                "retained-valid-cursor"
            } else {
                "validated-delta-successor"
            },
        };
        let smart = MixedSession {
            smart: true,
            queries: smart_queries.clone(),
            ..unfiled.clone()
        };
        let mut library = make_run_cycle_library_state_with_passes(
            "PrimarySync",
            "sync_token:PrimarySync",
            vec![
                AlbumPass {
                    kind: PassKind::Unfiled,
                    album: make_named_full_album_with_boxed_session(
                        "PrimarySync",
                        "",
                        Box::new(unfiled),
                    ),
                    exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
                },
                AlbumPass {
                    kind: PassKind::SmartFolder,
                    album: smart_album("PrimarySync", "Hidden", Box::new(smart)),
                    exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
                },
            ],
        );
        library.plan_is_stale = active_fault == Some(MixedFault::StalePlan);
        let builder = make_run_cycle_download_config_builder(&media, db.clone());
        let result = run_cycle(
            &[&library],
            &config,
            Some(db.as_ref()),
            false,
            &builder,
            download::DownloadControls::download_hidden(),
            &shared,
            &cancel,
        )
        .await;
        if held {
            assert!(
                result.is_err()
                    || result
                        .as_ref()
                        .is_ok_and(|result| !result.db_sync_token_advance_safe),
                "fault must hold checkpoint: {fault:?}: {result:?}"
            );
            assert_eq!(
                db.get_metadata("sync_token:PrimarySync")
                    .await
                    .unwrap()
                    .as_deref(),
                Some("retained-valid-cursor")
            );
            assert_eq!(
                db.get_metadata(ENUM_CONFIG_HASH_KEY)
                    .await
                    .unwrap()
                    .as_deref(),
                Some(initial_hash)
            );
            assert_eq!(
                db.get_metadata(PENDING_ENUM_CONFIG_HASH_KEY)
                    .await
                    .unwrap()
                    .as_deref(),
                Some(current_hash.as_str())
            );
            assert!(db.get_metadata(&marker).await.unwrap().is_some());
            if fault == Some(MixedFault::CheckpointWrite) {
                assert_eq!(
                    db.get_metadata(&pending_zone_token_key(&current_hash, "PrimarySync"))
                        .await
                        .unwrap()
                        .as_deref(),
                    Some("validated-delta-successor"),
                    "safe bridged candidate must survive failed activation"
                );
            }
        } else {
            let result = result.unwrap();
            assert_eq!(
                result.failed_count, 0,
                "{fault:?}, cycle {cycle}: {result:?}"
            );
            assert!(
                result.db_sync_token_advance_safe,
                "{fault:?}, cycle {cycle}: {result:?}"
            );
            assert_eq!(
                changes.load(Ordering::SeqCst),
                1,
                "exactly one completed source delta: {fault:?}, cycle {cycle}"
            );
            assert_eq!(
                db.get_metadata("sync_token:PrimarySync")
                    .await
                    .unwrap()
                    .as_deref(),
                Some("validated-delta-successor")
            );
            assert_eq!(
                db.get_metadata(ENUM_CONFIG_HASH_KEY)
                    .await
                    .unwrap()
                    .as_deref(),
                Some(current_hash.as_str())
            );
            assert!(
                db.get_metadata(PENDING_ENUM_CONFIG_HASH_KEY)
                    .await
                    .unwrap()
                    .is_none()
            );
            assert!(db.get_metadata(&marker).await.unwrap().is_none());
            assert_eq!(result.stats.downloaded, 0);
            assert!(
                db.get_metadata(&pending_zone_token_key(&current_hash, "PrimarySync"))
                    .await
                    .unwrap()
                    .is_none(),
                "activation consumes the pending candidate"
            );
            if cycle > recovery_cycle || !drift || resumes_candidate {
                assert_eq!(
                    unfiled_queries.load(Ordering::SeqCst),
                    0,
                    "no repeat Unfiled inventory"
                );
            } else {
                assert!(
                    unfiled_queries.load(Ordering::SeqCst) > 0,
                    "initial drift requires current inventory"
                );
            }
            assert!(
                smart_queries.load(Ordering::SeqCst) > 0,
                "selected Hidden refresh still runs"
            );
        }
        if active_fault == Some(MixedFault::CheckpointWrite) {
            db.acquire_lock("remove mixed checkpoint fault")
                .unwrap()
                .execute_batch("DROP TRIGGER mixed_checkpoint_fault")
                .unwrap();
        }
        let rows = db.get_downloaded_page(0, 10).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].local_path.as_deref(), Some(kept.as_path()));
        assert_eq!(std::fs::read(&kept).unwrap(), bytes);
        assert_eq!(
            std::fs::metadata(&kept).unwrap().modified().unwrap(),
            modified
        );
        assert!(db.get_pending().await.unwrap().is_empty());
        assert_eq!(std::fs::read_dir(&media).unwrap().count(), 1);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InventoryFault {
    MissingCount,
    FailedQuery,
    MalformedAsset,
    Cancellation,
    BridgeQuery,
    BridgeCancellation,
    AnchorRead,
    AnchorWrite,
    StateWrite,
}

#[derive(Clone)]
struct InventorySession {
    inner: crate::test_helpers::DynamicRecentPhotosSession,
    zone: &'static str,
    smart: bool,
    fault: Option<InventoryFault>,
    download_url: Option<String>,
    changes: Arc<AtomicUsize>,
    cancel: CancellationToken,
}

#[async_trait::async_trait]
impl PhotosSession for InventorySession {
    async fn post(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        if url.contains("/changes/zone?") {
            assert!(!self.smart, "smart selection must use its actual query");
            self.changes.fetch_add(1, Ordering::SeqCst);
            return Ok(json!({"zones": [{
                "zoneID": {"zoneName": self.zone, "ownerRecordName": "_defaultOwner"},
                "syncToken": "validated-delta-successor",
                "moreComing": false,
                "records": []
            }]}));
        }
        if self.smart
            && url.contains("/internal/records/query/batch?")
            && self.fault == Some(InventoryFault::MissingCount)
        {
            return Ok(json!({"batch": [{"records": [{"fields": {}}]}]}));
        }
        if url.contains("/records/query?") {
            let bridge = self.smart && self.changes.load(Ordering::SeqCst) > 0;
            if self.fault == Some(InventoryFault::FailedQuery)
                || (bridge && self.fault == Some(InventoryFault::BridgeQuery))
            {
                return Ok(json!({"records": null}));
            }
            if self.fault == Some(InventoryFault::Cancellation)
                || (bridge && self.fault == Some(InventoryFault::BridgeCancellation))
            {
                self.cancel.cancel();
            }
        }
        let mut response = self.inner.post(url, body, headers).await?;
        if let Some(download_url) = &self.download_url
            && let Some(records) = response["records"].as_array_mut()
        {
            use sha2::Digest;
            let media = include_bytes!("../../../../tests/data/media/pattern.jpg");
            for record in records {
                if record["recordType"] == "CPLMaster"
                    && let Some(resource) =
                        record["fields"]["resOriginalRes"]["value"].as_object_mut()
                {
                    resource.insert("downloadURL".into(), json!(download_url));
                    resource.insert("size".into(), json!(media.len()));
                    resource.insert(
                        "fileChecksum".into(),
                        json!(data_encoding::BASE64.encode(&sha2::Sha256::digest(media))),
                    );
                }
            }
        }
        if self.fault == Some(InventoryFault::MalformedAsset)
            && url.contains("/records/query?")
            && let Some(records) = response["records"].as_array_mut()
        {
            for record in records {
                if record["recordType"] == "CPLAsset" {
                    record["fields"]
                        .as_object_mut()
                        .unwrap()
                        .remove("assetDate");
                }
            }
        }
        Ok(response)
    }

    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

fn inventory_named_album(zone: &str, name: &str, session: Box<dyn PhotosSession>) -> PhotoAlbum {
    let container = format!("container-{name}");
    PhotoAlbum::new(
        PhotoAlbumConfig {
            params: Arc::new(std::collections::HashMap::new()),
            service_endpoint: Arc::from("https://example.com"),
            name: Arc::from(name),
            list_type: Arc::from("CPLContainerRelationLiveByAssetDate"),
            obj_type: Arc::from(format!(
                "CPLContainerRelationNotDeletedByAssetDate:{container}"
            )),
            query_filter: Some(Arc::new(json!([{
                "fieldName": "parentId",
                "comparator": "EQUALS",
                "fieldValue": {"type": "STRING", "value": container},
            }]))),
            page_size: 100,
            zone_id: Arc::new(json!({"zoneName": zone})),
            retry_config: crate::retry::RetryConfig::default(),
            container_id: Some(Arc::from(container)),
            cross_zone_sources: Vec::new(),
        },
        session,
    )
}

#[derive(Clone)]
struct InventoryStep {
    zone: &'static str,
    passes: Vec<(PassKind, &'static str, u64)>,
    full: bool,
    drift: bool,
    stale: bool,
    retry: bool,
    mode: download::DownloadRunMode,
    fault: Option<InventoryFault>,
    download_url: Option<String>,
    options: crate::sync_loop::test_support::RunCycleDownloadConfigOptions,
}

impl InventoryStep {
    fn mixed(unfiled: u64, hidden: u64) -> Self {
        Self {
            zone: "PrimarySync",
            passes: vec![
                (PassKind::Unfiled, "", unfiled),
                (PassKind::SmartFolder, "Hidden", hidden),
            ],
            full: true,
            drift: false,
            stale: false,
            retry: false,
            mode: download::DownloadRunMode::Download,
            fault: None,
            download_url: None,
            options: crate::sync_loop::test_support::RunCycleDownloadConfigOptions {
                media: crate::sync_loop::test_support::media_without_photo_downloads(),
                ..Default::default()
            },
        }
    }

    fn hidden(total: u64) -> Self {
        Self {
            passes: vec![(PassKind::SmartFolder, "Hidden", total)],
            full: false,
            ..Self::mixed(0, 0)
        }
    }
}

fn inventory_root() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("media")).unwrap();
    std::fs::write(
        root.path().join("media/historical.jpg"),
        include_bytes!("../../../../tests/data/media/pattern.jpg"),
    )
    .unwrap();
    root
}

async fn inventory_anchors(root: &std::path::Path) -> Vec<(String, String)> {
    let db = state::SqliteStateDb::open(&root.join("state.db"))
        .await
        .unwrap();
    let conn = db.acquire_lock("inventory evidence after reopen").unwrap();
    let mut query = conn
        .prepare(
            "SELECT key, value FROM metadata WHERE key LIKE 'inventory_anchor_v1:%' ORDER BY key",
        )
        .unwrap();
    query
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

fn anchor_total(rows: &[(String, String)]) -> u64 {
    assert_eq!(rows.len(), 1);
    let anchor: Value = serde_json::from_str(&rows[0].1).unwrap();
    assert_eq!(anchor["version"], 1);
    anchor["api_total_at_start"].as_u64().unwrap()
}

async fn inventory_cycle(
    root: &std::path::Path,
    step: &InventoryStep,
) -> crate::sync_cycle::CycleResult {
    use crate::cycle_reporter::{CycleFacts, CycleReporter, CycleReporterConfig};
    use crate::sync_loop::test_support::{
        FailingMetadataSetDb, MetadataSetFailure,
        make_run_cycle_download_config_builder_with_options,
    };

    let db = Arc::new(
        state::SqliteStateDb::open(&root.join("state.db"))
            .await
            .unwrap(),
    );
    let mut config = make_run_cycle_config();
    config.runtime.dry_run = step.mode == download::DownloadRunMode::DryRun;
    config.runtime.refresh_metadata = step.full;
    config.filters.recent = step.options.recent;
    config.filters.skip_created_before = step.options.skip_created_before;
    config.filters.skip_created_after = step.options.skip_created_after;
    if step.drift {
        db.set_metadata(ENUM_CONFIG_HASH_KEY, "prior-enum-generation")
            .await
            .unwrap();
        if db
            .get_metadata(&format!("sync_token:{}", step.zone))
            .await
            .unwrap()
            .is_none()
        {
            db.set_metadata(
                &format!("sync_token:{}", step.zone),
                "retained-valid-cursor",
            )
            .await
            .unwrap();
        }
    }
    let store: Arc<dyn download::DownloadStore> = match step.fault {
        Some(InventoryFault::AnchorRead) => Arc::new(
            FailingMetadataSetDb::without_set_failure(db.clone(), "inventory read fault")
                .with_get_failure(MetadataSetFailure::Prefix("inventory_anchor_v1:")),
        ),
        Some(InventoryFault::AnchorWrite) => Arc::new(FailingMetadataSetDb::new(
            db.clone(),
            MetadataSetFailure::Prefix("inventory_anchor_v1:"),
            "inventory write fault",
        )),
        Some(InventoryFault::StateWrite) => Arc::new(
            FailingMetadataSetDb::without_set_failure(db.clone(), "inventory state write fault")
                .with_upsert_seen_failure(),
        ),
        _ => db.clone(),
    };
    let cancel = CancellationToken::new();
    let changes = Arc::new(AtomicUsize::new(0));
    let passes = step
        .passes
        .iter()
        .map(|&(kind, name, count)| {
            let ids = (0..count)
                .map(|index| format!("{name}-master-{index:04}"))
                .collect();
            let session = InventorySession {
                inner: crate::test_helpers::DynamicRecentPhotosSession::from_ids(ids)
                    .with_zone(step.zone)
                    .with_token("inventory-query-eof")
                    .with_filename_prefix(if name.is_empty() { "unfiled" } else { name }),
                zone: step.zone,
                smart: kind == PassKind::SmartFolder,
                fault: step.fault,
                download_url: step.download_url.clone(),
                changes: changes.clone(),
                cancel: cancel.clone(),
            };
            let album = if kind == PassKind::SmartFolder {
                smart_album(step.zone, name, Box::new(session))
            } else if kind == PassKind::Album {
                inventory_named_album(step.zone, name, Box::new(session))
            } else {
                make_named_full_album_with_boxed_session(step.zone, name, Box::new(session))
            };
            AlbumPass {
                kind,
                album,
                exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
            }
        })
        .collect();
    let mut library = make_run_cycle_library_state_with_passes(
        step.zone,
        &format!("sync_token:{}", step.zone),
        passes,
    );
    library.plan_is_stale = step.stale;
    let media = root.join("media");
    let build =
        make_run_cycle_download_config_builder_with_options(&media, store.clone(), step.options);
    // The existing explicit refresh mode requests a full query without resetting
    // cursors. Preserve the cycle owner's mode choice for the delta bridge.
    let builder = |mode, excludes, groupings, zone| {
        let mut config = build(mode, excludes, groupings, zone);
        let selected = Arc::make_mut(&mut config);
        selected.retry_only = step.retry;
        selected.refresh_metadata = step.full;
        config
    };
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let result = Box::pin(run_cycle(
        &[&library],
        &config,
        Some(store.as_ref()),
        step.retry,
        &builder,
        download::DownloadControls::new(step.mode, download::DownloadReporting::hidden()),
        &shared,
        &cancel,
    ))
    .await
    .unwrap();
    crate::sync_loop::reporting::record_cycle_run(Some(db.as_ref()), &result, chrono::Utc::now())
        .await;
    let report_path = root.join("inventory-report.json");
    let notifier = crate::notifications::Notifier::new(None);
    let reporter = CycleReporter::new(CycleReporterConfig {
        watch_mode: false,
        report_path: Some(&report_path),
        run_options: crate::report::RunOptions::from_config(&config),
        health_dir: root,
        personality_mode: crate::personality::Mode::Off,
        state_db: Some(db.as_ref()),
        metrics_handle: None,
        notifier: &notifier,
    });
    reporter
        .report_completed_cycle(
            &mut crate::health::HealthStatus::new(),
            CycleFacts::new(
                &result.stats,
                result.failed_count,
                result.session_expired,
                std::time::Duration::ZERO,
            ),
        )
        .await;
    let report: Value = serde_json::from_slice(&std::fs::read(&report_path).unwrap()).unwrap();
    let projected = serde_json::to_value(&result.stats).unwrap();
    for field in [
        "api_total_at_start",
        "api_total_at_start_partial",
        "inventory_drop_warnings",
        "inventory_drop_assets",
        "inventory_drop_percent",
        "inventory_drop_previous_total",
        "inventory_drop_current_total",
        "inventory_drop_library",
    ] {
        assert_eq!(report["stats"][field], projected[field], "{field}");
    }
    assert_eq!(
        std::fs::read(media.join("historical.jpg")).unwrap(),
        include_bytes!("../../../../tests/data/media/pattern.jpg"),
    );
    // All production handles, including the reporter and builder, leave scope
    // before the caller reopens the file-backed database.
    result
}

async fn assert_inventory_ledger(root: &std::path::Path, result: &crate::sync_cycle::CycleResult) {
    let db = state::SqliteStateDb::open(&root.join("state.db"))
        .await
        .unwrap();
    let summary = db.get_summary().await.unwrap();
    assert_eq!(
        summary.last_api_total_at_start,
        result.stats.api_total_at_start
    );
    assert_eq!(
        summary.last_api_total_at_start_partial,
        result.stats.api_total_at_start_partial
    );
    assert_eq!(
        summary.last_inventory_drop_detected,
        result.stats.inventory_drop_warnings > 0
    );
    assert_eq!(
        summary.last_inventory_drop_previous_total,
        result.stats.inventory_drop_previous_total
    );
    assert_eq!(
        summary.last_inventory_drop_current_total,
        result.stats.inventory_drop_current_total
    );
    assert_eq!(
        summary.last_inventory_drop_library,
        result.stats.inventory_drop_library
    );
}

#[tokio::test]
#[tracing_test::traced_test]
async fn comparable_inventory_full_then_mixed_refresh_does_not_replace_anchor() {
    let root = inventory_root();
    let mut step = InventoryStep::mixed(90, 10);
    let first = inventory_cycle(root.path(), &step).await;
    assert_eq!(first.failed_count, 0);
    assert_eq!(first.stats.assets_seen, 100);
    assert_eq!(first.stats.api_total_at_start, Some(100));
    assert_inventory_ledger(root.path(), &first).await;
    let before = inventory_anchors(root.path()).await;
    assert_eq!(anchor_total(&before), 100);
    step.full = false;
    let delta = inventory_cycle(root.path(), &step).await;
    assert_eq!(delta.failed_count, 0);
    assert!(delta.db_sync_token_advance_safe);
    assert_eq!(delta.stats.api_total_at_start, Some(10));
    assert!(delta.stats.api_total_at_start_partial);
    assert_eq!(delta.stats.inventory_drop_warnings, 0);
    assert_inventory_ledger(root.path(), &delta).await;
    assert_eq!(inventory_anchors(root.path()).await, before);
    step.full = true;
    step.passes.reverse();
    let full = inventory_cycle(root.path(), &step).await;
    assert_eq!(full.stats.inventory_drop_warnings, 0);
    assert_eq!(anchor_total(&inventory_anchors(root.path()).await), 100);
    assert!(!logs_contain(
        "Inventory dropped below previous comparable full run"
    ));
}

#[tokio::test]
#[tracing_test::traced_test]
async fn comparable_inventory_bridge_keeps_initial_count_and_additive_activity() {
    let root = inventory_root();
    let mut step = InventoryStep::mixed(90, 10);
    step.drift = true;
    let result = inventory_cycle(root.path(), &step).await;
    assert_eq!(result.failed_count, 0);
    assert!(result.db_sync_token_advance_safe);
    assert_eq!(result.stats.api_total_at_start, Some(110));
    assert!(result.stats.api_total_at_start_partial);
    assert_eq!(result.stats.assets_seen, 110);
    assert_eq!(result.stats.inventory_drop_warnings, 0);
    let rows = inventory_anchors(root.path()).await;
    assert_eq!(anchor_total(&rows), 100);
    let anchor: Value = serde_json::from_str(&rows[0].1).unwrap();
    assert_eq!(anchor["assets_seen"], 100);
    assert_inventory_ledger(root.path(), &result).await;
    let db = state::SqliteStateDb::open(&root.path().join("state.db"))
        .await
        .unwrap();
    assert_eq!(
        db.get_metadata("sync_token:PrimarySync")
            .await
            .unwrap()
            .as_deref(),
        Some("validated-delta-successor")
    );
    assert_eq!(
        db.get_metadata(ENUM_CONFIG_HASH_KEY)
            .await
            .unwrap()
            .as_deref(),
        Some(download::compute_config_hash(&make_run_cycle_config()).as_str())
    );
    drop(db);
    step.drift = false;
    step.full = false;
    let mixed = inventory_cycle(root.path(), &step).await;
    assert_eq!(mixed.stats.inventory_drop_warnings, 0);
    assert_eq!(inventory_anchors(root.path()).await, rows);
    step.full = true;
    let full = inventory_cycle(root.path(), &step).await;
    assert_eq!(full.stats.inventory_drop_warnings, 0);
    assert_eq!(anchor_total(&inventory_anchors(root.path()).await), 100);
    assert!(!logs_contain(
        "Inventory dropped below previous comparable full run"
    ));
}

#[tokio::test]
async fn comparable_inventory_resolved_pass_changes_and_order_are_scoped() {
    let root = inventory_root();
    let mut step = InventoryStep::mixed(0, 0);
    // Same requested configuration and enum hash; actual all-album resolution changes.
    step.passes = vec![(PassKind::Album, "A", 60), (PassKind::Album, "B", 40)];
    assert_eq!(
        inventory_cycle(root.path(), &step)
            .await
            .stats
            .inventory_drop_warnings,
        0
    );
    let original = inventory_anchors(root.path()).await;
    assert_eq!(anchor_total(&original), 100);
    step.passes = vec![(PassKind::Album, "B", 40), (PassKind::Album, "C", 10)];
    let changed = inventory_cycle(root.path(), &step).await;
    assert_eq!(changed.stats.inventory_drop_warnings, 0);
    let changed_rows = inventory_anchors(root.path()).await;
    assert_eq!(changed_rows.len(), 2);
    assert!(changed_rows.contains(&original[0]));
    step.passes = vec![(PassKind::Album, "B", 40), (PassKind::Album, "A", 55)];
    let reordered = inventory_cycle(root.path(), &step).await;
    assert_eq!(reordered.stats.inventory_drop_warnings, 1);
    assert_eq!(reordered.stats.inventory_drop_previous_total, Some(100));
    assert_eq!(reordered.stats.inventory_drop_current_total, Some(95));
    assert_eq!(inventory_anchors(root.path()).await.len(), 2);
    assert_inventory_ledger(root.path(), &reordered).await;
}

#[tokio::test]
#[tracing_test::traced_test]
async fn comparable_inventory_complete_smart_only_decline_warns_and_survives_reopen() {
    let root = inventory_root();
    let mut step = InventoryStep::hidden(100);
    assert_eq!(
        inventory_cycle(root.path(), &step)
            .await
            .stats
            .inventory_drop_warnings,
        0
    );
    step.passes = vec![(PassKind::SmartFolder, "Favorites", 10)];
    assert_eq!(
        inventory_cycle(root.path(), &step)
            .await
            .stats
            .inventory_drop_warnings,
        0
    );
    step.passes = vec![(PassKind::SmartFolder, "Hidden", 95)];
    let result = inventory_cycle(root.path(), &step).await;
    assert_eq!(result.stats.inventory_drop_warnings, 1);
    assert_eq!(result.stats.inventory_drop_assets, 5);
    assert_eq!(result.stats.inventory_drop_percent, Some(5.0));
    assert_eq!(result.stats.inventory_drop_previous_total, Some(100));
    assert_eq!(result.stats.inventory_drop_current_total, Some(95));
    assert_eq!(
        result.stats.inventory_drop_library.as_deref(),
        Some("PrimarySync")
    );
    assert_inventory_ledger(root.path(), &result).await;
    let steady = inventory_cycle(root.path(), &step).await;
    assert_eq!(steady.stats.inventory_drop_warnings, 0);
    assert_inventory_ledger(root.path(), &steady).await;
    assert_eq!(inventory_anchors(root.path()).await.len(), 2);
    assert!(logs_contain(
        "Inventory dropped below previous comparable full run"
    ));
}

#[tokio::test]
#[tracing_test::traced_test]
async fn comparable_inventory_four_percent_decline_does_not_warn() {
    let root = inventory_root();
    let mut step = InventoryStep::hidden(100);
    inventory_cycle(root.path(), &step).await;
    step.passes = vec![(PassKind::SmartFolder, "Hidden", 96)];
    let result = inventory_cycle(root.path(), &step).await;
    assert_eq!(result.stats.inventory_drop_warnings, 0);
    assert_eq!(anchor_total(&inventory_anchors(root.path()).await), 96);
    assert_inventory_ledger(root.path(), &result).await;
    assert!(!logs_contain(
        "Inventory dropped below previous comparable full run"
    ));
}

#[tokio::test]
async fn comparable_inventory_vetoes_preserve_complete_anchor_then_recover() {
    use crate::config::CreatedDateFilter;
    for case in 0..13 {
        let root = inventory_root();
        inventory_cycle(root.path(), &InventoryStep::mixed(90, 10)).await;
        let before = inventory_anchors(root.path()).await;
        let mut step = InventoryStep::mixed(9, 1);
        match case {
            0 => step.fault = Some(InventoryFault::MissingCount),
            1 => step.fault = Some(InventoryFault::FailedQuery),
            2 => step.fault = Some(InventoryFault::MalformedAsset),
            3 => step.fault = Some(InventoryFault::Cancellation),
            4 => step.stale = true,
            5 => step.options.recent = Some(1),
            6 => {
                step.options.skip_created_before = Some(CreatedDateFilter::CaptureDate(
                    chrono::NaiveDate::from_ymd_opt(2000, 1, 1).unwrap(),
                ))
            }
            7 => {
                step.options.skip_created_after = Some(CreatedDateFilter::CaptureDate(
                    chrono::NaiveDate::from_ymd_opt(2030, 1, 1).unwrap(),
                ))
            }
            8 => step.mode = download::DownloadRunMode::DryRun,
            9 => step.mode = download::DownloadRunMode::PrintFilenames,
            10 => step.retry = true,
            11 => {
                step.drift = true;
                step.fault = Some(InventoryFault::BridgeQuery);
            }
            _ => {
                assert_eq!(case, 12, "unexpected inventory veto case");
                step.drift = true;
                step.fault = Some(InventoryFault::BridgeCancellation);
            }
        }
        let result = inventory_cycle(root.path(), &step).await;
        assert_eq!(
            result.stats.inventory_drop_warnings, 0,
            "case {case}: {result:?}"
        );
        assert_eq!(inventory_anchors(root.path()).await, before, "case {case}");
        assert_inventory_ledger(root.path(), &result).await;
        let recovered = inventory_cycle(root.path(), &InventoryStep::mixed(90, 10)).await;
        assert_eq!(recovered.failed_count, 0, "case {case}: {recovered:?}");
        assert_eq!(recovered.stats.inventory_drop_warnings, 0, "case {case}");
        assert_eq!(anchor_total(&inventory_anchors(root.path()).await), 100);
    }
}

#[tokio::test]
#[tracing_test::traced_test]
async fn comparable_inventory_state_write_failure_holds_anchor_and_checkpoint_then_recovers() {
    use crate::sync_loop::test_support::run_cycle_expected_date_dir;

    let root = inventory_root();
    let media = root
        .path()
        .join("media")
        .join(run_cycle_expected_date_dir());
    std::fs::create_dir_all(&media).unwrap();
    // Seed owned media through real downloads from a local server. Later
    // cycles must preserve those files without another transfer.
    let body = include_bytes!("../../../../tests/data/media/pattern.jpg").as_slice();
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(body))
        .mount(&server)
        .await;
    let mut paths = Vec::with_capacity(100);
    for (prefix, count) in [("unfiled", 90), ("Hidden", 10)] {
        for index in 0..count {
            let path = media.join(format!("{prefix}-{index:04}.JPG"));
            paths.push(path);
        }
    }
    let mut step = InventoryStep::mixed(90, 10);
    step.options.media = Default::default();
    step.download_url = Some(format!("{}/inventory-fixture.jpg", server.uri()));
    let first = inventory_cycle(root.path(), &step).await;
    assert_eq!(first.failed_count, 0, "{first:?}");
    assert_eq!(first.stats.state_write_failures, 0);
    assert_eq!(first.stats.assets_seen, 100);
    assert_eq!(first.stats.downloaded, 100);
    for path in &paths {
        assert_eq!(std::fs::read(path).unwrap(), body);
    }
    let before = inventory_anchors(root.path()).await;
    assert_eq!(anchor_total(&before), 100);

    // A real incremental cycle establishes a distinct retained successor:
    // the upcoming full query offers inventory-query-eof, not this cursor.
    step.full = false;
    let delta = inventory_cycle(root.path(), &step).await;
    assert!(delta.db_sync_token_advance_safe);
    assert_eq!(inventory_anchors(root.path()).await, before);
    {
        let db = state::SqliteStateDb::open(&root.path().join("state.db"))
            .await
            .unwrap();
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("validated-delta-successor")
        );
    }

    std::fs::remove_file(&paths[0]).unwrap();
    step.full = true;
    step.passes[0].2 = 85;
    step.fault = Some(InventoryFault::StateWrite);
    let failed = inventory_cycle(root.path(), &step).await;
    assert_eq!(failed.stats.api_total_at_start, Some(95));
    // Composed activity counts remain distinct from complete inventory evidence.
    assert!(failed.stats.api_total_at_start_partial);
    assert_eq!(failed.stats.assets_seen, 95);
    assert_eq!(failed.stats.enumeration_errors, 0);
    assert!(!failed.stats.enumeration_incomplete);
    assert!(!failed.stats.interrupted);
    assert_eq!(failed.stats.state_write_failures, 1);
    assert!(!failed.db_sync_token_advance_safe);
    assert_eq!(failed.stats.downloaded, 0);
    assert_eq!(failed.stats.bytes_downloaded, 0);
    assert_eq!(failed.stats.inventory_drop_warnings, 0);
    assert_eq!(inventory_anchors(root.path()).await, before);
    assert_inventory_ledger(root.path(), &failed).await;
    {
        let db = state::SqliteStateDb::open(&root.path().join("state.db"))
            .await
            .unwrap();
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("validated-delta-successor"),
            "a complete count cannot authorize a checkpoint after upsert_seen fails"
        );
        let summary = db.get_summary().await.unwrap();
        assert_eq!(summary.pending + summary.failed, 0);
    }
    assert!(!paths[0].exists());
    for path in &paths[1..] {
        assert_eq!(std::fs::read(path).unwrap(), body);
    }

    // Restore the landed file and remove only the injected failure. The same
    // resolved scope now admits the real 5% decline and its query checkpoint.
    std::fs::write(&paths[0], body).unwrap();
    step.fault = None;
    let recovered = inventory_cycle(root.path(), &step).await;
    assert_eq!(recovered.failed_count, 0);
    assert_eq!(recovered.stats.state_write_failures, 0);
    assert!(recovered.db_sync_token_advance_safe);
    assert_eq!(recovered.stats.inventory_drop_warnings, 1);
    assert_eq!(recovered.stats.inventory_drop_previous_total, Some(100));
    assert_eq!(recovered.stats.inventory_drop_current_total, Some(95));
    assert_eq!(anchor_total(&inventory_anchors(root.path()).await), 95);
    assert_inventory_ledger(root.path(), &recovered).await;
    {
        let db = state::SqliteStateDb::open(&root.path().join("state.db"))
            .await
            .unwrap();
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("inventory-query-eof")
        );
    }

    let steady = inventory_cycle(root.path(), &step).await;
    assert_eq!(steady.failed_count, 0);
    assert_eq!(steady.stats.state_write_failures, 0);
    assert!(steady.db_sync_token_advance_safe);
    assert_eq!(steady.stats.inventory_drop_warnings, 0);
    assert_eq!(steady.stats.downloaded, 0);
    assert_eq!(anchor_total(&inventory_anchors(root.path()).await), 95);
    assert_inventory_ledger(root.path(), &steady).await;
    for path in &paths {
        assert_eq!(std::fs::read(path).unwrap(), body);
    }
    assert_eq!(std::fs::read_dir(&media).unwrap().count(), paths.len());
    assert_eq!(server.received_requests().await.unwrap().len(), 100);
}

#[tokio::test]
async fn comparable_inventory_legacy_and_unverifiable_anchors_are_never_certified() {
    let root = inventory_root();
    let hash = download::compute_config_hash(&make_run_cycle_config());
    let legacy_key = format!("inventory_anchor:{hash}:PrimarySync");
    let legacy =
        json!({"api_total_at_start": 110, "assets_seen": 110, "completed_at": 1}).to_string();
    let db = state::SqliteStateDb::open(&root.path().join("state.db"))
        .await
        .unwrap();
    db.set_metadata(&legacy_key, &legacy).await.unwrap();
    drop(db);
    let step = InventoryStep::hidden(100);
    let first = inventory_cycle(root.path(), &step).await;
    assert_eq!(first.stats.inventory_drop_warnings, 0);
    let rows = inventory_anchors(root.path()).await;
    assert_eq!(anchor_total(&rows), 100);
    let mut unsupported: Value = serde_json::from_str(&rows[0].1).unwrap();
    unsupported["version"] = json!(99);
    unsupported["api_total_at_start"] = json!(110);
    for invalid in [
        unsupported.to_string(),
        "not json".to_owned(),
        legacy.clone(),
        json!({"version": 1, "scope": "different coverage", "api_total_at_start": 110, "assets_seen": 110, "completed_at": 1}).to_string(),
    ] {
        let db = state::SqliteStateDb::open(&root.path().join("state.db")).await.unwrap();
        db.set_metadata(&rows[0].0, &invalid).await.unwrap();
        drop(db);
        let result = inventory_cycle(root.path(), &step).await;
        assert_eq!(result.stats.inventory_drop_warnings, 0);
        assert_eq!(anchor_total(&inventory_anchors(root.path()).await), 100);
        assert_inventory_ledger(root.path(), &result).await;
    }
    let db = state::SqliteStateDb::open(&root.path().join("state.db"))
        .await
        .unwrap();
    assert_eq!(
        db.get_metadata(&legacy_key).await.unwrap().as_deref(),
        Some(legacy.as_str())
    );
}

#[tokio::test]
async fn comparable_inventory_anchor_io_failure_does_not_invent_or_erase_evidence() {
    for fault in [InventoryFault::AnchorRead, InventoryFault::AnchorWrite] {
        let root = inventory_root();
        inventory_cycle(root.path(), &InventoryStep::hidden(100)).await;
        let before = inventory_anchors(root.path()).await;
        let mut step = InventoryStep::hidden(95);
        step.fault = Some(fault);
        let failed = inventory_cycle(root.path(), &step).await;
        assert_eq!(failed.failed_count, 0);
        assert_eq!(
            failed.stats.inventory_drop_warnings,
            usize::from(fault == InventoryFault::AnchorWrite)
        );
        assert_eq!(inventory_anchors(root.path()).await, before);
        assert_inventory_ledger(root.path(), &failed).await;
        step.fault = None;
        let recovered = inventory_cycle(root.path(), &step).await;
        assert_eq!(recovered.stats.inventory_drop_warnings, 1);
        assert_eq!(anchor_total(&inventory_anchors(root.path()).await), 95);
        assert_inventory_ledger(root.path(), &recovered).await;
        assert_eq!(
            inventory_cycle(root.path(), &step)
                .await
                .stats
                .inventory_drop_warnings,
            0
        );
    }
}

#[tokio::test]
async fn comparable_inventory_zones_are_independent_and_empty_count_is_available() {
    let root = inventory_root();
    inventory_cycle(root.path(), &InventoryStep::hidden(100)).await;
    let mut other = InventoryStep::hidden(10);
    other.zone = "SharedSync-INVENTORY";
    let result = inventory_cycle(root.path(), &other).await;
    assert_eq!(result.stats.inventory_drop_warnings, 0);
    let rows = inventory_anchors(root.path()).await;
    assert_eq!(rows.len(), 2);
    other.fault = Some(InventoryFault::FailedQuery);
    inventory_cycle(root.path(), &other).await;
    assert_eq!(inventory_anchors(root.path()).await, rows);
    let result = inventory_cycle(root.path(), &InventoryStep::hidden(0)).await;
    assert_eq!(result.stats.api_total_at_start, Some(0));
    assert_eq!(result.stats.inventory_drop_warnings, 1);
    assert_eq!(result.stats.inventory_drop_previous_total, Some(100));
    assert_eq!(result.stats.inventory_drop_current_total, Some(0));
    assert_inventory_ledger(root.path(), &result).await;
}

#[tokio::test]
async fn comparable_inventory_targeted_album_subset_cannot_seed_complete_scope() {
    let root = inventory_root();
    let mut step = InventoryStep::mixed(0, 0);
    step.passes = vec![(PassKind::Album, "A", 90), (PassKind::Unfiled, "", 10)];
    let first = inventory_cycle(root.path(), &step).await;
    assert_eq!(first.failed_count, 0);
    let before = inventory_anchors(root.path()).await;
    assert_eq!(anchor_total(&before), 100);
    step.full = false;
    step.passes.insert(1, (PassKind::Album, "B", 5));
    let subset = inventory_cycle(root.path(), &step).await;
    assert_eq!(subset.failed_count, 0);
    assert!(subset.db_sync_token_advance_safe);
    assert_eq!(subset.stats.api_total_at_start, Some(5));
    assert_eq!(
        subset.stats.full_enumeration_reason,
        Some(download::FullEnumerationReason::AlbumRelationHydrationIncomplete)
    );
    assert_eq!(subset.stats.inventory_drop_warnings, 0);
    assert_eq!(inventory_anchors(root.path()).await, before);
    assert_inventory_ledger(root.path(), &subset).await;
    // Once every selected pass is really queried, the new scope can seed itself.
    step.full = true;
    let complete = inventory_cycle(root.path(), &step).await;
    assert_eq!(complete.stats.inventory_drop_warnings, 0);
    assert_eq!(inventory_anchors(root.path()).await.len(), 2);
}
