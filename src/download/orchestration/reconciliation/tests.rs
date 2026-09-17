use std::sync::Arc;

use anyhow::Result;
use reqwest::Client;
use rustc_hash::FxHashSet;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

use crate::commands::{AlbumPass, PassKind};
use crate::download::{file, filter, paths};
use crate::icloud::photos::{PhotoAsset, PhotosSession};
use crate::state::{ReconciliationStateStore, SqliteStateDb, VersionSizeKey};
use crate::test_helpers::{
    MockPhotosFlow, TestAssetRecord, mock_photo_records_for_zone_with_filename,
};
use crate::types::AssetVersionSize;

use super::super::config::{DownloadConfig, hash_download_config};
use super::super::dispatch::download_photos_with_sync;
use super::super::incremental::download_photos_incremental_collecting;
use super::super::models::{
    DownloadControls, DownloadOutcome, DownloadReporting, DownloadRunMode, SyncMode, SyncResult,
};
use super::super::test_support::{
    PendingLookupSession, album_with_session, incremental_photo_records_with_url, mock_album,
    test_config,
};
use super::reconcile_catalog_paths;

#[tokio::test]
async fn import_refusal_preserves_reserved_retry_across_restart() {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};
    let server = crate::start_wiremock_or_skip!();
    let mut bytes = vec![3u8; 1024];
    bytes[..3].copy_from_slice(&[0xff, 0xd8, 0xff]);
    let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&bytes));
    Mock::given(method("GET"))
        .and(path("/media.jpg"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.clone()))
        .mount(&server)
        .await;
    let root = TempDir::new().unwrap();
    let db_path = root.path().join("state.db");
    let db = Arc::new(SqliteStateDb::open(&db_path).await.unwrap());
    let source = root.path().join("source.jpg");
    std::fs::write(&source, &bytes).unwrap();
    let local_checksum = file::compute_sha256(&source).await.unwrap();
    let records = |id| {
        let mut records = incremental_photo_records_with_url(
            id,
            "same.jpg",
            &format!("{}/media.jpg", server.uri()),
            1024,
        );
        records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(checksum);
        records
    };
    let a = records("A");
    db.upsert_seen(
        &TestAssetRecord::new("A")
            .filename("same.jpg")
            .checksum(&checksum)
            .size(1024)
            .build(),
    )
    .await
    .unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "A",
        "original",
        &source,
        &local_checksum,
        None,
    )
    .await
    .unwrap();
    db.upsert_asset_master_mapping("PrimarySync", "asset-A", "A")
        .await
        .unwrap();
    let passes = vec![AlbumPass {
        kind: PassKind::Unfiled,
        album: album_with_session(
            "PrimarySync",
            "",
            Box::new(PendingLookupSession {
                records: Arc::new(a),
            }),
        ),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];
    let mut config = test_config();
    config.directory = Arc::from(root.path().join("new"));
    std::fs::create_dir_all(&config.directory).unwrap();
    config.state_db = Some(db.clone());
    let reconciled =
        reconcile_catalog_paths(&passes, Arc::new(config.clone()), CancellationToken::new())
            .await
            .unwrap();
    assert!(reconciled.complete, "{reconciled:?}");
    let rows = db.get_downloaded_page(0, 10).await.unwrap();
    let reserved = rows[0].local_path.clone().unwrap();
    let b = records("B");
    let asset_b =
        PhotoAsset::new(b[0].clone(), b[1].clone()).with_state_record_name(Arc::from("B"));
    let (tx, rx) = tokio::sync::oneshot::channel();
    drop(tx);
    let stats = crate::commands::import_assets(
        futures_util::stream::iter(vec![Ok::<_, anyhow::Error>(asset_b)]),
        rx,
        db.as_ref(),
        &config,
        "PrimarySync",
        &mut paths::DirCache::new(),
        crate::commands::ImportRunOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(stats.total, 1);
    assert_eq!(stats.unmatched, 0);
    assert_eq!(stats.filtered, 0);
    assert_eq!(stats.hash_errors, 0);
    assert_eq!(stats.matched, 0);
    let rows = db.get_downloaded_page(0, 10).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(db.get_summary().await.unwrap().total_assets, 1);
    assert!(
        rows.iter()
            .all(|row| row.local_path.as_ref() == Some(&reserved))
    );
    std::fs::remove_file(&reserved).unwrap();
    db.mark_failed("PrimarySync", "A", "original", "local file missing")
        .await
        .unwrap();
    config.state_db = None;
    drop(db);
    for cycle in 0..2 {
        config.state_db = None;
        config.state_db = Some(Arc::new(SqliteStateDb::open(&db_path).await.unwrap()));
        let repaired = download_photos_with_sync(
            &Client::new(),
            &passes,
            Arc::new(config.clone()),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(
            matches!(repaired.outcome, DownloadOutcome::Success),
            "{repaired:?}"
        );
        assert_eq!(repaired.stats.downloaded, usize::from(cycle == 0));
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
        let rows = config
            .state_db
            .as_ref()
            .unwrap()
            .get_downloaded_page(0, 10)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].local_path.as_ref(), Some(&reserved));
        assert_eq!(std::fs::read(&reserved).unwrap(), bytes);
    }
    assert_eq!(std::fs::read(&source).unwrap(), bytes);
    assert_eq!(std::fs::read(&reserved).unwrap(), bytes);
}

#[cfg(unix)]
#[tokio::test]
async fn path_reconciliation_rejects_leaf_symlink_then_recovers() {
    path_reconciliation_rejects_unsafe_entry_then_recovers(ReconciliationUnsafeEntry::Leaf).await;
}

#[cfg(unix)]
#[tokio::test]
async fn path_reconciliation_rejects_parent_symlinks_then_recovers() {
    for entry in [
        ReconciliationUnsafeEntry::DestinationParent,
        ReconciliationUnsafeEntry::SourceParent,
    ] {
        path_reconciliation_rejects_unsafe_entry_then_recovers(entry).await;
    }
}

#[cfg(unix)]
#[tokio::test]
async fn path_reconciliation_rejects_parent_components_then_recovers() {
    let old_dir = TempDir::new().unwrap();
    let new_dir = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let db_path = old_dir.path().join("state.db");
    let db = Arc::new(crate::state::SqliteStateDb::open(&db_path).await.unwrap());
    let old_path = old_dir.path().join("reconcile.jpg");
    let bytes = vec![7u8; 1024];
    std::fs::write(&old_path, &bytes).unwrap();
    let checksum = file::compute_sha256(&old_path).await.unwrap();
    let record = crate::test_helpers::TestAssetRecord::new("PARENT_COMPONENT")
        .filename("reconcile.jpg")
        .checksum("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
        .size(1024)
        .build();
    db.upsert_seen(&record).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "PARENT_COMPONENT",
        "original",
        &old_path,
        &checksum,
        None,
    )
    .await
    .unwrap();
    db.upsert_asset_master_mapping("PrimarySync", "asset-PARENT_COMPONENT", "PARENT_COMPONENT")
        .await
        .unwrap();
    db.set_metadata("sync_token:PrimarySync", "before-reconciliation")
        .await
        .unwrap();
    let passes = vec![AlbumPass {
        kind: PassKind::Unfiled,
        album: album_with_session(
            "PrimarySync",
            "",
            Box::new(PendingLookupSession {
                records: Arc::new(mock_photo_records_for_zone_with_filename(
                    "PARENT_COMPONENT",
                    "PrimarySync",
                    "reconcile.jpg",
                )),
            }),
        ),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];
    let external_root = outside.path().join("photos");
    let lexical_root = new_dir.path().join("photos");
    std::fs::create_dir_all(outside.path().join("child")).unwrap();
    std::fs::create_dir(&external_root).unwrap();
    std::fs::create_dir(&lexical_root).unwrap();
    std::fs::write(external_root.join("user.jpg"), b"external user bytes").unwrap();
    std::fs::write(lexical_root.join("user.jpg"), b"local user bytes").unwrap();
    std::os::unix::fs::symlink(outside.path().join("child"), new_dir.path().join("link")).unwrap();
    let mut config = test_config();
    config.directory = Arc::from(new_dir.path().join("link/../photos"));
    config.state_db = Some(db.clone());
    let config = Arc::new(config);
    for _ in 0..2 {
        let rejected =
            reconcile_catalog_paths(&passes, Arc::clone(&config), CancellationToken::new())
                .await
                .unwrap();
        assert!(!rejected.complete);
        assert_eq!(rejected.stats.failed, 1);
        assert_eq!(rejected.stats.downloaded, 0);
        let reopened = crate::state::SqliteStateDb::open(&db_path).await.unwrap();
        let rows = reopened.get_downloaded_page(0, 10).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].local_path.as_deref(), Some(old_path.as_path()));
        assert_eq!(rows[0].local_checksum.as_deref(), Some(checksum.as_str()));
        assert_eq!(
            reopened
                .get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("before-reconciliation")
        );
        assert_eq!(std::fs::read_dir(&lexical_root).unwrap().count(), 1);
        assert_eq!(std::fs::read_dir(&external_root).unwrap().count(), 1);
        assert_eq!(
            std::fs::read(lexical_root.join("user.jpg")).unwrap(),
            b"local user bytes"
        );
        assert_eq!(
            std::fs::read(external_root.join("user.jpg")).unwrap(),
            b"external user bytes"
        );
        assert_eq!(std::fs::read(&old_path).unwrap(), bytes);
    }
    // Use the intended root directly instead of the ambiguous spelling.
    let mut recovered = (*config).clone();
    recovered.directory = Arc::from(external_root.as_path());
    let recovered = Arc::new(recovered);
    let first = reconcile_catalog_paths(&passes, Arc::clone(&recovered), CancellationToken::new())
        .await
        .unwrap();
    assert!(first.complete);
    assert_eq!(first.stats.downloaded, 1);
    let reopened = crate::state::SqliteStateDb::open(&db_path).await.unwrap();
    let rows = reopened.get_downloaded_page(0, 10).await.unwrap();
    let recorded = rows[0].local_path.as_ref().unwrap();
    assert!(recorded.starts_with(&external_root));
    assert_eq!(std::fs::read(recorded).unwrap(), bytes);
    let steady = reconcile_catalog_paths(&passes, recovered, CancellationToken::new())
        .await
        .unwrap();
    assert!(steady.complete);
    assert_eq!(steady.stats.downloaded, 0);
    let rows_after = reopened.get_downloaded_page(0, 10).await.unwrap();
    assert_eq!(rows_after[0].local_path, rows[0].local_path);
    assert_eq!(std::fs::read(&old_path).unwrap(), bytes);
    assert_eq!(std::fs::read_dir(&lexical_root).unwrap().count(), 1);
}

#[cfg(unix)]
enum ReconciliationUnsafeEntry {
    Leaf,
    DestinationParent,
    SourceParent,
}

#[cfg(unix)]
async fn path_reconciliation_rejects_unsafe_entry_then_recovers(entry: ReconciliationUnsafeEntry) {
    #[derive(Clone, Debug)]
    struct LookupOnlySession {
        records: Arc<Vec<Value>>,
    }

    #[async_trait::async_trait]
    impl PhotosSession for LookupOnlySession {
        async fn post(
            &self,
            url: &str,
            _body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            if url.contains("/records/lookup?") {
                return Ok(json!({"records": self.records.as_ref().clone()}));
            }
            anyhow::bail!("path reconciliation made an unexpected provider request: {url}")
        }

        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(self.clone())
        }
    }

    let old_dir = TempDir::new().expect("old dir");
    let db_path = old_dir.path().join("state.db");
    let db = Arc::new(
        crate::state::SqliteStateDb::open(&db_path)
            .await
            .expect("state db"),
    );
    let new_dir = TempDir::new().expect("new dir");
    let old_path = old_dir.path().join("source/reconcile.jpg");
    std::fs::create_dir_all(old_path.parent().unwrap()).unwrap();
    tokio::fs::write(&old_path, vec![0u8; 1024]).await.unwrap();
    let local_checksum = file::compute_sha256(&old_path).await.unwrap();
    let record = crate::test_helpers::TestAssetRecord::new("RECONCILE")
        .filename("reconcile.jpg")
        .checksum("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
        .size(1024)
        .build();
    db.upsert_seen(&record).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "RECONCILE",
        "original",
        &old_path,
        &local_checksum,
        Some("provider-checksum"),
    )
    .await
    .unwrap();
    db.upsert_asset_master_mapping("PrimarySync", "asset-RECONCILE", "RECONCILE")
        .await
        .unwrap();

    let mut records =
        mock_photo_records_for_zone_with_filename("RECONCILE", "PrimarySync", "reconcile.jpg");
    records[1]["fields"]["resJPEGFullRes"] = json!({"value": {
        "downloadURL": "https://p01.icloud-content.com/reconcile-adjusted.jpg",
        "size": 512,
        "fileChecksum": "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB="
    }});
    records[1]["fields"]["resJPEGFullFileType"] = json!({"value": "public.jpeg"});
    let asset = PhotoAsset::new(records[0].clone(), records[1].clone());
    let passes = vec![AlbumPass {
        kind: PassKind::Unfiled,
        album: album_with_session(
            "PrimarySync",
            "",
            Box::new(LookupOnlySession {
                records: Arc::new(records),
            }),
        ),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];
    let mut config = test_config();
    config.directory = Arc::from(new_dir.path());
    config.state_db = Some(db.clone());
    config.edited = true;
    let expected_paths = filter::expected_paths_for(&asset, &config);
    let expected_path = expected_paths
        .into_iter()
        .find(|path| path.version_size == VersionSizeKey::Original)
        .unwrap()
        .path;
    let external = TempDir::new().unwrap();
    let target = external.path().join("target.jpg");
    std::fs::write(&target, vec![0u8; 1024]).unwrap();
    std::fs::create_dir_all(expected_path.parent().unwrap()).unwrap();
    let (unsafe_path, link_target) = match entry {
        ReconciliationUnsafeEntry::Leaf => (expected_path.clone(), target.clone()),
        ReconciliationUnsafeEntry::DestinationParent => {
            let parent = expected_path.parent().unwrap().to_path_buf();
            std::fs::remove_dir(&parent).unwrap();
            (parent, external.path().to_path_buf())
        }
        ReconciliationUnsafeEntry::SourceParent => {
            let parent = old_path.parent().unwrap().to_path_buf();
            let retained = external.path().join("retained-source");
            std::fs::rename(&parent, &retained).unwrap();
            (parent, retained)
        }
    };
    std::os::unix::fs::symlink(&link_target, &unsafe_path).unwrap();
    let external_entries = std::fs::read_dir(external.path()).unwrap().count();
    let config = Arc::new(config);
    for _ in 0..2 {
        let result =
            reconcile_catalog_paths(&passes, Arc::clone(&config), CancellationToken::new())
                .await
                .unwrap();
        assert!(!result.complete);
        assert_eq!(result.stats.downloaded, 0);
        assert_eq!(result.stats.failed, 1);
        let reopened = crate::state::SqliteStateDb::open(&db_path).await.unwrap();
        let rows = reopened.get_downloaded_page(0, 10).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].local_path.as_deref(), Some(old_path.as_path()));
        assert_eq!(
            rows[0].local_checksum.as_deref(),
            Some(local_checksum.as_str())
        );
        assert_eq!(std::fs::read_link(&unsafe_path).unwrap(), link_target);
        assert_eq!(std::fs::read(&target).unwrap(), vec![0u8; 1024]);
    }
    assert_eq!(
        std::fs::read_dir(external.path()).unwrap().count(),
        external_entries
    );
    std::fs::remove_file(&unsafe_path).unwrap();
    if matches!(entry, ReconciliationUnsafeEntry::SourceParent) {
        std::fs::rename(&link_target, &unsafe_path).unwrap();
    }
    let repaired = reconcile_catalog_paths(&passes, Arc::clone(&config), CancellationToken::new())
        .await
        .unwrap();
    assert!(repaired.complete);
    assert_eq!(repaired.stats.downloaded, 1);
    let steady = reconcile_catalog_paths(&passes, config, CancellationToken::new())
        .await
        .unwrap();
    assert!(steady.complete);
    assert_eq!(steady.stats.downloaded, 0);
    let reopened = crate::state::SqliteStateDb::open(&db_path).await.unwrap();
    let rows = reopened.get_downloaded_page(0, 10).await.unwrap();
    assert_eq!(rows[0].local_path.as_deref(), Some(expected_path.as_path()));
    assert_eq!(std::fs::read(&old_path).unwrap(), vec![0u8; 1024]);
    assert_eq!(std::fs::read(&expected_path).unwrap(), vec![0u8; 1024]);
    assert_eq!(std::fs::read(&target).unwrap(), vec![0u8; 1024]);
}

#[derive(Clone, Copy)]
enum ReservedDownloadPass {
    Full,
    IncrementalStreaming,
    IncrementalCollecting,
    Pending,
    PendingRecorded,
}

#[derive(Clone, Copy)]
enum ReservedDownloadFailure {
    None,
    Finalization,
    Reservation,
}

#[tokio::test]
async fn full_download_preserves_rendition_reservations() {
    Box::pin(assert_reserved_download_transition(
        ReservedDownloadPass::Full,
        ReservedDownloadFailure::None,
    ))
    .await;
}

#[tokio::test]
async fn incremental_streaming_preserves_rendition_reservations() {
    Box::pin(assert_reserved_download_transition(
        ReservedDownloadPass::IncrementalStreaming,
        ReservedDownloadFailure::None,
    ))
    .await;
}

#[tokio::test]
async fn incremental_collecting_preserves_rendition_reservations() {
    Box::pin(assert_reserved_download_transition(
        ReservedDownloadPass::IncrementalCollecting,
        ReservedDownloadFailure::None,
    ))
    .await;
}

#[tokio::test]
async fn pending_retry_reservations_survive_finalization_failure() {
    Box::pin(assert_reserved_download_transition(
        ReservedDownloadPass::Pending,
        ReservedDownloadFailure::Finalization,
    ))
    .await;
}

#[tokio::test]
async fn pending_retry_reservation_failure_prevents_publication() {
    Box::pin(assert_reserved_download_transition(
        ReservedDownloadPass::Pending,
        ReservedDownloadFailure::Reservation,
    ))
    .await;
}

#[tokio::test]
async fn full_download_reservation_failure_prevents_publication() {
    Box::pin(assert_reserved_download_transition(
        ReservedDownloadPass::Full,
        ReservedDownloadFailure::Reservation,
    ))
    .await;
}

#[tokio::test]
async fn incremental_collecting_reservation_failure_prevents_publication() {
    Box::pin(assert_reserved_download_transition(
        ReservedDownloadPass::IncrementalCollecting,
        ReservedDownloadFailure::Reservation,
    ))
    .await;
}

#[tokio::test]
async fn pending_retry_reservations_preserve_final_recorded_override() {
    Box::pin(assert_reserved_download_transition(
        ReservedDownloadPass::PendingRecorded,
        ReservedDownloadFailure::Finalization,
    ))
    .await;
}

#[tokio::test]
async fn reserved_content_update_full() {
    Box::pin(assert_reserved_content_update(
        ReservedDownloadPass::Full,
        2048,
        ReservedDownloadFailure::None,
    ))
    .await;
}

#[tokio::test]
async fn reserved_content_update_streaming() {
    Box::pin(assert_reserved_content_update(
        ReservedDownloadPass::IncrementalStreaming,
        2048,
        ReservedDownloadFailure::None,
    ))
    .await;
}

#[tokio::test]
async fn reserved_content_update_collecting() {
    Box::pin(assert_reserved_content_update(
        ReservedDownloadPass::IncrementalCollecting,
        2048,
        ReservedDownloadFailure::None,
    ))
    .await;
}

#[tokio::test]
async fn reserved_content_update_same_size() {
    Box::pin(assert_reserved_content_update(
        ReservedDownloadPass::IncrementalStreaming,
        1024,
        ReservedDownloadFailure::None,
    ))
    .await;
}

#[tokio::test]
async fn reserved_content_update_finalization_retry() {
    Box::pin(assert_reserved_content_update(
        ReservedDownloadPass::Full,
        2048,
        ReservedDownloadFailure::Finalization,
    ))
    .await;
}

async fn assert_reserved_content_update(
    pass: ReservedDownloadPass,
    updated_size: usize,
    failure: ReservedDownloadFailure,
) {
    let mode = if matches!(pass, ReservedDownloadPass::Full) {
        SyncMode::Full
    } else {
        SyncMode::Incremental {
            zone_sync_token: "before".into(),
        }
    };
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};
    let server = crate::start_wiremock_or_skip!();
    let mut bytes = vec![3u8; 1024];
    bytes[..3].copy_from_slice(&[0xff, 0xd8, 0xff]);
    let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&bytes));
    Mock::given(method("GET"))
        .and(path("/new.jpg"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.clone()))
        .mount(&server)
        .await;
    let root = TempDir::new().unwrap();
    let db_path = root.path().join("state.db");
    let db = Arc::new(SqliteStateDb::open(&db_path).await.unwrap());
    let source = root.path().join("reserved.jpg");
    std::fs::write(&source, vec![1u8; 1024]).unwrap();
    let source_checksum = file::compute_sha256(&source).await.unwrap();
    let mut a = mock_photo_records_for_zone_with_filename("A", "PrimarySync", "reserved.jpg");
    a[1]["fields"]["resJPEGFullRes"] = json!({"value": {
        "downloadURL": "https://p01.icloud-content.com/edited.jpg", "size": 1024,
        "fileChecksum": "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB="
    }});
    a[1]["fields"]["resJPEGFullFileType"] = json!({"value": "public.jpeg"});
    let asset_a = PhotoAsset::new(a[0].clone(), a[1].clone());
    let mut b = incremental_photo_records_with_url(
        "B",
        "reserved_edited.JPG",
        &format!("{}/new.jpg", server.uri()),
        1024,
    );
    b[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(checksum);
    b[1]["fields"]["resJPEGFullRes"] = json!({"value": {
        "downloadURL": format!("{}/edited.jpg", server.uri()), "size": 1024,
        "fileChecksum": checksum
    }});
    b[1]["fields"]["resJPEGFullFileType"] = json!({"value": "public.jpeg"});
    Mock::given(method("GET"))
        .and(path("/edited.jpg"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.clone()))
        .mount(&server)
        .await;
    db.upsert_seen(
        &TestAssetRecord::new("A")
            .filename("reserved.jpg")
            .checksum("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
            .size(1024)
            .build(),
    )
    .await
    .unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "A",
        "original",
        &source,
        &source_checksum,
        None,
    )
    .await
    .unwrap();
    db.upsert_asset_master_mapping("PrimarySync", "asset-A", "A")
        .await
        .unwrap();
    db.set_metadata("sync_token:PrimarySync", "before")
        .await
        .unwrap();
    let lookup_pass = |records| {
        vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: album_with_session(
                "PrimarySync",
                "",
                Box::new(PendingLookupSession {
                    records: Arc::new(records),
                }),
            ),
            exclude_ids: Arc::new(FxHashSet::default()),
        }]
    };
    let mut config = test_config();
    config.directory = Arc::from(root.path().join("new"));
    std::fs::create_dir_all(&config.directory).unwrap();
    config.edited = true;
    config.state_db = Some(db.clone());
    config.sync_mode = mode;
    let reserved = filter::expected_paths_for(&asset_a, &config)
        .into_iter()
        .find(|item| item.version_size != VersionSizeKey::Original)
        .unwrap()
        .path;
    let first = reconcile_catalog_paths(
        &lookup_pass(a.clone()),
        Arc::new(config.clone()),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(first.complete, "initial reconciliation: {first:?}");
    assert_eq!(first.stats.downloaded, 1);
    assert!(!reserved.exists());
    assert!(
        db.get_pending().await.unwrap().is_empty(),
        "B must not enter through pending recovery"
    );
    let downloaded = run_reserved_download_cycle(
        pass,
        b.clone(),
        Arc::new(config.clone()),
        DownloadRunMode::Download,
    )
    .await
    .unwrap();
    assert!(
        matches!(downloaded.outcome, DownloadOutcome::Success),
        "{downloaded:?}"
    );
    assert_eq!(downloaded.stats.downloaded, 2);
    let initial = db.get_downloaded_page(0, 10).await.unwrap();
    let old_path = initial
        .iter()
        .find(|row| row.id.as_ref() != "A" && row.version_size == VersionSizeKey::Adjusted)
        .unwrap()
        .local_path
        .clone()
        .unwrap();
    let original_b = b.clone();
    let mut updated = vec![4u8; updated_size];
    updated[..3].copy_from_slice(&[0xff, 0xd8, 0xff]);
    let updated_checksum =
        base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&updated));
    Mock::given(method("GET"))
        .and(path("/updated.jpg"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(updated.clone()))
        .mount(&server)
        .await;
    b[1]["fields"]["resJPEGFullRes"] = json!({"value": {
        "downloadURL": format!("{}/updated.jpg", server.uri()), "size": updated_size,
        "fileChecksum": updated_checksum
    }});
    let db = Arc::new(SqliteStateDb::open(&db_path).await.unwrap());
    config.state_db = Some(db.clone());
    let original_reservations = db.get_reconciliation_reservations().await.unwrap();
    if matches!(failure, ReservedDownloadFailure::Finalization) {
        db.acquire_lock("inject generation finalization failure").unwrap().execute_batch(
            "CREATE TEMP TRIGGER fail_generation BEFORE UPDATE OF status ON assets WHEN NEW.id = 'asset-B' AND NEW.version_size = 'adjusted' AND NEW.status = 'downloaded' BEGIN SELECT RAISE(FAIL, 'injected finalization failure'); END;"
        ).unwrap();
    }
    let changed = run_reserved_download_cycle(
        pass,
        b.clone(),
        Arc::new(config.clone()),
        DownloadRunMode::Download,
    )
    .await
    .unwrap();
    let reservations = db.get_reconciliation_reservations().await.unwrap();
    assert_eq!(reservations.len(), original_reservations.len() + 1);
    assert!(
        original_reservations
            .iter()
            .all(|old| reservations.contains(old))
    );
    let choice = reservations
        .iter()
        .find(|r| {
            r.asset_id.as_ref() == "asset-B"
                && r.version_size == VersionSizeKey::Adjusted
                && r.content
                    .as_ref()
                    .is_some_and(|c| c.checksum.as_ref() == updated_checksum)
        })
        .unwrap();
    let new_path = choice.destination_path.clone();
    assert_ne!(new_path, old_path);
    assert_eq!(std::fs::read(&new_path).unwrap(), updated);
    assert_eq!(std::fs::read(&old_path).unwrap(), bytes);
    let request_count = server.received_requests().await.unwrap().len();
    assert_eq!(request_count, 3);
    if matches!(failure, ReservedDownloadFailure::Finalization) {
        assert!(changed.stats.state_write_failures > 0, "{changed:?}");
        let mut retryable = db.get_pending().await.unwrap();
        retryable.extend(db.get_failed().await.unwrap());
        assert!(retryable.iter().any(|r| r.id.as_ref() == "asset-B"
            && r.version_size == VersionSizeKey::Adjusted
            && r.checksum.as_ref() == updated_checksum));
        db.acquire_lock("remove generation failure")
            .unwrap()
            .execute_batch("DROP TRIGGER fail_generation")
            .unwrap();
    } else {
        assert!(
            matches!(changed.outcome, DownloadOutcome::Success),
            "{changed:?}"
        );
        assert_eq!(changed.stats.downloaded, 1);
    }
    config.state_db = Some(Arc::new(SqliteStateDb::open(&db_path).await.unwrap()));
    // Targeted recovery must adopt the new reserved sibling, not the old
    // catalog path, after publication succeeded but finalization failed.
    let restarted = run_reserved_download_cycle(
        ReservedDownloadPass::Pending,
        b.clone(),
        Arc::new(config.clone()),
        DownloadRunMode::Download,
    )
    .await
    .unwrap();
    assert!(
        matches!(restarted.outcome, DownloadOutcome::Success),
        "{restarted:?}"
    );
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        request_count
    );
    let rows = config
        .state_db
        .as_ref()
        .unwrap()
        .get_downloaded_page(0, 10)
        .await
        .unwrap();
    let row = rows
        .iter()
        .find(|r| r.id.as_ref() == "asset-B" && r.version_size == VersionSizeKey::Adjusted)
        .unwrap();
    assert_eq!(row.local_path.as_ref(), Some(&new_path));
    assert_eq!(row.checksum.as_ref(), updated_checksum);
    assert_eq!(row.size_bytes, updated_size as u64);
    assert_eq!(
        config
            .state_db
            .as_ref()
            .unwrap()
            .get_reconciliation_reservations()
            .await
            .unwrap()
            .len(),
        reservations.len()
    );
    let count = std::fs::read_dir(new_path.parent().unwrap())
        .unwrap()
        .count();
    let steady =
        run_reserved_download_cycle(pass, b, Arc::new(config.clone()), DownloadRunMode::Download)
            .await
            .unwrap();
    assert!(
        matches!(steady.outcome, DownloadOutcome::Success),
        "{steady:?}"
    );
    assert_eq!(steady.stats.downloaded, 0);
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        request_count
    );
    assert_eq!(
        std::fs::read_dir(new_path.parent().unwrap())
            .unwrap()
            .count(),
        count
    );
    assert_eq!(std::fs::read(&new_path).unwrap(), updated);
    assert_eq!(std::fs::read(&old_path).unwrap(), bytes);
    // A provider revert reuses the first generation's immutable choice;
    // it must not discard the newer generation's reservation or bytes.
    let reverted = run_reserved_download_cycle(
        pass,
        original_b,
        Arc::new(config.clone()),
        DownloadRunMode::Download,
    )
    .await
    .unwrap();
    assert!(
        matches!(reverted.outcome, DownloadOutcome::Success),
        "{reverted:?}"
    );
    let rows = config
        .state_db
        .as_ref()
        .unwrap()
        .get_downloaded_page(0, 10)
        .await
        .unwrap();
    let row = rows
        .iter()
        .find(|r| r.id.as_ref() == "asset-B" && r.version_size == VersionSizeKey::Adjusted)
        .unwrap();
    assert_eq!(row.local_path.as_ref(), Some(&old_path));
    assert_eq!(row.checksum.as_ref(), checksum);
    assert_eq!(
        config
            .state_db
            .as_ref()
            .unwrap()
            .get_reconciliation_reservations()
            .await
            .unwrap()
            .len(),
        reservations.len()
    );
    assert_eq!(
        std::fs::read_dir(new_path.parent().unwrap())
            .unwrap()
            .count(),
        count
    );
    assert_eq!(std::fs::read(&new_path).unwrap(), updated);
    assert_eq!(std::fs::read(&old_path).unwrap(), bytes);
}

async fn run_reserved_download_cycle(
    pass: ReservedDownloadPass,
    records: Vec<serde_json::Value>,
    config: Arc<DownloadConfig>,
    run_mode: DownloadRunMode,
) -> Result<SyncResult> {
    let album = match pass {
        ReservedDownloadPass::Full => mock_album(
            "",
            MockPhotosFlow::new()
                .query_page(records, Some("after"))
                .build(),
        ),
        ReservedDownloadPass::Pending | ReservedDownloadPass::PendingRecorded => {
            album_with_session(
                "PrimarySync",
                "",
                Box::new(PendingLookupSession {
                    records: Arc::new(records),
                }),
            )
        }
        _ => mock_album(
            "",
            MockPhotosFlow::new()
                .changes_zone_page(records, "after", false)
                .build(),
        ),
    };
    let passes = vec![AlbumPass {
        kind: PassKind::Unfiled,
        album,
        exclude_ids: Arc::new(FxHashSet::default()),
    }];
    if matches!(pass, ReservedDownloadPass::IncrementalCollecting) {
        download_photos_incremental_collecting(
            &Client::new(),
            &passes,
            &config,
            "before",
            DownloadControls::new(run_mode, DownloadReporting::hidden()),
            CancellationToken::new(),
        )
        .await
    } else {
        download_photos_with_sync(
            &Client::new(),
            &passes,
            config,
            DownloadControls::new(run_mode, DownloadReporting::hidden()),
            CancellationToken::new(),
        )
        .await
    }
}

async fn assert_reserved_download_transition(
    pass: ReservedDownloadPass,
    failure: ReservedDownloadFailure,
) {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};
    let server = crate::start_wiremock_or_skip!();
    let mut bytes = vec![3u8; 1024];
    bytes[..3].copy_from_slice(&[0xff, 0xd8, 0xff]);
    let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&bytes));
    Mock::given(method("GET"))
        .and(path("/new.jpg"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.clone()))
        .mount(&server)
        .await;
    let root = TempDir::new().unwrap();
    let db_path = root.path().join("state.db");
    let db = Arc::new(SqliteStateDb::open(&db_path).await.unwrap());
    let source = root.path().join("reserved.jpg");
    std::fs::write(&source, vec![1u8; 1024]).unwrap();
    let source_checksum = file::compute_sha256(&source).await.unwrap();
    let mut a = mock_photo_records_for_zone_with_filename("A", "PrimarySync", "reserved.jpg");
    a[1]["fields"]["resJPEGFullRes"] = json!({"value": {
        "downloadURL": "https://p01.icloud-content.com/edited.jpg", "size": 1024,
        "fileChecksum": "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB="
    }});
    a[1]["fields"]["resJPEGFullFileType"] = json!({"value": "public.jpeg"});
    let asset_a = PhotoAsset::new(a[0].clone(), a[1].clone());
    let mut b = incremental_photo_records_with_url(
        "B",
        "reserved_edited.JPG",
        &format!("{}/new.jpg", server.uri()),
        1024,
    );
    b[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(checksum);
    db.upsert_seen(
        &TestAssetRecord::new("A")
            .filename("reserved.jpg")
            .checksum("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
            .size(1024)
            .build(),
    )
    .await
    .unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "A",
        "original",
        &source,
        &source_checksum,
        None,
    )
    .await
    .unwrap();
    db.upsert_asset_master_mapping("PrimarySync", "asset-A", "A")
        .await
        .unwrap();
    db.set_metadata("sync_token:PrimarySync", "before")
        .await
        .unwrap();
    let lookup_pass = |records| {
        vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: album_with_session(
                "PrimarySync",
                "",
                Box::new(PendingLookupSession {
                    records: Arc::new(records),
                }),
            ),
            exclude_ids: Arc::new(FxHashSet::default()),
        }]
    };
    let mut config = test_config();
    config.directory = Arc::from(root.path().join("new"));
    std::fs::create_dir_all(&config.directory).unwrap();
    config.edited = true;
    config.state_db = Some(db.clone());
    config.sync_mode = match pass {
        ReservedDownloadPass::Full => SyncMode::Full,
        _ => SyncMode::Incremental {
            zone_sync_token: "before".into(),
        },
    };
    let reserved = filter::expected_paths_for(&asset_a, &config)
        .into_iter()
        .find(|item| item.version_size != VersionSizeKey::Original)
        .unwrap()
        .path;
    let first = reconcile_catalog_paths(
        &lookup_pass(a.clone()),
        Arc::new(config.clone()),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(first.complete, "initial reconciliation: {first:?}");
    assert_eq!(first.stats.downloaded, 1);
    assert!(!reserved.exists());
    assert!(
        db.get_pending().await.unwrap().is_empty(),
        "new assets must not enter through pending recovery"
    );
    if matches!(
        pass,
        ReservedDownloadPass::Pending | ReservedDownloadPass::PendingRecorded
    ) {
        db.upsert_seen(
            &TestAssetRecord::new("B")
                .filename("reserved_edited.JPG")
                .checksum(&checksum)
                .size(1024)
                .build(),
        )
        .await
        .unwrap();
        db.upsert_asset_master_mapping("PrimarySync", "asset-B", "B")
            .await
            .unwrap();
    }
    let recorded = reserved.with_file_name(paths::insert_asset_identity_suffix(
        "reserved_edited.JPG",
        "B",
    ));
    if matches!(pass, ReservedDownloadPass::PendingRecorded) {
        std::fs::write(&recorded, vec![9u8; 1024]).unwrap();
        let old_checksum = file::compute_sha256(&recorded).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "B",
            "original",
            &recorded,
            &old_checksum,
            None,
        )
        .await
        .unwrap();
        db.mark_failed(
            "PrimarySync",
            "B",
            "original",
            "retry with a conflicting recorded path",
        )
        .await
        .unwrap();
    }
    let parent = reserved.parent().unwrap();
    let initial_file_count = std::fs::read_dir(parent).unwrap().count();
    let original_reservations = db.get_reconciliation_reservations().await.unwrap();
    assert!(
        original_reservations
            .iter()
            .all(|reservation| reservation.asset_id.as_ref() == "A")
    );
    if matches!(pass, ReservedDownloadPass::Pending) {
        for run_mode in [DownloadRunMode::DryRun, DownloadRunMode::PrintFilenames] {
            let preview =
                run_reserved_download_cycle(pass, b.clone(), Arc::new(config.clone()), run_mode)
                    .await
                    .unwrap();
            assert_eq!(preview.stats.downloaded, usize::from(run_mode.is_dry_run()));
            assert_eq!(
                std::fs::read_dir(parent).unwrap().count(),
                initial_file_count
            );
            assert!(server.received_requests().await.unwrap().is_empty());
            assert_eq!(
                db.get_reconciliation_reservations().await.unwrap(),
                original_reservations
            );
        }
    }
    match failure {
        ReservedDownloadFailure::Finalization => db.acquire_lock("inject finalization failure").unwrap().execute_batch(
            "CREATE TEMP TRIGGER fail_reserved_download BEFORE UPDATE OF status ON assets WHEN NEW.id = 'B' AND NEW.status = 'downloaded' BEGIN SELECT RAISE(FAIL, 'injected finalization failure'); END;"
        ).unwrap(),
        ReservedDownloadFailure::Reservation => db.acquire_lock("inject reservation failure").unwrap().execute_batch(
            "CREATE TEMP TRIGGER fail_reserved_download BEFORE INSERT ON reconciliation_paths WHEN NEW.id != 'A' BEGIN SELECT RAISE(FAIL, 'injected reservation failure'); END;"
        ).unwrap(),
        ReservedDownloadFailure::None => {},
    }
    let downloaded = run_reserved_download_cycle(
        pass,
        b.clone(),
        Arc::new(config.clone()),
        DownloadRunMode::Download,
    )
    .await;
    match failure {
        ReservedDownloadFailure::None => {
            let downloaded = downloaded.unwrap();
            assert!(
                matches!(downloaded.outcome, DownloadOutcome::Success),
                "{downloaded:?}"
            );
            assert_eq!(downloaded.stats.downloaded, 1);
        }
        ReservedDownloadFailure::Finalization => {
            let downloaded = downloaded.unwrap();
            assert!(downloaded.stats.state_write_failures > 0, "{downloaded:?}");
            assert_eq!(
                std::fs::read_dir(parent).unwrap().count(),
                initial_file_count + 1
            );
            assert_eq!(server.received_requests().await.unwrap().len(), 1);
            let reservations = db.get_reconciliation_reservations().await.unwrap();
            let choice = reservations
                .iter()
                .find(|r| r.asset_id.as_ref() == "B")
                .unwrap();
            assert_eq!(std::fs::read(&choice.destination_path).unwrap(), bytes);
            // End-of-cycle promotion depends on whether last_seen_at falls
            // within this sync. Both statuses must retain retry evidence.
            let mut retryable = db.get_pending().await.unwrap();
            retryable.extend(db.get_failed().await.unwrap());
            assert!(retryable.iter().any(|row| {
                row.id.as_ref() == "B"
                    && row.local_path.as_ref()
                        == matches!(pass, ReservedDownloadPass::PendingRecorded)
                            .then_some(&recorded)
            }));
        }
        ReservedDownloadFailure::Reservation => {
            if let Ok(result) = downloaded {
                assert!(
                    matches!(result.outcome, DownloadOutcome::PartialFailure { .. }),
                    "{result:?}"
                );
                if matches!(pass, ReservedDownloadPass::Full) {
                    assert!(result.stats.enumeration_incomplete);
                    assert!(result.sync_token.is_none());
                }
            }
            assert!(server.received_requests().await.unwrap().is_empty());
            assert_eq!(
                std::fs::read_dir(parent).unwrap().count(),
                initial_file_count
            );
            assert_eq!(
                db.get_reconciliation_reservations().await.unwrap(),
                original_reservations
            );
        }
    }
    assert!(
        !reserved.exists(),
        "another rendition's reserved path must stay empty"
    );
    assert_eq!(
        db.get_metadata("sync_token:PrimarySync")
            .await
            .unwrap()
            .as_deref(),
        Some("before")
    );
    if !matches!(failure, ReservedDownloadFailure::None) {
        db.acquire_lock("restore state writes")
            .unwrap()
            .execute_batch("DROP TRIGGER fail_reserved_download")
            .unwrap();
    }
    config.state_db = Some(Arc::new(SqliteStateDb::open(&db_path).await.unwrap()));
    if !matches!(failure, ReservedDownloadFailure::None) {
        let recovered = run_reserved_download_cycle(
            pass,
            b.clone(),
            Arc::new(config.clone()),
            DownloadRunMode::Download,
        )
        .await
        .unwrap();
        assert!(
            matches!(recovered.outcome, DownloadOutcome::Success),
            "{recovered:?}"
        );
        assert_eq!(recovered.stats.state_write_failures, 0);
        assert_eq!(
            recovered.stats.downloaded,
            usize::from(matches!(failure, ReservedDownloadFailure::Reservation))
        );
    }
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    assert_eq!(
        std::fs::read_dir(parent).unwrap().count(),
        initial_file_count + 1
    );
    let reopened = config.state_db.as_ref().unwrap();
    let rows = reopened.get_downloaded_page(0, 10).await.unwrap();
    let row = rows.iter().find(|row| row.id.as_ref() != "A").unwrap();
    let new_path = row.local_path.as_ref().unwrap();
    assert_ne!(new_path, &reserved);
    assert_eq!(std::fs::read(new_path).unwrap(), bytes);
    assert_eq!(std::fs::read(&source).unwrap(), vec![1u8; 1024]);
    if matches!(pass, ReservedDownloadPass::PendingRecorded) {
        assert_eq!(std::fs::read(&recorded).unwrap(), vec![9u8; 1024]);
    }
    let reservations = reopened.get_reconciliation_reservations().await.unwrap();
    assert!(
        reservations
            .iter()
            .any(|choice| choice.asset_id == row.id && choice.destination_path == *new_path)
    );
    let both = lookup_pass(a.into_iter().chain(b).collect());
    for _ in 0..2 {
        let result =
            reconcile_catalog_paths(&both, Arc::new(config.clone()), CancellationToken::new())
                .await
                .unwrap();
        assert_eq!(
            (
                result.complete,
                result.stats.failed,
                result.stats.downloaded
            ),
            (true, 0, 0)
        );
        assert_eq!(
            reopened.get_reconciliation_reservations().await.unwrap(),
            reservations
        );
        assert_eq!(
            std::fs::read_dir(parent).unwrap().count(),
            initial_file_count + 1
        );
    }
}

#[tokio::test]
async fn pending_retry_preserves_reconciliation_reservations_across_restart() {
    assert_pending_retry_reservations(PendingReservationEntry::Absent).await;
}

#[tokio::test]
async fn pending_retry_reservations_reject_foreign_adoption() {
    assert_pending_retry_reservations(PendingReservationEntry::ForeignIdentical).await;
}

#[tokio::test]
async fn pending_retry_reservations_reuse_owned_file() {
    assert_pending_retry_reservations(PendingReservationEntry::Owned).await;
}

#[tokio::test]
async fn pending_retry_reservations_allow_owned_truncation_repair() {
    assert_pending_retry_reservations(PendingReservationEntry::OwnedTruncated).await;
}

#[derive(Clone, Copy)]
enum PendingReservationEntry {
    Absent,
    ForeignIdentical,
    Owned,
    OwnedTruncated,
}

async fn assert_pending_retry_reservations(entry: PendingReservationEntry) {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    let server = crate::start_wiremock_or_skip!();
    let mut body = vec![3u8; 1024];
    body[..3].copy_from_slice(&[0xFF, 0xD8, 0xFF]);
    let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&body));
    Mock::given(method("GET"))
        .and(path("/pending.jpg"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body.clone()))
        .expect(if matches!(entry, PendingReservationEntry::Owned) {
            0
        } else {
            1
        })
        .mount(&server)
        .await;
    let root = TempDir::new().unwrap();
    let db_path = root.path().join("state.db");
    let db = Arc::new(crate::state::SqliteStateDb::open(&db_path).await.unwrap());
    let source = root.path().join("reserved.jpg");
    std::fs::write(&source, vec![1u8; 1024]).unwrap();
    let local_checksum = file::compute_sha256(&source).await.unwrap();
    let mut a = mock_photo_records_for_zone_with_filename("A", "PrimarySync", "reserved.jpg");
    a[1]["fields"]["resJPEGFullRes"] = json!({"value": {
        "downloadURL": "https://p01.icloud-content.com/edited.jpg",
        "size": 1024, "fileChecksum": "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB="
    }});
    a[1]["fields"]["resJPEGFullFileType"] = json!({"value": "public.jpeg"});
    let mut b = incremental_photo_records_with_url(
        "B",
        "reserved_edited.JPG",
        &format!("{}/pending.jpg", server.uri()),
        1024,
    );
    b[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(checksum);
    for (id, filename, provider_checksum) in [
        (
            "A",
            "reserved.jpg",
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
        ),
        ("B", "reserved_edited.JPG", checksum.as_str()),
    ] {
        db.upsert_seen(
            &TestAssetRecord::new(id)
                .filename(filename)
                .checksum(provider_checksum)
                .size(1024)
                .build(),
        )
        .await
        .unwrap();
        db.upsert_asset_master_mapping("PrimarySync", &format!("asset-{id}"), id)
            .await
            .unwrap();
    }
    db.mark_downloaded(
        "PrimarySync",
        "A",
        "original",
        &source,
        &local_checksum,
        None,
    )
    .await
    .unwrap();
    db.set_metadata("sync_token:PrimarySync", "saved-token")
        .await
        .unwrap();
    let make_passes = |records| {
        vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: album_with_session(
                "PrimarySync",
                "",
                Box::new(PendingLookupSession {
                    records: Arc::new(records),
                }),
            ),
            exclude_ids: Arc::new(FxHashSet::default()),
        }]
    };
    let mut config = test_config();
    config.directory = Arc::from(root.path().join("new"));
    std::fs::create_dir_all(&config.directory).unwrap();
    config.edited = true;
    config.state_db = Some(db.clone());
    config.sync_mode = SyncMode::Incremental {
        zone_sync_token: "saved-token".into(),
    };
    let first = reconcile_catalog_paths(
        &make_passes(a.clone()),
        Arc::new(config.clone()),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(first.complete, "{first:?}");
    assert_eq!(first.stats.downloaded, 1);
    let reservations = db.get_reconciliation_reservations().await.unwrap();
    assert_eq!(reservations.len(), 2);
    let edited_path = reservations
        .iter()
        .find(|reservation| reservation.version_size != VersionSizeKey::Original)
        .unwrap()
        .destination_path
        .clone();
    assert!(
        !edited_path.exists(),
        "undownloaded rendition must still own its leaf"
    );
    let owned_filename = paths::insert_asset_identity_suffix(
        edited_path.file_name().unwrap().to_str().unwrap(),
        "B",
    );
    let owned_path = edited_path.with_file_name(&owned_filename);
    match entry {
        PendingReservationEntry::Absent => {}
        PendingReservationEntry::ForeignIdentical => {
            std::fs::write(&edited_path, &body).unwrap();
        }
        PendingReservationEntry::Owned | PendingReservationEntry::OwnedTruncated => {
            let requested_key = filter::PathPlanningMode::Reconciliation
                .key(&edited_path)
                .unwrap();
            let destination_key = filter::PathPlanningMode::Reconciliation
                .key(&owned_path)
                .unwrap();
            db.reserve_reconciliation_paths(&[crate::state::ReconciliationReservation {
                content: Some(crate::state::ReconciliationContent {
                    checksum: checksum.clone().into(),
                    size: 1024,
                }),
                library: Arc::from("PrimarySync"),
                asset_id: "B".into(),
                version_size: VersionSizeKey::Original,
                requested_path_key: crate::state::ReconciliationPathKey(
                    requested_key.as_ref().into(),
                ),
                destination_path_key: crate::state::ReconciliationPathKey(
                    destination_key.as_ref().into(),
                ),
                destination_path: owned_path.clone(),
            }])
            .await
            .unwrap();
            db.upsert_seen(
                &TestAssetRecord::new("B")
                    .filename(&owned_filename)
                    .checksum(&checksum)
                    .size(1024)
                    .build(),
            )
            .await
            .unwrap();
            std::fs::write(&owned_path, &body).unwrap();
            let local = file::compute_sha256(&owned_path).await.unwrap();
            db.mark_downloaded(
                "PrimarySync",
                "B",
                "original",
                &owned_path,
                &local,
                Some(&local),
            )
            .await
            .unwrap();
            let reason = if matches!(entry, PendingReservationEntry::OwnedTruncated) {
                std::fs::write(&owned_path, &body[..4]).unwrap();
                config.repair_truncated = true;
                crate::commands::reconcile::FILE_TRUNCATED_REASON
            } else {
                "retry finalization"
            };
            db.mark_failed("PrimarySync", "B", "original", reason)
                .await
                .unwrap();
        }
    }
    // Retry must load the ledger from durable state, not a previous planner.
    config.state_db = Some(Arc::new(
        crate::state::SqliteStateDb::open(&db_path).await.unwrap(),
    ));
    let retry = download_photos_with_sync(
        &Client::new(),
        &make_passes(b.clone()),
        Arc::new(config.clone()),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(
        matches!(retry.outcome, DownloadOutcome::Success),
        "{retry:?}"
    );
    assert_eq!(
        retry.stats.downloaded,
        usize::from(!matches!(entry, PendingReservationEntry::Owned))
    );
    config.state_db = Some(Arc::new(
        crate::state::SqliteStateDb::open(&db_path).await.unwrap(),
    ));
    let reopened = config.state_db.as_ref().unwrap();
    let downloaded = reopened.get_downloaded_page(0, 10).await.unwrap();
    let b_path = downloaded
        .iter()
        .find(|row| row.id.as_ref() == "B")
        .unwrap()
        .local_path
        .clone()
        .unwrap();
    if matches!(
        entry,
        PendingReservationEntry::Owned | PendingReservationEntry::OwnedTruncated
    ) {
        assert_eq!(b_path, owned_path, "retry must reuse its saved destination");
    }
    let all_records = a.into_iter().chain(b).collect();
    let passes = make_passes(all_records);
    for _ in 0..2 {
        let repeated =
            reconcile_catalog_paths(&passes, Arc::new(config.clone()), CancellationToken::new())
                .await
                .unwrap();
        assert!(repeated.complete, "{repeated:?}");
        assert_eq!(repeated.stats.failed, 0);
        assert_eq!(repeated.stats.downloaded, 0);
        assert_eq!(repeated.stats.disk_bytes_written, 0);
    }
    assert_ne!(b_path, edited_path);
    if matches!(entry, PendingReservationEntry::ForeignIdentical) {
        assert_eq!(std::fs::read(&edited_path).unwrap(), body);
    } else {
        assert!(!edited_path.exists());
    }
    if matches!(
        entry,
        PendingReservationEntry::Owned | PendingReservationEntry::OwnedTruncated
    ) {
        assert_eq!(b_path, owned_path);
    }
    assert_eq!(std::fs::read(&b_path).unwrap(), body);
    assert_eq!(std::fs::read(&source).unwrap(), vec![1u8; 1024]);
    let rows = reopened.get_downloaded_page(0, 10).await.unwrap();
    assert_eq!(rows.len(), 2);
    assert!(reopened.get_pending().await.unwrap().is_empty());
    assert_eq!(
        reopened
            .get_metadata("sync_token:PrimarySync")
            .await
            .unwrap()
            .as_deref(),
        Some("saved-token")
    );
    for row in rows {
        let path = row.local_path.unwrap();
        if row.id.as_ref() == "B" {
            assert_eq!(path, b_path);
        } else {
            assert_eq!(std::fs::read(&path).unwrap(), vec![1u8; 1024]);
        }
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            if matches!(entry, PendingReservationEntry::ForeignIdentical) {
                3
            } else {
                2
            }
        );
    }
}

#[tokio::test]
async fn path_reconciliation_live_resolution_change_preserves_rendition_ownership() {
    let root = tempfile::tempdir().unwrap();
    let db_path = root.path().join("state.db");
    let db = Arc::new(crate::state::SqliteStateDb::open(&db_path).await.unwrap());
    let mut records = mock_photo_records_for_zone_with_filename("LIVE", "PrimarySync", "live.jpg");
    for (key, size, url) in [
        ("resOriginalVidCompl", 1024, "original"),
        ("resVidMed", 512, "medium"),
    ] {
        records[0]["fields"][format!("{key}Res")] = json!({"value": {
            "downloadURL": format!("https://p01.icloud-content.com/{url}.mov"),
            "size": size, "fileChecksum": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
        }});
        records[0]["fields"][format!("{key}FileType")] =
            json!({"value": "com.apple.quicktime-movie"});
    }
    let asset = PhotoAsset::new(records[0].clone(), records[1].clone());
    let mut config = test_config();
    config.directory = Arc::from(root.path().join("new"));
    std::fs::create_dir_all(&config.directory).unwrap();
    config.state_db = Some(db.clone());
    let expected = filter::expected_paths_for(&asset, &config);
    assert_eq!(expected.len(), 2);
    for item in expected {
        let source = root.path().join("old").join(item.path.file_name().unwrap());
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(&source, vec![1u8; 1024]).unwrap();
        let checksum = file::compute_sha256(&source).await.unwrap();
        let row = crate::test_helpers::TestAssetRecord::new("LIVE")
            .filename("live.jpg")
            .version_size(item.version_size)
            .checksum("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
            .size(1024)
            .build();
        db.upsert_seen(&row).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "LIVE",
            item.version_size.as_str(),
            &source,
            &checksum,
            None,
        )
        .await
        .unwrap();
    }
    db.upsert_asset_master_mapping("PrimarySync", "asset-LIVE", "LIVE")
        .await
        .unwrap();
    let passes = vec![AlbumPass {
        kind: PassKind::Unfiled,
        album: album_with_session(
            "PrimarySync",
            "",
            Box::new(PendingLookupSession {
                records: Arc::new(records),
            }),
        ),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];
    let first =
        reconcile_catalog_paths(&passes, Arc::new(config.clone()), CancellationToken::new())
            .await
            .unwrap();
    assert!(first.complete, "initial move: {first:?}");
    assert_eq!(first.stats.downloaded, 2);
    let steady =
        reconcile_catalog_paths(&passes, Arc::new(config.clone()), CancellationToken::new())
            .await
            .unwrap();
    assert!(steady.complete);
    assert_eq!(steady.stats.downloaded, 0);
    config.state_db = Some(Arc::new(
        crate::state::SqliteStateDb::open(&db_path).await.unwrap(),
    ));
    config.live_resolution = AssetVersionSize::LiveMedium;
    let changed =
        reconcile_catalog_paths(&passes, Arc::new(config.clone()), CancellationToken::new())
            .await
            .unwrap();
    assert!(
        changed.complete,
        "changing live resolution must leave the newly selected rendition to normal download: {changed:?}"
    );
    assert_eq!(changed.stats.downloaded, 0);
    assert_eq!(changed.stats.state_write_failures, 0);
    let reservations = db.get_reconciliation_reservations().await.unwrap();
    assert_eq!(reservations.len(), 3);
    let motion_paths: FxHashSet<_> = reservations
        .iter()
        .filter(|reservation| reservation.version_size != VersionSizeKey::Original)
        .map(|reservation| &reservation.destination_path)
        .collect();
    assert_eq!(
        motion_paths.len(),
        2,
        "motion renditions must not share a path"
    );
    for resolution in [AssetVersionSize::LiveMedium, AssetVersionSize::LiveOriginal] {
        config.state_db = Some(Arc::new(
            crate::state::SqliteStateDb::open(&db_path).await.unwrap(),
        ));
        config.live_resolution = resolution;
        let repeated =
            reconcile_catalog_paths(&passes, Arc::new(config.clone()), CancellationToken::new())
                .await
                .unwrap();
        assert!(repeated.complete);
        assert_eq!(repeated.stats.downloaded, 0);
        assert_eq!(repeated.stats.disk_bytes_written, 0);
        let retained = db.get_reconciliation_reservations().await.unwrap();
        assert_eq!(retained.len(), reservations.len());
        assert!(
            reservations
                .iter()
                .all(|reservation| retained.contains(reservation))
        );
    }
    let rows = db.get_downloaded_page(0, 10).await.unwrap();
    assert_eq!(rows.len(), 2, "new rendition remains for normal download");
    for row in rows {
        let destination = row.local_path.unwrap();
        let source = root
            .path()
            .join("old")
            .join(destination.file_name().unwrap());
        assert_eq!(std::fs::read(&source).unwrap(), vec![1u8; 1024]);
        assert_eq!(std::fs::read(&destination).unwrap(), vec![1u8; 1024]);
        assert_eq!(
            std::fs::read_dir(destination.parent().unwrap())
                .unwrap()
                .count(),
            2
        );
    }
}

#[tokio::test]
async fn path_reconciliation_cross_library_reservations() {
    for scenario in [
        ReservationScenario::CrossLibraryDistinctBytes,
        ReservationScenario::CrossLibraryIdenticalBytes,
        ReservationScenario::CrossLibrarySameIdentity,
    ] {
        assert_reconciliation_reservations(scenario).await;
    }
}

#[tokio::test]
async fn path_reconciliation_reservations_survive_lookup_and_state_failure() {
    assert_reconciliation_reservations(ReservationScenario::Interrupted).await;
}

#[tokio::test]
async fn reconciliation_reservations_write_failure_prevents_publication() {
    assert_reconciliation_reservations(ReservationScenario::ReservationWriteFailure).await;
}

#[derive(Clone, Copy)]
enum ReservationScenario {
    CrossLibraryDistinctBytes,
    CrossLibraryIdenticalBytes,
    CrossLibrarySameIdentity,
    Interrupted,
    ReservationWriteFailure,
}

async fn assert_reconciliation_reservations(scenario: ReservationScenario) {
    let cross_library = matches!(
        scenario,
        ReservationScenario::CrossLibraryDistinctBytes
            | ReservationScenario::CrossLibraryIdenticalBytes
            | ReservationScenario::CrossLibrarySameIdentity
    );
    let same_bytes = matches!(scenario, ReservationScenario::CrossLibraryIdenticalBytes);
    let root = tempfile::tempdir().unwrap();
    let db_path = root.path().join("state.db");
    let db = Arc::new(crate::state::SqliteStateDb::open(&db_path).await.unwrap());
    let mut config = test_config();
    config.directory = Arc::from(root.path().join("new"));
    std::fs::create_dir_all(&config.directory).unwrap();
    config.state_db = Some(db.clone());
    let mut fixtures = Vec::new();
    for (index, id) in ["COLLIDE_A", "COLLIDE_B"].into_iter().enumerate() {
        let library = if cross_library && index == 1 {
            "SharedSync"
        } else {
            "PrimarySync"
        };
        let id = if matches!(scenario, ReservationScenario::CrossLibrarySameIdentity) {
            "COLLIDE"
        } else {
            id
        };
        let records = mock_photo_records_for_zone_with_filename(id, library, "IMG_0001.JPG");
        let source = root.path().join(library).join(id).join("IMG_0001.JPG");
        let bytes = vec![if same_bytes || index == 0 { 1u8 } else { 2u8 }; 1024];
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(&source, &bytes).unwrap();
        let checksum = file::compute_sha256(&source).await.unwrap();
        let row = crate::test_helpers::TestAssetRecord::new(id)
            .library(library)
            .filename("IMG_0001.JPG")
            .checksum("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
            .size(1024)
            .build();
        db.upsert_seen(&row).await.unwrap();
        db.mark_downloaded(library, id, "original", &source, &checksum, None)
            .await
            .unwrap();
        db.upsert_asset_master_mapping(library, &format!("asset-{id}"), id)
            .await
            .unwrap();
        db.set_metadata(&format!("sync_token:{library}"), "original-token")
            .await
            .unwrap();
        fixtures.push((library, id, records, source, bytes));
    }
    let make_passes = |library: &str, records: Vec<Value>| {
        vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: album_with_session(
                library,
                "",
                Box::new(PendingLookupSession {
                    records: Arc::new(records),
                }),
            ),
            exclude_ids: Arc::new(FxHashSet::default()),
        }]
    };
    if matches!(scenario, ReservationScenario::Interrupted) {
        db.acquire_lock("inject reconciliation finalization failure").unwrap().execute_batch(
            "CREATE TEMP TRIGGER fail_reservation BEFORE UPDATE OF local_path ON assets WHEN NEW.id = 'COLLIDE_B' AND NEW.local_path IS NOT OLD.local_path BEGIN SELECT RAISE(FAIL, 'injected path state failure'); END;"
        ).unwrap();
        // A is unresolved, so B publishes the unsuffixed path before its
        // state write fails. Neither new destination is in assets yet.
        let passes = make_passes("PrimarySync", fixtures[1].2.clone());
        let failed =
            reconcile_catalog_paths(&passes, Arc::new(config.clone()), CancellationToken::new())
                .await
                .unwrap();
        assert!(!failed.complete);
        assert_eq!(failed.stats.state_write_failures, 1);
        let rows = db.get_downloaded_page(0, 10).await.unwrap();
        for (_, id, _, source, _) in &fixtures {
            assert_eq!(
                rows.iter()
                    .find(|row| row.id.as_ref() == *id)
                    .unwrap()
                    .local_path
                    .as_ref(),
                Some(source)
            );
        }
        db.acquire_lock("restore reconciliation finalization")
            .unwrap()
            .execute_batch("DROP TRIGGER fail_reservation")
            .unwrap();
    }
    if matches!(scenario, ReservationScenario::ReservationWriteFailure) {
        db.acquire_lock("inject reservation write failure").unwrap().execute_batch(
            "CREATE TEMP TRIGGER fail_reservation_insert BEFORE INSERT ON reconciliation_paths BEGIN SELECT RAISE(FAIL, 'injected reservation failure'); END;"
        ).unwrap();
        let passes = make_passes(
            "PrimarySync",
            fixtures
                .iter()
                .flat_map(|fixture| fixture.2.clone())
                .collect(),
        );
        let failed =
            reconcile_catalog_paths(&passes, Arc::new(config.clone()), CancellationToken::new())
                .await
                .unwrap();
        assert!(!failed.complete);
        assert_eq!(failed.stats.state_write_failures, 1);
        assert_eq!(failed.stats.downloaded, 0);
        assert!(
            db.get_reconciliation_reservations()
                .await
                .unwrap()
                .is_empty()
        );
        for fixture in &fixtures {
            let asset = PhotoAsset::new(fixture.2[0].clone(), fixture.2[1].clone());
            let destination = filter::expected_paths_for(&asset, &config).remove(0).path;
            assert!(!destination.exists());
        }
        db.acquire_lock("restore reservation writes")
            .unwrap()
            .execute_batch("DROP TRIGGER fail_reservation_insert")
            .unwrap();
    }
    // Reopen SQLite and rebuild every planner, as after a process restart.
    let reopened = Arc::new(crate::state::SqliteStateDb::open(&db_path).await.unwrap());
    config.state_db = Some(reopened.clone());
    let libraries = if cross_library {
        vec!["PrimarySync", "SharedSync"]
    } else {
        vec!["PrimarySync"]
    };
    let mut destinations = Vec::new();
    for cycle in 0..2 {
        for library in &libraries {
            config.library = Arc::from(*library);
            let records = fixtures
                .iter()
                .filter(|fixture| fixture.0 == *library)
                .flat_map(|fixture| fixture.2.clone())
                .collect();
            let passes = make_passes(library, records);
            let result = reconcile_catalog_paths(
                &passes,
                Arc::new(config.clone()),
                CancellationToken::new(),
            )
            .await
            .unwrap();
            assert!(
                result.complete,
                "library {library}, cycle {cycle}: {result:?}"
            );
            assert_eq!(
                result.stats.downloaded,
                if cycle == 0 {
                    if cross_library { 1 } else { 2 }
                } else {
                    0
                }
            );
            if cycle == 1 {
                assert_eq!(result.stats.disk_bytes_written, 0);
            }
            assert_eq!(
                reopened
                    .get_metadata(&format!("sync_token:{library}"))
                    .await
                    .unwrap()
                    .as_deref(),
                Some("original-token")
            );
        }
        let rows = reopened.get_downloaded_page(0, 10).await.unwrap();
        assert_eq!(rows.len(), 2);
        let paths: Vec<_> = fixtures
            .iter()
            .map(|(library, id, _, source, bytes)| {
                let row = rows
                    .iter()
                    .find(|row| row.library.as_ref() == *library && row.id.as_ref() == *id)
                    .unwrap();
                let path = row.local_path.clone().unwrap();
                assert_eq!(std::fs::read(source).unwrap(), *bytes);
                assert_eq!(std::fs::read(&path).unwrap(), *bytes);
                path
            })
            .collect();
        assert_ne!(paths[0], paths[1]);
        assert_eq!(
            std::fs::read_dir(paths[0].parent().unwrap())
                .unwrap()
                .count(),
            2,
            "no orphaned reconciliation copies"
        );
        if cycle == 0 {
            destinations = paths;
        } else {
            assert_eq!(paths, destinations);
        }
    }
}

#[tokio::test]
async fn path_reconciliation_equivalent_root_collisions_preserve_ownership() {
    let cwd = std::env::current_dir().unwrap();
    let root = tempfile::tempdir_in(&cwd).unwrap();
    let relative = root.path().strip_prefix(&cwd).unwrap();
    let db_path = root.path().join("state.db");
    let db = Arc::new(crate::state::SqliteStateDb::open(&db_path).await.unwrap());
    let mut config = test_config();
    config.directory = Arc::from(relative);
    config.state_db = Some(db.clone());
    #[cfg(feature = "xmp")]
    {
        config.metadata.xmp_sidecar = true;
    }
    let old_time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(12345);
    let mut provider_records = Vec::new();
    let mut fixtures = Vec::new();
    // Row order need not match the order of successful downloads: B owns
    // the natural filename, although A's durable row comes first.
    for (id, byte) in [("COLLIDE_A", 1u8), ("COLLIDE_B", 2u8)] {
        let records = mock_photo_records_for_zone_with_filename(id, "PrimarySync", "IMG_0001.JPG");
        let asset = PhotoAsset::new(records[0].clone(), records[1].clone());
        let canonical = filter::expected_paths_for(&asset, &config).remove(0).path;
        let source = if id == "COLLIDE_A" {
            canonical.with_file_name(paths::insert_asset_identity_suffix("IMG_0001.JPG", id))
        } else {
            canonical
        };
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(&source, vec![byte; 1024]).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&source)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(old_time))
            .unwrap();
        let sidecar = source.with_extension("JPG.xmp");
        std::fs::write(&sidecar, id.as_bytes()).unwrap();
        let checksum = file::compute_sha256(&source).await.unwrap();
        let row = crate::test_helpers::TestAssetRecord::new(id)
            .filename("IMG_0001.JPG")
            .checksum("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
            .size(1024)
            .build();
        db.upsert_seen(&row).await.unwrap();
        db.mark_downloaded("PrimarySync", id, "original", &source, &checksum, None)
            .await
            .unwrap();
        db.upsert_asset_master_mapping("PrimarySync", &format!("asset-{id}"), id)
            .await
            .unwrap();
        fixtures.push((
            id,
            byte,
            source.strip_prefix(relative).unwrap().to_path_buf(),
            checksum,
        ));
        provider_records.extend(records);
    }
    let passes = vec![AlbumPass {
        kind: PassKind::Unfiled,
        album: album_with_session(
            "PrimarySync",
            "",
            Box::new(PendingLookupSession {
                records: Arc::new(provider_records),
            }),
        ),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];
    let mut old_directory = relative;
    for directory in [root.path(), relative] {
        config.directory = Arc::from(directory);
        let config = Arc::new(config.clone());
        db.acquire_lock("inject equivalent-root collision state failure").unwrap().execute_batch(
            "CREATE TEMP TRIGGER fail_collision_root BEFORE UPDATE OF local_path ON assets WHEN NEW.id = 'COLLIDE_A' AND NEW.local_path IS NOT OLD.local_path BEGIN SELECT RAISE(FAIL, 'injected path state failure'); END;"
        ).unwrap();
        let failed =
            reconcile_catalog_paths(&passes, Arc::clone(&config), CancellationToken::new())
                .await
                .unwrap();
        assert!(!failed.complete);
        assert_eq!(failed.stats.failed, 0);
        assert_eq!(failed.stats.exif_failures, 0);
        assert_eq!(failed.stats.state_write_failures, 1);
        assert_eq!(failed.stats.downloaded, 1);
        let reopened = crate::state::SqliteStateDb::open(&db_path).await.unwrap();
        let rows = reopened.get_downloaded_page(0, 10).await.unwrap();
        assert_eq!(rows.len(), 2);
        for (id, _, suffix, _) in &fixtures {
            let row = rows.iter().find(|row| row.id.as_ref() == *id).unwrap();
            let expected = if *id == "COLLIDE_A" {
                old_directory
            } else {
                directory
            }
            .join(suffix);
            assert_eq!(row.local_path.as_deref(), Some(expected.as_path()));
        }
        db.acquire_lock("restore equivalent-root collision state writes")
            .unwrap()
            .execute_batch("DROP TRIGGER fail_collision_root")
            .unwrap();
        for cycle in 0..2 {
            let result =
                reconcile_catalog_paths(&passes, Arc::clone(&config), CancellationToken::new())
                    .await
                    .unwrap();
            assert!(result.complete);
            assert_eq!(result.stats.downloaded, if cycle == 0 { 1 } else { 0 });
            assert_eq!(result.stats.disk_bytes_written, 0);
            let rows = reopened.get_downloaded_page(0, 10).await.unwrap();
            assert_eq!(rows.len(), 2);
            for (id, byte, suffix, checksum) in &fixtures {
                let destination = directory.join(suffix);
                let row = rows.iter().find(|row| row.id.as_ref() == *id).unwrap();
                assert_eq!(row.local_path.as_deref(), Some(destination.as_path()));
                assert_eq!(row.local_checksum.as_deref(), Some(checksum.as_str()));
                assert_eq!(std::fs::read(&destination).unwrap(), vec![*byte; 1024]);
                assert_eq!(
                    std::fs::metadata(&destination).unwrap().modified().unwrap(),
                    old_time
                );
                assert_eq!(
                    std::fs::read(destination.with_extension("JPG.xmp")).unwrap(),
                    id.as_bytes()
                );
                assert_eq!(
                    std::fs::read_dir(destination.parent().unwrap())
                        .unwrap()
                        .count(),
                    4
                );
            }
        }
        old_directory = directory;
    }
}

#[tokio::test]
async fn path_reconciliation_equivalent_roots_preserve_files_across_retry() {
    for packet in [None, Some(b"unchanged user-owned sidecar".as_slice())] {
        let cwd = std::env::current_dir().unwrap();
        let root = tempfile::tempdir_in(&cwd).unwrap();
        let relative = root.path().strip_prefix(&cwd).unwrap();
        let db_path = root.path().join("state.db");
        let db = Arc::new(crate::state::SqliteStateDb::open(&db_path).await.unwrap());
        let records =
            mock_photo_records_for_zone_with_filename("RECONCILE", "PrimarySync", "reconcile.jpg");
        let asset = PhotoAsset::new(records[0].clone(), records[1].clone());
        let passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: album_with_session(
                "PrimarySync",
                "",
                Box::new(PendingLookupSession {
                    records: Arc::new(records),
                }),
            ),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];
        let mut config = test_config();
        config.directory = Arc::from(relative);
        config.state_db = Some(db.clone());
        #[cfg(feature = "xmp")]
        {
            config.metadata.xmp_sidecar = true;
        }
        let source = filter::expected_paths_for(&asset, &config).remove(0).path;
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(&source, vec![1u8; 1024]).unwrap();
        let old_time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(12345);
        std::fs::File::options()
            .write(true)
            .open(&source)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(old_time))
            .unwrap();
        let sidecar = source.with_file_name("reconcile.jpg.xmp");
        if let Some(packet) = packet {
            std::fs::write(&sidecar, packet).unwrap();
        }
        let checksum = file::compute_sha256(&source).await.unwrap();
        let row = crate::test_helpers::TestAssetRecord::new("RECONCILE")
            .filename("reconcile.jpg")
            .checksum("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
            .size(1024)
            .build();
        db.upsert_seen(&row).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "RECONCILE",
            "original",
            &source,
            &checksum,
            None,
        )
        .await
        .unwrap();
        db.upsert_asset_master_mapping("PrimarySync", "asset-RECONCILE", "RECONCILE")
            .await
            .unwrap();
        let mut recorded_path = source.clone();
        for directory in [root.path(), relative] {
            let old_hash = hash_download_config(&config);
            config.directory = Arc::from(directory);
            assert_ne!(hash_download_config(&config), old_hash);
            let destination = filter::expected_paths_for(&asset, &config).remove(0).path;
            let cycle_config = Arc::new(config.clone());
            db.acquire_lock("inject equivalent-root finalization failure").unwrap().execute_batch(
                "CREATE TEMP TRIGGER fail_equivalent_root BEFORE UPDATE OF local_path ON assets WHEN NEW.local_path IS NOT OLD.local_path BEGIN SELECT RAISE(FAIL, 'injected path state failure'); END;"
            ).unwrap();
            let failed = reconcile_catalog_paths(
                &passes,
                Arc::clone(&cycle_config),
                CancellationToken::new(),
            )
            .await
            .unwrap();
            assert!(!failed.complete);
            assert_eq!(failed.stats.failed, 0);
            assert_eq!(failed.stats.exif_failures, 0);
            assert_eq!(failed.stats.state_write_failures, 1);
            assert_eq!(failed.stats.downloaded, 0);
            assert_eq!(
                std::fs::metadata(&source).unwrap().modified().unwrap(),
                old_time
            );
            assert_eq!(std::fs::read(&sidecar).ok().as_deref(), packet);
            let reopened = crate::state::SqliteStateDb::open(&db_path).await.unwrap();
            let rows = reopened.get_downloaded_page(0, 10).await.unwrap();
            assert_eq!(rows[0].local_path.as_deref(), Some(recorded_path.as_path()));
            db.acquire_lock("restore equivalent-root finalization")
                .unwrap()
                .execute_batch("DROP TRIGGER fail_equivalent_root")
                .unwrap();
            for cycle in 0..2 {
                let result = reconcile_catalog_paths(
                    &passes,
                    Arc::clone(&cycle_config),
                    CancellationToken::new(),
                )
                .await
                .unwrap();
                assert!(result.complete);
                assert_eq!(result.stats.downloaded, if cycle == 0 { 1 } else { 0 });
                assert_eq!(result.stats.disk_bytes_written, 0);
                assert_eq!(std::fs::read(&source).unwrap(), vec![1u8; 1024]);
                assert_eq!(
                    std::fs::metadata(&source).unwrap().modified().unwrap(),
                    old_time
                );
                assert_eq!(std::fs::read(&sidecar).ok().as_deref(), packet);
                assert_eq!(
                    std::fs::read_dir(source.parent().unwrap()).unwrap().count(),
                    1 + usize::from(packet.is_some())
                );
                let rows = reopened.get_downloaded_page(0, 10).await.unwrap();
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0].local_path.as_deref(), Some(destination.as_path()));
                assert_eq!(rows[0].local_checksum.as_deref(), Some(checksum.as_str()));
            }
            recorded_path = destination;
        }
    }
}

#[tokio::test]
async fn path_reconciliation_preserves_mtime_and_metadata_across_retry() {
    for mode in [
        ReconciliationMetadataCase::Disabled,
        ReconciliationMetadataCase::MediaConflict,
        ReconciliationMetadataCase::Conflict,
        ReconciliationMetadataCase::StateFailure,
        ReconciliationMetadataCase::Missing,
    ] {
        #[cfg(not(feature = "xmp"))]
        if !matches!(
            mode,
            ReconciliationMetadataCase::Disabled | ReconciliationMetadataCase::MediaConflict
        ) {
            continue;
        }
        reconciliation_metadata_transition(mode).await;
    }
}

#[tokio::test]
async fn path_reconciliation_rejects_hardlinked_destination_then_recovers() {
    reconciliation_metadata_transition(ReconciliationMetadataCase::HardLinkedDestination).await;
}

#[derive(Debug)]
enum ReconciliationMetadataCase {
    Disabled,
    MediaConflict,
    HardLinkedDestination,
    Conflict,
    StateFailure,
    Missing,
}

async fn reconciliation_metadata_transition(mode: ReconciliationMetadataCase) {
    #[derive(Clone, Debug)]
    struct LookupOnlySession {
        records: Arc<Vec<Value>>,
    }

    #[async_trait::async_trait]
    impl PhotosSession for LookupOnlySession {
        async fn post(
            &self,
            url: &str,
            _body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            if url.contains("/records/lookup?") {
                return Ok(json!({"records": self.records.as_ref().clone()}));
            }
            anyhow::bail!("path reconciliation made an unexpected provider request: {url}")
        }

        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(self.clone())
        }
    }

    let old_dir = TempDir::new().expect("old dir");
    let db_path = old_dir.path().join("state.db");
    let db = Arc::new(crate::state::SqliteStateDb::open(&db_path).await.unwrap());
    let new_dir = TempDir::new().expect("new dir");
    let old_path = old_dir.path().join("reconcile.jpg");
    tokio::fs::write(&old_path, vec![0u8; 1024]).await.unwrap();
    let local_checksum = file::compute_sha256(&old_path).await.unwrap();
    let record = crate::test_helpers::TestAssetRecord::new("RECONCILE")
        .filename("reconcile.jpg")
        .checksum("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
        .size(1024)
        .build();
    db.upsert_seen(&record).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "RECONCILE",
        "original",
        &old_path,
        &local_checksum,
        Some("provider-checksum"),
    )
    .await
    .unwrap();
    db.upsert_asset_master_mapping("PrimarySync", "asset-RECONCILE", "RECONCILE")
        .await
        .unwrap();

    let mut records =
        mock_photo_records_for_zone_with_filename("RECONCILE", "PrimarySync", "reconcile.jpg");
    records[1]["fields"]["resJPEGFullRes"] = json!({"value": {
        "downloadURL": "https://p01.icloud-content.com/reconcile-adjusted.jpg",
        "size": 512,
        "fileChecksum": "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB="
    }});
    records[1]["fields"]["resJPEGFullFileType"] = json!({"value": "public.jpeg"});
    let asset = PhotoAsset::new(records[0].clone(), records[1].clone());
    let passes = vec![AlbumPass {
        kind: PassKind::Unfiled,
        album: album_with_session(
            "PrimarySync",
            "",
            Box::new(LookupOnlySession {
                records: Arc::new(records),
            }),
        ),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];
    let mut config = test_config();
    config.directory = Arc::from(new_dir.path());
    config.state_db = Some(db.clone());
    config.edited = true;
    let expected_paths = filter::expected_paths_for(&asset, &config);
    let expected_path = expected_paths
        .into_iter()
        .find(|path| path.version_size == VersionSizeKey::Original)
        .unwrap()
        .path;
    let source_sidecar = old_path.with_file_name("reconcile.jpg.xmp");
    let mut sidecar_name = expected_path.file_name().unwrap().to_os_string();
    sidecar_name.push(".xmp");
    let new_sidecar = expected_path.with_file_name(sidecar_name);
    let packet = br#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:custom="https://example.test/custom/" custom:Note="keep this exact packet" xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmp:Rating="5"/></rdf:RDF></x:xmpmeta>"#;
    let old_time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(12345);
    std::fs::File::options()
        .write(true)
        .open(&old_path)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(old_time))
        .unwrap();
    if !matches!(mode, ReconciliationMetadataCase::Missing) {
        std::fs::write(&source_sidecar, packet).unwrap();
    }
    #[cfg(feature = "xmp")]
    {
        config.metadata.xmp_sidecar = !matches!(mode, ReconciliationMetadataCase::Disabled);
    }
    if matches!(mode, ReconciliationMetadataCase::Conflict) {
        std::fs::create_dir_all(new_sidecar.parent().unwrap()).unwrap();
        std::fs::write(&new_sidecar, b"user-owned conflicting sidecar").unwrap();
    }
    if matches!(mode, ReconciliationMetadataCase::MediaConflict) {
        std::fs::create_dir_all(expected_path.parent().unwrap()).unwrap();
        std::fs::write(&expected_path, vec![1u8; 1024]).unwrap();
    }
    if matches!(mode, ReconciliationMetadataCase::HardLinkedDestination) {
        std::fs::create_dir_all(expected_path.parent().unwrap()).unwrap();
        std::fs::hard_link(&old_path, &expected_path).unwrap();
    }
    if matches!(mode, ReconciliationMetadataCase::StateFailure) {
        db.acquire_lock("inject reconciliation finalization failure").unwrap().execute_batch(
            "CREATE TEMP TRIGGER fail_reconciled_path BEFORE UPDATE OF local_path ON assets WHEN NEW.local_path IS NOT OLD.local_path BEGIN SELECT RAISE(FAIL, 'injected reconciliation state failure'); END;"
        ).unwrap();
    }
    let config = Arc::new(config);
    if matches!(
        mode,
        ReconciliationMetadataCase::Conflict
            | ReconciliationMetadataCase::StateFailure
            | ReconciliationMetadataCase::MediaConflict
            | ReconciliationMetadataCase::HardLinkedDestination
    ) {
        let failed =
            reconcile_catalog_paths(&passes, Arc::clone(&config), CancellationToken::new())
                .await
                .unwrap();
        assert_eq!(
            std::fs::metadata(&old_path).unwrap().modified().unwrap(),
            old_time
        );
        assert!(!failed.complete);
        assert_eq!(failed.stats.downloaded, 0);
        assert_eq!(
            failed.stats.failed + failed.stats.exif_failures + failed.stats.state_write_failures,
            1
        );
        let reopened = crate::state::SqliteStateDb::open(&db_path).await.unwrap();
        let rows = reopened.get_downloaded_page(0, 10).await.unwrap();
        assert_eq!(rows[0].local_path.as_deref(), Some(old_path.as_path()));
        assert_eq!(std::fs::read(&source_sidecar).unwrap(), packet);
        if matches!(mode, ReconciliationMetadataCase::MediaConflict) {
            assert_eq!(std::fs::read(&expected_path).unwrap(), vec![1u8; 1024]);
            assert_eq!(
                std::fs::read_dir(expected_path.parent().unwrap())
                    .unwrap()
                    .count(),
                1
            );
            std::fs::remove_file(&expected_path).unwrap();
        } else {
            assert_eq!(std::fs::read(&expected_path).unwrap(), vec![0u8; 1024]);
        }
        if matches!(mode, ReconciliationMetadataCase::HardLinkedDestination) {
            let repeated =
                reconcile_catalog_paths(&passes, Arc::clone(&config), CancellationToken::new())
                    .await
                    .unwrap();
            assert!(!repeated.complete);
            assert_eq!(repeated.stats.failed, 1);
            assert_eq!(repeated.stats.downloaded, 0);
            assert_eq!(
                std::fs::metadata(&old_path).unwrap().modified().unwrap(),
                old_time
            );
            assert_eq!(
                std::fs::read_dir(expected_path.parent().unwrap())
                    .unwrap()
                    .count(),
                1
            );
            let rows = reopened.get_downloaded_page(0, 10).await.unwrap();
            assert_eq!(rows[0].local_path.as_deref(), Some(old_path.as_path()));
            std::fs::remove_file(&expected_path).unwrap();
        }
        if matches!(mode, ReconciliationMetadataCase::Conflict) {
            assert_eq!(
                std::fs::read(&new_sidecar).unwrap(),
                b"user-owned conflicting sidecar"
            );
            std::fs::remove_file(&new_sidecar).unwrap();
        } else if matches!(mode, ReconciliationMetadataCase::StateFailure) {
            db.acquire_lock("restore reconciliation finalization")
                .unwrap()
                .execute_batch("DROP TRIGGER fail_reconciled_path")
                .unwrap();
        }
    }
    let repaired = reconcile_catalog_paths(&passes, Arc::clone(&config), CancellationToken::new())
        .await
        .unwrap();
    assert!(repaired.complete, "case {mode:?}");
    assert_eq!(repaired.stats.downloaded, 1);
    assert_eq!(repaired.stats.exif_failures, 0);
    let reopened = crate::state::SqliteStateDb::open(&db_path).await.unwrap();
    let rows = reopened.get_downloaded_page(0, 10).await.unwrap();
    assert_eq!(rows[0].local_path.as_deref(), Some(expected_path.as_path()));
    assert_eq!(std::fs::read(&old_path).unwrap(), vec![0u8; 1024]);
    assert_eq!(
        std::fs::metadata(&old_path).unwrap().modified().unwrap(),
        old_time
    );
    assert_eq!(
        std::fs::metadata(&expected_path)
            .unwrap()
            .modified()
            .unwrap(),
        std::time::UNIX_EPOCH
            + std::time::Duration::from_secs(asset.created_local().timestamp().unsigned_abs())
    );
    if !cfg!(feature = "xmp") || matches!(mode, ReconciliationMetadataCase::Disabled) {
        assert!(!new_sidecar.exists());
    } else if !matches!(mode, ReconciliationMetadataCase::Missing) {
        assert_eq!(std::fs::read(&source_sidecar).unwrap(), packet);
        assert_eq!(std::fs::read(&new_sidecar).unwrap(), packet);
    } else {
        #[cfg(feature = "xmp")]
        {
            let text = std::fs::read_to_string(&new_sidecar).unwrap();
            let xmp: xmp_toolkit::XmpMeta = text.parse().unwrap();
            assert!(
                xmp.property(xmp_toolkit::xmp_ns::EXIF, "DateTimeOriginal")
                    .is_some()
            );
        }
    }
    let count = std::fs::read_dir(expected_path.parent().unwrap())
        .unwrap()
        .count();
    let sidecar_before = std::fs::read(&new_sidecar).ok();
    let steady = reconcile_catalog_paths(&passes, config, CancellationToken::new())
        .await
        .unwrap();
    assert!(steady.complete);
    assert_eq!(steady.stats.downloaded, 0);
    assert_eq!(
        std::fs::read_dir(expected_path.parent().unwrap())
            .unwrap()
            .count(),
        count
    );
    assert_eq!(std::fs::read(&new_sidecar).ok(), sidecar_before);
}

#[tokio::test]
async fn path_reconciliation_distinct_assets_keep_stable_paths() {
    #[derive(Debug, Clone, Copy)]
    enum CollisionCase {
        EmptyRoot,
        StateFailure,
        LaterAssetAlreadyMoved,
    }

    for case in [
        CollisionCase::EmptyRoot,
        CollisionCase::StateFailure,
        CollisionCase::LaterAssetAlreadyMoved,
    ] {
        let old_dir = TempDir::new().unwrap();
        let new_dir = TempDir::new().unwrap();
        let db_path = old_dir.path().join("state.db");
        let db = Arc::new(crate::state::SqliteStateDb::open(&db_path).await.unwrap());
        let mut provider_records = Vec::new();
        let mut fixtures = Vec::new();
        for (id, byte) in [("COLLIDE_A", 1u8), ("COLLIDE_B", 2u8)] {
            let old_path = old_dir.path().join(format!("IMG_0001-{id}.JPG"));
            std::fs::write(&old_path, vec![byte; 1024]).unwrap();
            let checksum = file::compute_sha256(&old_path).await.unwrap();
            let row = crate::test_helpers::TestAssetRecord::new(id)
                .filename("IMG_0001.JPG")
                .checksum("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
                .size(1024)
                .build();
            db.upsert_seen(&row).await.unwrap();
            db.mark_downloaded("PrimarySync", id, "original", &old_path, &checksum, None)
                .await
                .unwrap();
            db.upsert_asset_master_mapping("PrimarySync", &format!("asset-{id}"), id)
                .await
                .unwrap();
            let packet = format!(
                r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description xmlns:custom="https://example.test/custom/" custom:Note="{id}"/></rdf:RDF></x:xmpmeta>"#
            );
            let sidecar = old_path.with_file_name(format!("IMG_0001-{id}.JPG.xmp"));
            std::fs::write(&sidecar, packet.as_bytes()).unwrap();
            fixtures.push((id, byte, old_path, checksum, sidecar, packet));
            provider_records.extend(mock_photo_records_for_zone_with_filename(
                id,
                "PrimarySync",
                "IMG_0001.JPG",
            ));
        }
        let asset = PhotoAsset::new(provider_records[0].clone(), provider_records[1].clone());
        let passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: album_with_session(
                "PrimarySync",
                "",
                Box::new(PendingLookupSession {
                    records: Arc::new(provider_records),
                }),
            ),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];
        let mut config = test_config();
        config.directory = Arc::from(new_dir.path());
        config.state_db = Some(db.clone());
        #[cfg(feature = "xmp")]
        {
            config.metadata.xmp_sidecar = true;
        }
        let canonical = filter::expected_paths_for(&asset, &config).remove(0).path;
        if matches!(case, CollisionCase::LaterAssetAlreadyMoved) {
            let (id, byte, _, checksum, _, packet) = &fixtures[1];
            std::fs::create_dir_all(canonical.parent().unwrap()).unwrap();
            std::fs::write(&canonical, vec![*byte; 1024]).unwrap();
            #[cfg(feature = "xmp")]
            {
                let mut name = canonical.file_name().unwrap().to_os_string();
                name.push(".xmp");
                std::fs::write(canonical.with_file_name(name), packet.as_bytes()).unwrap();
            }
            #[cfg(not(feature = "xmp"))]
            let _ = packet;
            db.mark_downloaded("PrimarySync", id, "original", &canonical, checksum, None)
                .await
                .unwrap();
        }
        if matches!(case, CollisionCase::StateFailure) {
            db.acquire_lock("inject collision finalization failure").unwrap().execute_batch(
                "CREATE TEMP TRIGGER fail_collision_path BEFORE UPDATE OF local_path ON assets WHEN NEW.id = 'COLLIDE_A' AND NEW.local_path IS NOT OLD.local_path BEGIN SELECT RAISE(FAIL, 'injected collision state failure'); END;"
            ).unwrap();
        }
        let config = Arc::new(config);
        let first = reconcile_catalog_paths(&passes, Arc::clone(&config), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(first.stats.failed, 0, "case {case:?}");
        if matches!(case, CollisionCase::StateFailure) {
            assert!(!first.complete);
            assert_eq!(first.stats.downloaded, 1);
            assert_eq!(first.stats.state_write_failures, 1);
            let reopened = crate::state::SqliteStateDb::open(&db_path).await.unwrap();
            let rows = reopened.get_downloaded_page(0, 10).await.unwrap();
            let failed = rows
                .iter()
                .find(|row| row.id.as_ref() == "COLLIDE_A")
                .unwrap();
            assert_eq!(failed.local_path.as_deref(), Some(fixtures[0].2.as_path()));
            let repeated =
                reconcile_catalog_paths(&passes, Arc::clone(&config), CancellationToken::new())
                    .await
                    .unwrap();
            assert_eq!(repeated.stats.downloaded, 0);
            assert_eq!(repeated.stats.state_write_failures, 1);
            assert_eq!(repeated.stats.failed, 0);
            db.acquire_lock("restore collision finalization")
                .unwrap()
                .execute_batch("DROP TRIGGER fail_collision_path")
                .unwrap();
            let retry =
                reconcile_catalog_paths(&passes, Arc::clone(&config), CancellationToken::new())
                    .await
                    .unwrap();
            assert!(retry.complete);
            assert_eq!(retry.stats.downloaded, 1);
        } else {
            assert!(first.complete, "case {case:?}");
            assert_eq!(
                first.stats.downloaded,
                if matches!(case, CollisionCase::EmptyRoot) {
                    2
                } else {
                    1
                }
            );
        }
        let reopened = crate::state::SqliteStateDb::open(&db_path).await.unwrap();
        let rows = reopened.get_downloaded_page(0, 10).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_ne!(rows[0].local_path, rows[1].local_path);
        for (id, byte, old_path, checksum, old_sidecar, packet) in &fixtures {
            let row = rows.iter().find(|row| row.id.as_ref() == *id).unwrap();
            let path = row.local_path.as_ref().unwrap();
            assert!(path.starts_with(new_dir.path()));
            assert_eq!(row.local_checksum.as_deref(), Some(checksum.as_str()));
            assert_eq!(std::fs::read(path).unwrap(), vec![*byte; 1024]);
            assert_eq!(std::fs::read(old_path).unwrap(), vec![*byte; 1024]);
            assert_eq!(std::fs::read(old_sidecar).unwrap(), packet.as_bytes());
            #[cfg(feature = "xmp")]
            {
                let mut name = path.file_name().unwrap().to_os_string();
                name.push(".xmp");
                assert_eq!(
                    std::fs::read(path.with_file_name(name)).unwrap(),
                    packet.as_bytes()
                );
            }
            if matches!(case, CollisionCase::LaterAssetAlreadyMoved) && *id == "COLLIDE_B" {
                assert_eq!(path, &canonical);
            }
        }
        let visible_entries = || {
            let mut paths: Vec<_> = std::fs::read_dir(canonical.parent().unwrap())
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| {
                    !path
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with(".kei-reconcile-")
                })
                .collect();
            paths.sort();
            paths
        };
        let before = visible_entries();
        assert_eq!(before.len(), if cfg!(feature = "xmp") { 4 } else { 2 });
        let steady = reconcile_catalog_paths(&passes, config, CancellationToken::new())
            .await
            .unwrap();
        assert!(steady.complete);
        assert_eq!(steady.stats.downloaded, 0);
        assert_eq!(visible_entries(), before);
        let steady_rows = reopened.get_downloaded_page(0, 10).await.unwrap();
        for (before, after) in rows.iter().zip(&steady_rows) {
            assert_eq!(after.id, before.id);
            assert_eq!(after.local_path, before.local_path);
            assert_eq!(after.local_checksum, before.local_checksum);
        }
    }
}

#[tokio::test]
async fn path_reconciliation_copies_catalog_file_without_provider_inventory() {
    #[derive(Clone, Debug)]
    struct LookupOnlySession {
        records: Arc<Vec<Value>>,
    }

    #[async_trait::async_trait]
    impl PhotosSession for LookupOnlySession {
        async fn post(
            &self,
            url: &str,
            _body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            if url.contains("/records/lookup?") {
                return Ok(json!({"records": self.records.as_ref().clone()}));
            }
            anyhow::bail!("path reconciliation made an unexpected provider request: {url}")
        }

        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(self.clone())
        }
    }

    let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().expect("state db"));
    let old_dir = TempDir::new().expect("old dir");
    let new_dir = TempDir::new().expect("new dir");
    let old_path = old_dir.path().join("reconcile.jpg");
    tokio::fs::write(&old_path, b"catalog bytes").await.unwrap();
    let local_checksum = file::compute_sha256(&old_path).await.unwrap();
    let record = crate::test_helpers::TestAssetRecord::new("RECONCILE")
        .filename("reconcile.jpg")
        .checksum("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
        .size(1024)
        .build();
    db.upsert_seen(&record).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "RECONCILE",
        "original",
        &old_path,
        &local_checksum,
        Some("provider-checksum"),
    )
    .await
    .unwrap();
    db.upsert_asset_master_mapping("PrimarySync", "asset-RECONCILE", "RECONCILE")
        .await
        .unwrap();

    let mut records =
        mock_photo_records_for_zone_with_filename("RECONCILE", "PrimarySync", "reconcile.jpg");
    records[1]["fields"]["resJPEGFullRes"] = json!({"value": {
        "downloadURL": "https://p01.icloud-content.com/reconcile-adjusted.jpg",
        "size": 512,
        "fileChecksum": "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB="
    }});
    records[1]["fields"]["resJPEGFullFileType"] = json!({"value": "public.jpeg"});
    let asset = PhotoAsset::new(records[0].clone(), records[1].clone());
    let passes = vec![AlbumPass {
        kind: PassKind::Unfiled,
        album: album_with_session(
            "PrimarySync",
            "",
            Box::new(LookupOnlySession {
                records: Arc::new(records),
            }),
        ),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];
    let mut config = test_config();
    config.directory = Arc::from(new_dir.path());
    config.state_db = Some(db.clone());
    config.edited = true;
    let expected_paths = filter::expected_paths_for(&asset, &config);
    let expected_path = expected_paths
        .into_iter()
        .find(|path| path.version_size == VersionSizeKey::Original)
        .unwrap()
        .path;
    let adjusted_path = filter::expected_paths_for(&asset, &config)
        .into_iter()
        .find(|path| path.version_size == VersionSizeKey::Adjusted)
        .unwrap()
        .path;

    let result = reconcile_catalog_paths(&passes, Arc::new(config), CancellationToken::new())
        .await
        .expect("path reconciliation");

    assert!(result.complete);
    assert_eq!(result.stats.downloaded, 1);
    assert_eq!(result.stats.failed, 0);
    assert_eq!(tokio::fs::read(&old_path).await.unwrap(), b"catalog bytes");
    assert_eq!(
        tokio::fs::read(&expected_path).await.unwrap(),
        b"catalog bytes"
    );
    assert!(!adjusted_path.exists());
    let downloaded = db.get_downloaded_page(0, 10).await.unwrap();
    assert_eq!(downloaded.len(), 1);
    assert_eq!(
        downloaded[0].local_path.as_deref(),
        Some(expected_path.as_path())
    );

    tokio::fs::remove_file(&expected_path).await.unwrap();
    let deferred = reconcile_catalog_paths(
        &passes,
        Arc::new({
            let mut config = test_config();
            config.directory = Arc::from(new_dir.path());
            config.state_db = Some(db.clone());
            config.edited = true;
            config
        }),
        CancellationToken::new(),
    )
    .await
    .expect("missing local file should defer to targeted retry");
    assert!(!deferred.complete);
    assert_eq!(deferred.stats.failed, 0);
    let summary = db.get_summary().await.unwrap();
    assert_eq!(summary.pending, 0);
    assert_eq!(summary.failed, 1);
}
