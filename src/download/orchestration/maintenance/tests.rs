//! Automatic historical repair tests were retired: normal sync preserves
//! existing metadata instead of rewriting it or holding provider checkpoints.
//! Explicit queue draining remains covered in metadata_rewrite/queued/tests.rs.
use super::super::dispatch::download_photos_with_sync;
use super::super::models::{DownloadControls, DownloadOutcome, DownloadStore, SyncMode};
use super::super::test_support::{
    album_with_session, changes_zone_response, incremental_photo_records,
    seed_downloaded_metadata_asset, test_config,
};
use crate::commands::{AlbumPass, PassKind};
use crate::icloud::photos::{PhotoAsset, PhotosSession};
use crate::state::SqliteStateDb;
use reqwest::Client;
use rustc_hash::FxHashSet;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
struct QuietSession;
#[async_trait::async_trait]
impl PhotosSession for QuietSession {
    async fn post(
        &self,
        url: &str,
        _body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        assert!(
            url.contains("/changes/zone?"),
            "normal sync must not hydrate historical metadata: {url}"
        );
        Ok(changes_zone_response(Vec::new(), "next-current-token"))
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

#[tokio::test]
async fn completed_legacy_capture_does_not_infer_historical_recovery_after_restart() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("state.db");
    let media_path = dir.path().join("historical.jpg");
    let overwritten_date = chrono::DateTime::from_timestamp(1_500_000_000, 0).unwrap();
    tokio::fs::write(&media_path, b"preserved historical bytes")
        .await
        .unwrap();
    {
        let db = SqliteStateDb::open(&db_path).await.unwrap();
        let record = crate::test_helpers::TestAssetRecord::new("historical-master")
            .added_at(overwritten_date)
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "historical-master",
            "original",
            &media_path,
            "local-checksum",
            None,
        )
        .await
        .unwrap();
        // Seed the durable result of an earlier bad claim. This deliberately
        // supplies an owner; it does not assert that ownership was proven.
        assert!(
            db.claim_legacy_master_state_owner("PrimarySync", "historical-master", "wrong-child")
                .await
                .unwrap()
        );
        for child in ["wrong-child", "historical-child"] {
            db.upsert_asset_master_mapping("PrimarySync", child, "historical-master")
                .await
                .unwrap();
        }
        db.set_metadata_capture_revision_for_test("PrimarySync", "historical-master", 1);
        db.set_metadata("sync_token:PrimarySync", "retained-token")
            .await
            .unwrap();
    }
    for _ in 0..2 {
        let db = Arc::new(SqliteStateDb::open(&db_path).await.unwrap());
        let mut config = test_config();
        config.directory = Arc::from(dir.path());
        config.state_db = Some(Arc::clone(&db) as Arc<dyn DownloadStore>);
        config.sync_mode = SyncMode::Incremental {
            zone_sync_token: "current-token".into(),
        };
        let pass = AlbumPass {
            kind: PassKind::Unfiled,
            album: album_with_session("PrimarySync", "", Box::new(QuietSession)),
            exclude_ids: Arc::new(FxHashSet::default()),
        };
        let result = download_photos_with_sync(
            &Client::new(),
            &[pass],
            Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert!(
            db.legacy_preparation_snapshots("PrimarySync", 64)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            db.legacy_preservations("PrimarySync")
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(result.stats.metadata_capture_refreshed, 0);
        assert_eq!(result.stats.metadata_capture_remaining, 0);
        let rows = db.get_downloaded_page(0, 10).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].added_at, Some(overwritten_date));
        assert_eq!(rows[0].local_path.as_deref(), Some(media_path.as_path()));
        let owners = db.get_legacy_master_state_owners().await.unwrap();
        assert_eq!(owners.len(), 1);
        assert!(owners.contains(&(
            "PrimarySync".to_string(),
            "historical-master".to_string(),
            "wrong-child".to_string(),
        )));
        assert_eq!(db.get_asset_master_mappings().await.unwrap().len(), 2);
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("retained-token")
        );
        assert_eq!(
            tokio::fs::read(&media_path).await.unwrap(),
            b"preserved historical bytes"
        );
    }
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
