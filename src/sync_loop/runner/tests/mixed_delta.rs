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

fn hidden_album(session: MixedSession) -> PhotoAlbum {
    let definition = crate::icloud::photos::smart_folders::smart_folders()
        .into_iter()
        .find(|(name, _)| *name == "Hidden")
        .unwrap()
        .1;
    PhotoAlbum::new(
        PhotoAlbumConfig {
            params: Arc::new(std::collections::HashMap::new()),
            service_endpoint: Arc::from("https://example.com"),
            name: Arc::from("Hidden"),
            list_type: Arc::from(definition.list_type),
            obj_type: Arc::from(definition.obj_type),
            query_filter: definition.query_filter,
            page_size: 100,
            zone_id: Arc::new(json!({"zoneName": "PrimarySync"})),
            retry_config: crate::retry::RetryConfig::default(),
            container_id: None,
            cross_zone_sources: Vec::new(),
        },
        Box::new(session),
    )
}

#[tokio::test]
async fn mixed_valid_delta_hidden_refresh_promotes_inventory_bridge_then_quiet_reopen() {
    mixed_lifecycle(true, None).await;
}

#[tokio::test]
async fn mixed_valid_delta_hidden_refresh_clears_retained_identity_marker_without_extra_replay() {
    mixed_lifecycle(false, None).await;
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
        mixed_lifecycle(true, Some(fault)).await;
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
                    album: hidden_album(smart),
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
