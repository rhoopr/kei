use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use reqwest::Client;
use rustc_hash::FxHashSet;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

use crate::commands::{AlbumPass, PassKind};
use crate::icloud::photos::{PhotoAsset, PhotosSession};
use crate::retry::RetryConfig;
use crate::state::SqliteStateDb;
use crate::test_helpers::{MockPhotosFlow, TestAssetRecord};

use super::super::dispatch::download_photos_with_sync;
use super::super::models::{
    DownloadControls, DownloadOutcome, DownloadReporting, DownloadRunMode, DownloadStore,
    FullEnumerationReason, METADATA_CAPTURE_REPAIR_FAILED_REASON, SyncMode,
};
use super::super::test_support::{
    PendingLookupSession, album_with_session, album_with_session_and_retry_config,
    changes_zone_response, incremental_photo_records, incremental_photo_records_with_favorite,
    mock_album, seed_downloaded_metadata_asset, test_config,
};
use super::run_metadata_capture_repair;

#[tokio::test]
async fn incremental_with_metadata_backfill_records_full_enumeration_reason() {
    let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().expect("state db"));
    let dir = TempDir::new().expect("temp dir");
    let record = crate::test_helpers::TestAssetRecord::new("BACKFILL_BEFORE_SYNC")
        .filename("backfill-before-sync.jpg")
        .checksum("ck_backfill_before_sync")
        .size(1024)
        .build();
    db.upsert_seen(&record).await.expect("seed pending row");
    let path = dir.path().join("backfill-before-sync.jpg");
    tokio::fs::write(&path, vec![0u8; 1024])
        .await
        .expect("write local file");
    db.mark_downloaded(
        "PrimarySync",
        "BACKFILL_BEFORE_SYNC",
        "original",
        &path,
        "local_hash",
        None,
    )
    .await
    .expect("mark downloaded");
    db.clear_metadata_hash_for_test("PrimarySync", "BACKFILL_BEFORE_SYNC", "original");
    assert!(db.has_downloaded_without_metadata_hash().await.unwrap());

    let session = MockPhotosFlow::new()
        .album_count(0)
        .empty_query_page(Some("zone-token-next"))
        .build();
    let passes = vec![AlbumPass {
        kind: PassKind::Unfiled,
        album: mock_album("", session),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];

    let mut config = test_config();
    config.directory = Arc::from(dir.path());
    config.state_db = Some(db);
    config.sync_mode = SyncMode::Incremental {
        zone_sync_token: "zone-token-prev".to_string(),
    };

    let result = download_photos_with_sync(
        &Client::new(),
        &passes,
        Arc::new(config),
        DownloadControls::new(DownloadRunMode::PrintFilenames, DownloadReporting::hidden()),
        CancellationToken::new(),
    )
    .await
    .expect("metadata backfill should fall back to full enumeration");

    assert!(result.full_enumeration_ran);
    assert_eq!(
        result.stats.full_enumeration_reason,
        Some(FullEnumerationReason::MetadataBackfill)
    );
}

#[tokio::test]
async fn contract_metadata_capture_revision_repair_is_durable() {
    let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
    let dir = TempDir::new().expect("temp dir");
    let stored_records = incremental_photo_records_with_favorite("CAPTURE_REVISION", false);
    let stored_asset = PhotoAsset::new(stored_records[0].clone(), stored_records[1].clone());
    let mut changed_records = incremental_photo_records_with_favorite("CAPTURE_REVISION", true);
    changed_records[1]["fields"]["assetDate"]["value"] = json!(1_700_000_000_123_i64);
    changed_records[1]["fields"]["addedDate"]["value"] = json!(1_700_000_000_789_i64);
    let pass = AlbumPass {
        kind: PassKind::Unfiled,
        album: album_with_session(
            "PrimarySync",
            "",
            Box::new(PendingLookupSession {
                records: Arc::new(changed_records),
            }),
        ),
        exclude_ids: Arc::new(FxHashSet::default()),
    };
    let mut config = test_config();
    config.directory = Arc::from(dir.path());
    config.state_db = Some(Arc::clone(&db) as Arc<dyn DownloadStore>);
    config.sync_mode = SyncMode::Incremental {
        zone_sync_token: "zone-token-prev".to_string(),
    };
    let media_path =
        seed_downloaded_metadata_asset(db.as_ref(), &config, &pass, &stored_asset).await;
    db.upsert_asset_master_mapping("PrimarySync", "asset-CAPTURE_REVISION", "CAPTURE_REVISION")
        .await
        .expect("seed durable provider identity");
    assert!(
        db.claim_legacy_master_state_owner(
            "PrimarySync",
            "CAPTURE_REVISION",
            "asset-CAPTURE_REVISION",
        )
        .await
        .expect("seed legacy state owner")
    );
    db.set_metadata_capture_revision_for_test("PrimarySync", "CAPTURE_REVISION", 0);
    let before = tokio::fs::read(&media_path)
        .await
        .expect("read seeded media");

    let result = download_photos_with_sync(
        &Client::new(),
        std::slice::from_ref(&pass),
        Arc::new(config),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("normal sync should repair stale capture metadata");

    assert!(
        matches!(result.outcome, DownloadOutcome::Success),
        "unexpected outcome: {:?}, stats: {:?}",
        result.outcome,
        result.stats
    );
    assert_eq!(result.stats.downloaded, 0);
    assert_eq!(result.stats.metadata_capture_revision, Some(1));
    assert_eq!(result.stats.metadata_capture_refreshed, 1);
    assert_eq!(result.stats.metadata_capture_failures, 0);
    assert_eq!(result.stats.metadata_capture_remaining, 0);
    assert_eq!(result.sync_token.as_deref(), Some("zone-token-next"));
    let refreshed = db
        .get_downloaded_page(0, 1)
        .await
        .expect("read refreshed row")
        .remove(0);
    assert!(refreshed.metadata.is_favorite);
    assert_eq!(refreshed.created_at.timestamp_millis(), 1_700_000_000_123);
    assert_eq!(
        refreshed.added_at.unwrap().timestamp_millis(),
        1_700_000_000_789
    );
    assert_eq!(refreshed.local_path.as_deref(), Some(media_path.as_path()));
    assert_eq!(
        tokio::fs::read(&media_path)
            .await
            .expect("read repaired media"),
        before,
        "catalogue-only repair must not mutate media when outputs are disabled"
    );
    let summary = db.get_summary().await.expect("summary");
    let capture = summary
        .metadata_capture
        .iter()
        .find(|status| status.library == "PrimarySync")
        .expect("capture status");
    assert_eq!(capture.active_revision, 1);
    assert_eq!(capture.pending_revision, None);
    assert_eq!(capture.remaining_assets, 0);
}

#[tokio::test]
async fn metadata_capture_revision_repair_progresses_across_bounded_cycles() {
    #[derive(Clone, Debug)]
    struct CaptureScaleSession {
        records: Arc<HashMap<String, Value>>,
        rate_limited_once: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl PhotosSession for CaptureScaleSession {
        async fn post(
            &self,
            url: &str,
            body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            if !url.contains("/records/lookup?") {
                return Ok(if url.contains("/changes/zone?") {
                    changes_zone_response(Vec::new(), "zone-token-next")
                } else {
                    json!({"records": [], "syncToken": "ignored-query-token"})
                });
            }
            if !self.rate_limited_once.swap(true, Ordering::SeqCst) {
                return Err(crate::icloud::photos::session::HttpStatusError {
                    status: 429,
                    url: url.to_owned(),
                    retry_after: None,
                    body: None,
                }
                .into());
            }
            let request: Value = serde_json::from_str(&body)?;
            let records = request["records"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|record| record["recordName"].as_str())
                .filter_map(|record_name| self.records.get(record_name).cloned())
                .collect::<Vec<_>>();
            Ok(json!({"records": records}))
        }

        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(self.clone())
        }
    }

    const STALE_ASSETS: usize = 1_201;
    let stale_assets = u64::try_from(STALE_ASSETS).expect("asset count fits u64");
    let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
    let mut provider_records = HashMap::with_capacity(STALE_ASSETS * 2);
    for index in 0..STALE_ASSETS {
        let master_id = format!("CAPTURE_SCALE_{index:04}");
        let asset_id = format!("asset-{master_id}");
        for record in incremental_photo_records_with_favorite(&master_id, true) {
            provider_records.insert(
                record["recordName"]
                    .as_str()
                    .expect("provider record name")
                    .to_owned(),
                record,
            );
        }
        let record = TestAssetRecord::new(&asset_id)
            .filename("metadata-capture-scale.jpg")
            .checksum("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
            .size(1024)
            .build();
        db.upsert_seen(&record).await.expect("seed state row");
        db.mark_downloaded(
            "PrimarySync",
            &asset_id,
            "original",
            Path::new("/photos/metadata-capture-scale.jpg"),
            "seeded-local-sha256",
            None,
        )
        .await
        .expect("mark state row downloaded");
        db.upsert_asset_master_mapping("PrimarySync", &asset_id, &master_id)
            .await
            .expect("seed provider mapping");
    }
    db.acquire_lock("test_metadata_capture_scale_stale")
        .expect("state lock")
        .execute("DELETE FROM asset_metadata_capture_revisions", [])
        .expect("mark every asset stale");

    let pass = AlbumPass {
        kind: PassKind::Unfiled,
        album: album_with_session_and_retry_config(
            "PrimarySync",
            "",
            None,
            RetryConfig {
                max_retries: 1,
                base_delay_secs: 0,
                max_delay_secs: 0,
            },
            Box::new(CaptureScaleSession {
                records: Arc::new(provider_records),
                rate_limited_once: Arc::new(AtomicBool::new(false)),
            }),
        ),
        exclude_ids: Arc::new(FxHashSet::default()),
    };
    let mut config = test_config();
    let dir = TempDir::new().expect("temp dir");
    config.directory = Arc::from(dir.path());
    config.state_db = Some(Arc::clone(&db) as Arc<dyn DownloadStore>);
    config.sync_mode = SyncMode::Incremental {
        zone_sync_token: "zone-token-prev".to_string(),
    };

    for (cycle, refreshed, remaining, active_revision, rate_limited) in [
        (1, 500, 701, 0, 1),
        (2, 500, 201, 0, 0),
        (3, 201, 0, crate::state::METADATA_CAPTURE_REVISION, 0),
    ] {
        let result = download_photos_with_sync(
            &Client::new(),
            std::slice::from_ref(&pass),
            Arc::new(config.clone()),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("metadata-capture cycle");

        assert!(
            matches!(result.outcome, DownloadOutcome::Success),
            "cycle {cycle}: {:?}",
            result.stats
        );
        assert_eq!(
            result.stats.metadata_capture_refreshed, refreshed,
            "cycle {cycle}"
        );
        assert_eq!(
            result.stats.metadata_capture_remaining, remaining,
            "cycle {cycle}"
        );
        assert_eq!(result.stats.rate_limited, rate_limited, "cycle {cycle}");
        assert_eq!(
            result.stats.has_rate_limit_pressure(),
            rate_limited > 0,
            "cycle {cycle}"
        );
        let summary = db.get_summary().await.expect("capture status");
        let capture = summary
            .metadata_capture
            .iter()
            .find(|status| status.library == "PrimarySync")
            .expect("primary capture status");
        assert_eq!(capture.active_revision, active_revision, "cycle {cycle}");
        assert_eq!(
            capture.pending_revision,
            (remaining > 0).then_some(crate::state::METADATA_CAPTURE_REVISION),
            "cycle {cycle}"
        );
        assert_eq!(
            capture.processed_assets,
            stale_assets - remaining,
            "cycle {cycle}"
        );
        assert_eq!(capture.remaining_assets, remaining, "cycle {cycle}");
    }
}

#[tokio::test]
async fn metadata_capture_source_deletions_count_as_clean_progress() {
    const DELETED_ASSETS: usize = 500;
    const STALE_ASSETS: usize = DELETED_ASSETS + 1;

    let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
    let mut provider_records = Vec::with_capacity(DELETED_ASSETS * 2);
    for index in 0..STALE_ASSETS {
        let master_id = format!("CAPTURE_DELETED_{index:04}");
        let asset_id = format!("asset-{master_id}");
        if index < DELETED_ASSETS {
            let master = incremental_photo_records(&master_id)
                .into_iter()
                .next()
                .expect("master record");
            provider_records.push(master);
            provider_records.push(json!({
                "recordName": asset_id,
                "serverErrorCode": "UNKNOWN_ITEM",
                "reason": "record not found"
            }));
        }

        let record = TestAssetRecord::new(&asset_id)
            .filename("metadata-capture-deleted.jpg")
            .checksum("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
            .size(1024)
            .build();
        db.upsert_seen(&record).await.expect("seed state row");
        db.mark_downloaded(
            "PrimarySync",
            &asset_id,
            "original",
            Path::new("/photos/metadata-capture-deleted.jpg"),
            "seeded-local-sha256",
            None,
        )
        .await
        .expect("mark state row downloaded");
        db.upsert_asset_master_mapping("PrimarySync", &asset_id, &master_id)
            .await
            .expect("seed provider mapping");
    }
    db.acquire_lock("test_metadata_capture_deleted_stale")
        .expect("state lock")
        .execute("DELETE FROM asset_metadata_capture_revisions", [])
        .expect("mark every asset stale");

    let pass = AlbumPass {
        kind: PassKind::Unfiled,
        album: album_with_session(
            "PrimarySync",
            "",
            Box::new(PendingLookupSession {
                records: Arc::new(provider_records),
            }),
        ),
        exclude_ids: Arc::new(FxHashSet::default()),
    };
    let mut config = test_config();
    let dir = TempDir::new().expect("temp dir");
    config.directory = Arc::from(dir.path());
    config.state_db = Some(Arc::clone(&db) as Arc<dyn DownloadStore>);
    config.sync_mode = SyncMode::Incremental {
        zone_sync_token: "zone-token-prev".to_string(),
    };

    let result = download_photos_with_sync(
        &Client::new(),
        std::slice::from_ref(&pass),
        Arc::new(config),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("source-deleted capture batch");

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert_eq!(result.stats.metadata_capture_refreshed, 0);
    assert_eq!(result.stats.metadata_capture_failures, 0);
    assert_eq!(result.stats.metadata_capture_remaining, 1);
    assert!(result.stats.metadata_capture_progressed);
    assert_eq!(
        db.get_summary().await.expect("summary").source_deleted,
        u64::try_from(DELETED_ASSETS).expect("deleted count fits u64")
    );
}

#[tokio::test]
async fn current_capture_revision_adds_no_provider_lookup_requests() {
    #[derive(Clone, Debug)]
    struct CountingCaptureSession {
        lookups: Arc<AtomicUsize>,
        changes: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl PhotosSession for CountingCaptureSession {
        async fn post(
            &self,
            url: &str,
            _body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            if url.contains("/records/lookup?") {
                self.lookups.fetch_add(1, Ordering::SeqCst);
                return Ok(json!({"records": []}));
            }
            if url.contains("/changes/zone?") {
                self.changes.fetch_add(1, Ordering::SeqCst);
                return Ok(changes_zone_response(Vec::new(), "zone-token-next"));
            }
            Ok(json!({"records": [], "syncToken": "ignored-query-token"}))
        }

        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(self.clone())
        }
    }

    let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
    let dir = TempDir::new().expect("temp dir");
    let records = incremental_photo_records("CAPTURE_CURRENT");
    let asset = PhotoAsset::new(records[0].clone(), records[1].clone());
    let lookups = Arc::new(AtomicUsize::new(0));
    let changes = Arc::new(AtomicUsize::new(0));
    let pass = AlbumPass {
        kind: PassKind::Unfiled,
        album: album_with_session(
            "PrimarySync",
            "",
            Box::new(CountingCaptureSession {
                lookups: Arc::clone(&lookups),
                changes: Arc::clone(&changes),
            }),
        ),
        exclude_ids: Arc::new(FxHashSet::default()),
    };
    let mut config = test_config();
    config.directory = Arc::from(dir.path());
    config.state_db = Some(Arc::clone(&db) as Arc<dyn DownloadStore>);
    config.sync_mode = SyncMode::Incremental {
        zone_sync_token: "zone-token-prev".to_string(),
    };
    seed_downloaded_metadata_asset(db.as_ref(), &config, &pass, &asset).await;

    let result = download_photos_with_sync(
        &Client::new(),
        &[pass],
        Arc::new(config),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("current capture revision should use the normal incremental path");

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert_eq!(lookups.load(Ordering::SeqCst), 0);
    assert_eq!(changes.load(Ordering::SeqCst), 1);
    assert_eq!(result.stats.metadata_capture_refreshed, 0);
    assert_eq!(result.stats.metadata_capture_remaining, 0);
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn normal_sync_writes_configured_xmp_after_capture_revision_repair() {
    let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
    let dir = TempDir::new().expect("temp dir");
    let stored_records = incremental_photo_records_with_favorite("CAPTURE_XMP", false);
    let stored_asset = PhotoAsset::new(stored_records[0].clone(), stored_records[1].clone());
    let changed_records = incremental_photo_records_with_favorite("CAPTURE_XMP", true);
    let pass = AlbumPass {
        kind: PassKind::Unfiled,
        album: album_with_session(
            "PrimarySync",
            "",
            Box::new(PendingLookupSession {
                records: Arc::new(changed_records),
            }),
        ),
        exclude_ids: Arc::new(FxHashSet::default()),
    };
    let mut config = test_config();
    config.directory = Arc::from(dir.path());
    config.metadata.xmp_sidecar = true;
    config.state_db = Some(Arc::clone(&db) as Arc<dyn DownloadStore>);
    config.sync_mode = SyncMode::Incremental {
        zone_sync_token: "zone-token-prev".to_string(),
    };
    let media_path =
        seed_downloaded_metadata_asset(db.as_ref(), &config, &pass, &stored_asset).await;
    db.upsert_asset_master_mapping("PrimarySync", "asset-CAPTURE_XMP", "CAPTURE_XMP")
        .await
        .expect("seed durable provider identity");
    assert!(
        db.claim_legacy_master_state_owner("PrimarySync", "CAPTURE_XMP", "asset-CAPTURE_XMP",)
            .await
            .expect("seed legacy state owner")
    );
    db.set_metadata_capture_revision_for_test("PrimarySync", "CAPTURE_XMP", 0);

    let result = download_photos_with_sync(
        &Client::new(),
        &[pass],
        Arc::new(config),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("normal sync should repair metadata and drain the configured XMP write");

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert_eq!(result.stats.downloaded, 0);
    assert_eq!(result.stats.metadata_capture_refreshed, 1);
    let sidecar_name = format!(
        "{}.xmp",
        media_path
            .file_name()
            .expect("media path has filename")
            .to_string_lossy()
    );
    assert!(media_path.with_file_name(sidecar_name).exists());
    assert!(
        db.get_pending_metadata_rewrites(10)
            .await
            .expect("read rewrite queue")
            .is_empty()
    );
}

#[tokio::test]
async fn failed_capture_lookup_preserves_incremental_checkpoint_and_retries() {
    #[derive(Clone, Debug)]
    struct FailingCaptureLookupSession;

    #[async_trait::async_trait]
    impl PhotosSession for FailingCaptureLookupSession {
        async fn post(
            &self,
            url: &str,
            _body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            if url.contains("/records/lookup?") {
                anyhow::bail!("temporary lookup failure");
            }
            if url.contains("/changes/zone?") {
                return Ok(changes_zone_response(Vec::new(), "zone-token-next"));
            }
            Ok(json!({"records": [], "syncToken": "ignored-query-token"}))
        }

        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(self.clone())
        }
    }

    let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
    let dir = TempDir::new().expect("temp dir");
    let records = incremental_photo_records("CAPTURE_RETRY");
    let asset = PhotoAsset::new(records[0].clone(), records[1].clone());
    let pass = AlbumPass {
        kind: PassKind::Unfiled,
        album: album_with_session("PrimarySync", "", Box::new(FailingCaptureLookupSession)),
        exclude_ids: Arc::new(FxHashSet::default()),
    };
    let mut config = test_config();
    config.directory = Arc::from(dir.path());
    config.state_db = Some(Arc::clone(&db) as Arc<dyn DownloadStore>);
    config.sync_mode = SyncMode::Incremental {
        zone_sync_token: "zone-token-prev".to_string(),
    };
    seed_downloaded_metadata_asset(db.as_ref(), &config, &pass, &asset).await;
    db.upsert_asset_master_mapping("PrimarySync", "asset-CAPTURE_RETRY", "CAPTURE_RETRY")
        .await
        .unwrap();
    db.set_metadata_capture_revision_for_test("PrimarySync", "CAPTURE_RETRY", 0);

    let result = download_photos_with_sync(
        &Client::new(),
        &[pass],
        Arc::new(config),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("lookup failure should remain a reported durable repair");

    assert!(matches!(
        result.outcome,
        DownloadOutcome::PartialFailure { failed_count: 1 }
    ));
    assert_eq!(result.sync_token, None);
    assert!(result.stats.sync_token_blocked);
    assert_eq!(
        result.stats.sync_token_blocked_reason,
        Some(METADATA_CAPTURE_REPAIR_FAILED_REASON)
    );
    assert_eq!(result.stats.metadata_capture_failures, 1);
    assert_eq!(result.stats.metadata_capture_remaining, 1);
    assert!(
        db.has_metadata_capture_work(&["PrimarySync"], crate::state::METADATA_CAPTURE_REVISION,)
            .await
            .unwrap(),
        "failed lookup must leave durable retry work"
    );
}

#[tokio::test]
async fn interrupted_capture_repair_keeps_unprocessed_rows_pending_without_failure() {
    let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
    let dir = TempDir::new().expect("temp dir");
    let records = incremental_photo_records("CAPTURE_INTERRUPTED");
    let asset = PhotoAsset::new(records[0].clone(), records[1].clone());
    let pass = AlbumPass {
        kind: PassKind::Unfiled,
        album: album_with_session(
            "PrimarySync",
            "",
            Box::new(PendingLookupSession {
                records: Arc::new(records),
            }),
        ),
        exclude_ids: Arc::new(FxHashSet::default()),
    };
    let mut config = test_config();
    config.directory = Arc::from(dir.path());
    config.state_db = Some(Arc::clone(&db) as Arc<dyn DownloadStore>);
    seed_downloaded_metadata_asset(db.as_ref(), &config, &pass, &asset).await;
    db.upsert_asset_master_mapping(
        "PrimarySync",
        "asset-CAPTURE_INTERRUPTED",
        "CAPTURE_INTERRUPTED",
    )
    .await
    .expect("seed durable provider identity");
    db.set_metadata_capture_revision_for_test("PrimarySync", "CAPTURE_INTERRUPTED", 0);
    let shutdown = CancellationToken::new();
    shutdown.cancel();

    let repair = run_metadata_capture_repair(
        std::slice::from_ref(&pass),
        &config,
        DownloadControls::download_hidden(),
        &shutdown,
    )
    .await;

    assert!(repair.stats.interrupted);
    assert!(repair.stats.sync_token_blocked);
    assert_eq!(repair.stats.metadata_capture_refreshed, 0);
    assert_eq!(repair.stats.metadata_capture_failures, 0);
    assert_eq!(repair.stats.metadata_capture_remaining, 1);
    let summary = db.get_summary().await.expect("summary");
    let capture = summary
        .metadata_capture
        .iter()
        .find(|status| status.library == "PrimarySync")
        .expect("capture status");
    assert_eq!(capture.pending_revision, Some(1));
    assert_eq!(capture.failed_assets, 0);
    assert_eq!(capture.remaining_assets, 1);
}
