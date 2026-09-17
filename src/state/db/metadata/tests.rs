//! Tests moved from `state::db::tests`, with their original names and assertions.
use std::path::Path;
use std::sync::Arc;

use chrono::DateTime;

use crate::state::db::test_support::test_dir;
use crate::state::db::{
    CaptureRepairReceipt, MetadataRewriteCompletion, MetadataRewriteQueue, SqliteStateDb,
};
use crate::state::error::StateError;
use crate::state::types::{
    AssetMetadata, METADATA_CAPTURE_REVISION, MetadataCapture, VersionSizeKey,
};
use crate::test_helpers::TestAssetRecord;

#[tokio::test]
async fn metadata_paths_finalize_atomically_and_complete_independently() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    let record = TestAssetRecord::new("copies").checksum("provider").build();
    db.upsert_seen(&record).await.unwrap();
    db.acquire_lock("test_path_registration_failure")
        .unwrap()
        .execute_batch(
            "CREATE TEMP TRIGGER fail_path_registration BEFORE INSERT ON asset_metadata_paths \
             BEGIN SELECT RAISE(ABORT, 'path registration failure'); END;",
        )
        .unwrap();
    assert!(
        db.mark_downloaded(
            "PrimarySync",
            "copies",
            "original",
            Path::new("/photos/a.jpg"),
            "local-a",
            None
        )
        .await
        .is_err()
    );
    assert_eq!(
        db.acquire_lock("test_atomic_finalize")
            .unwrap()
            .query_row(
                "SELECT status, local_path FROM assets WHERE id = 'copies'",
                [],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            )
            .unwrap(),
        ("pending".into(), None)
    );
    db.acquire_lock("test_allow_path_registration")
        .unwrap()
        .execute_batch("DROP TRIGGER fail_path_registration")
        .unwrap();
    for (path, checksum) in [("/photos/a.jpg", "local-a"), ("/photos/b.jpg", "local-b")] {
        db.mark_downloaded(
            "PrimarySync",
            "copies",
            "original",
            Path::new(path),
            checksum,
            Some("download"),
        )
        .await
        .unwrap();
    }
    db.record_metadata_write_failure("PrimarySync", "copies", "original")
        .await
        .unwrap();
    let pending = db
        .get_pending_metadata_rewrites_page_for_queue(
            MetadataRewriteQueue::Ordinary,
            Some(&["PrimarySync"]),
            0,
            10,
        )
        .await
        .unwrap();
    assert_eq!(pending.len(), 2);
    assert_eq!(pending[0].asset.local_checksum.as_deref(), Some("local-a"));
    assert_eq!(pending[1].asset.local_checksum.as_deref(), Some("local-b"));
    assert!(
        db.finish_metadata_rewrite(
            &pending[0],
            MetadataRewriteQueue::Ordinary,
            Some("rewritten-a"),
            Some("local-a"),
            MetadataRewriteCompletion::Ordinary
        )
        .await
        .unwrap()
    );
    let remaining = db.get_pending_metadata_rewrites(10).await.unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(
        remaining[0].local_path.as_deref(),
        Some(Path::new("/photos/b.jpg"))
    );
    assert_eq!(remaining[0].local_checksum.as_deref(), Some("local-b"));
    db.record_metadata_write_failure("PrimarySync", "copies", "original")
        .await
        .unwrap();
    assert!(
        !db.finish_metadata_rewrite(
            &pending[0],
            MetadataRewriteQueue::Ordinary,
            Some("stale-a"),
            Some("local-a"),
            MetadataRewriteCompletion::Ordinary
        )
        .await
        .unwrap()
    );
    let copies = db.get_pending_metadata_rewrites(10).await.unwrap();
    assert_eq!(copies.len(), 2);
    assert_eq!(copies[0].local_checksum.as_deref(), Some("rewritten-a"));
    assert_eq!(copies[1].local_checksum.as_deref(), Some("local-b"));
    assert!(
        db.finish_metadata_rewrite(
            &pending[1],
            MetadataRewriteQueue::Ordinary,
            Some("local-b"),
            None,
            MetadataRewriteCompletion::Ordinary
        )
        .await
        .unwrap()
    );
    let only_additional = db.get_pending_metadata_rewrites(1).await.unwrap();
    assert_eq!(only_additional.len(), 1);
    assert_eq!(
        only_additional[0].local_path.as_deref(),
        Some(Path::new("/photos/a.jpg"))
    );
    assert!(
        db.get_metadata_retry_markers().await.unwrap().contains(&(
            "PrimarySync".into(),
            "copies".into(),
            "original".into()
        )),
        "an additional copy must remain visible after the catalogue copy completes"
    );
}

#[tokio::test]
async fn additional_path_prepared_capture_receipt_blocks_stale_metadata() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    let record = TestAssetRecord::new("copies")
        .checksum("provider")
        .metadata(AssetMetadata {
            metadata_hash: Some("metadata-v1".into()),
            ..AssetMetadata::default()
        })
        .build();
    db.upsert_seen(&record).await.unwrap();
    for path in ["/photos/a.jpg", "/photos/b.jpg"] {
        db.mark_downloaded(
            "PrimarySync",
            "copies",
            "original",
            Path::new(path),
            "local",
            None,
        )
        .await
        .unwrap();
    }
    db.refresh_downloaded_asset_metadata(
        "PrimarySync",
        "copies",
        (
            &original_metadata(&record.metadata),
            record.created_at,
            record.added_at,
        ),
        true,
        true,
        METADATA_CAPTURE_REVISION,
    )
    .await
    .unwrap();
    let pending = db
        .get_pending_metadata_rewrites_page_for_queue(
            MetadataRewriteQueue::CaptureRepair,
            None,
            0,
            10,
        )
        .await
        .unwrap();
    assert_eq!(pending.len(), 2);
    assert!(
        db.record_capture_repair_prepared(&pending[0], "prepared", 42)
            .await
            .unwrap()
            .is_some()
    );
    db.mark_downloaded(
        "PrimarySync",
        "copies",
        "original",
        Path::new("/photos/a.jpg"),
        "local",
        None,
    )
    .await
    .unwrap();
    let switched = db
        .get_pending_metadata_rewrites_page_for_queue(
            MetadataRewriteQueue::CaptureRepair,
            None,
            0,
            10,
        )
        .await
        .unwrap();
    assert_eq!(
        switched.len(),
        2,
        "changing the catalogue path must not hide either copy's debt"
    );
    assert!(matches!(
        switched[0].capture_repair_receipt,
        Some(CaptureRepairReceipt::Prepared { .. })
    ));
    db.mark_downloaded(
        "PrimarySync",
        "copies",
        "original",
        Path::new("/photos/b.jpg"),
        "local",
        None,
    )
    .await
    .unwrap();
    let mut changed = (*record.metadata).clone();
    changed.title = Some("changed".into());
    changed.metadata_hash = Some("metadata-v2".into());
    assert_eq!(
        db.refresh_downloaded_asset_metadata(
            "PrimarySync",
            "copies",
            (
                &original_metadata(&changed),
                record.created_at,
                record.added_at
            ),
            true,
            true,
            METADATA_CAPTURE_REVISION
        )
        .await
        .unwrap(),
        0
    );
    let changed_record = TestAssetRecord::new("copies")
        .checksum("provider")
        .metadata(changed)
        .build();
    assert!(db.upsert_seen(&changed_record).await.is_err());
    let preserved = db
        .get_pending_metadata_rewrites_page_for_queue(
            MetadataRewriteQueue::CaptureRepair,
            None,
            0,
            10,
        )
        .await
        .unwrap();
    assert_eq!(
        preserved[0].capture_repair_receipt,
        Some(CaptureRepairReceipt::Prepared {
            metadata_hash: record.metadata.compute_hash(),
            output_checksum: "prepared".into(),
            output_size: 42
        })
    );
    assert_eq!(
        preserved[1].capture_repair_receipt,
        Some(CaptureRepairReceipt::Pending {
            metadata_hash: record.metadata.compute_hash()
        })
    );
}

/// #707 review: new provider media returns a downloaded row to pending and
/// stores the incoming metadata in the same statement. A refresh that then
/// matches no downloaded row has still found its metadata durable, so it
/// must not be reported as a lost write and hold the provider checkpoint.
#[tokio::test]
async fn refresh_reports_metadata_already_durable_on_a_row_that_left_downloaded() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    let mut edited = AssetMetadata {
        is_favorite: true,
        rating: Some(5),
        ..AssetMetadata::default()
    };
    edited.refresh_hash();

    let seeded = TestAssetRecord::new("MOVED_ON").checksum("ck_v1").build();
    db.upsert_seen(&seeded).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "MOVED_ON",
        "original",
        std::path::Path::new("/tmp/moved-on.jpg"),
        "local_v1",
        None,
    )
    .await
    .unwrap();

    // New media for the same asset: the row returns to pending and carries
    // the edited metadata forward.
    let republished = TestAssetRecord::new("MOVED_ON")
        .checksum("ck_v2")
        .metadata(edited.clone())
        .build();
    db.upsert_seen(&republished).await.unwrap();
    let mut medium = TestAssetRecord::new("MOVED_ON")
        .version_size(VersionSizeKey::Medium)
        .created_at(republished.created_at)
        .metadata(AssetMetadata {
            width: Some(1920),
            ..edited.clone()
        })
        .build();
    Arc::make_mut(&mut medium.metadata).refresh_hash();
    db.upsert_seen(&medium).await.unwrap();
    let snapshots = [
        (VersionSizeKey::Original, Arc::new(edited.clone())),
        (VersionSizeKey::Medium, Arc::clone(&medium.metadata)),
    ];

    let matched = db
        .refresh_downloaded_asset_metadata(
            "PrimarySync",
            "MOVED_ON",
            (
                &metadata_capture(&snapshots),
                republished.created_at,
                republished.added_at,
            ),
            true,
            false,
            METADATA_CAPTURE_REVISION,
        )
        .await
        .unwrap();
    assert_eq!(matched, 2, "each rendition's stored metadata is durable");
    db.set_metadata_capture_revision_for_test("PrimarySync", "MOVED_ON", 0);
    assert_eq!(
        db.refresh_downloaded_asset_metadata(
            "PrimarySync",
            "MOVED_ON",
            (
                &metadata_capture(&snapshots[..1]),
                republished.created_at,
                republished.added_at,
            ),
            true,
            false,
            METADATA_CAPTURE_REVISION,
        )
        .await
        .unwrap(),
        0
    );
    for dates in [
        (
            republished.created_at + chrono::Duration::milliseconds(1),
            republished.added_at,
        ),
        (republished.created_at, Some(DateTime::UNIX_EPOCH)),
    ] {
        assert_eq!(
            db.refresh_downloaded_asset_metadata(
                "PrimarySync",
                "MOVED_ON",
                (&metadata_capture(&snapshots), dates.0, dates.1),
                true,
                false,
                METADATA_CAPTURE_REVISION,
            )
            .await
            .unwrap(),
            0,
            "same hash cannot vouch for stale dates"
        );
    }
    db.clear_metadata_hash_for_test("PrimarySync", "MOVED_ON", "medium");
    assert_eq!(
        db.refresh_downloaded_asset_metadata(
            "PrimarySync",
            "MOVED_ON",
            (
                &metadata_capture(&snapshots),
                republished.created_at,
                republished.added_at,
            ),
            true,
            false,
            METADATA_CAPTURE_REVISION,
        )
        .await
        .unwrap(),
        0,
        "one matching pending rendition cannot hide a stale sibling"
    );
    let revision: i64 = db
        .acquire_lock("pending refresh revision")
        .unwrap()
        .query_row(
            "SELECT revision FROM asset_metadata_capture_revisions WHERE asset_id = 'MOVED_ON'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(revision, 0);
    assert_eq!(db.get_pending().await.unwrap().len(), 2);
    assert!(db.get_metadata_retry_markers().await.unwrap().is_empty());

    let stale = db
        .refresh_downloaded_asset_metadata(
            "PrimarySync",
            "ABSENT",
            (
                &original_metadata(&edited),
                republished.created_at,
                republished.added_at,
            ),
            true,
            false,
            METADATA_CAPTURE_REVISION,
        )
        .await
        .unwrap();
    assert_eq!(stale, 0, "a missing row is still a lost write");
}

#[tokio::test]
async fn record_and_clear_metadata_write_failure_roundtrip() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    let rec = TestAssetRecord::new("MWF_1").build();
    db.upsert_seen(&rec).await.unwrap();

    // Initially, the marker column is NULL.
    let ts_initial: Option<i64> = {
        let conn = db.acquire_lock("test").unwrap();
        conn.query_row(
            "SELECT metadata_write_failed_at FROM assets WHERE id = 'MWF_1'",
            [],
            |row| row.get(0),
        )
        .unwrap()
    };
    assert!(ts_initial.is_none());

    // Set the marker.
    db.record_metadata_write_failure("PrimarySync", "MWF_1", "original")
        .await
        .unwrap();
    let ts_after_set: Option<i64> = {
        let conn = db.acquire_lock("test").unwrap();
        conn.query_row(
            "SELECT metadata_write_failed_at FROM assets WHERE id = 'MWF_1'",
            [],
            |row| row.get(0),
        )
        .unwrap()
    };
    assert!(
        ts_after_set.is_some(),
        "marker should be set after record_metadata_write_failure"
    );

    // Clear the marker after a successful retry.
    db.clear_metadata_write_failure("PrimarySync", "MWF_1", "original")
        .await
        .unwrap();
    let ts_after_clear: Option<i64> = {
        let conn = db.acquire_lock("test").unwrap();
        conn.query_row(
            "SELECT metadata_write_failed_at FROM assets WHERE id = 'MWF_1'",
            [],
            |row| row.get(0),
        )
        .unwrap()
    };
    assert!(
        ts_after_clear.is_none(),
        "marker should be cleared after clear_metadata_write_failure"
    );
}

#[tokio::test]
async fn has_downloaded_without_metadata_hash_returns_false_on_empty() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    assert!(!db.has_downloaded_without_metadata_hash().await.unwrap());
}

#[tokio::test]
async fn has_downloaded_without_metadata_hash_skips_pending() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    let rec = TestAssetRecord::new("P1").build();
    db.upsert_seen(&rec).await.unwrap();
    assert!(!db.has_downloaded_without_metadata_hash().await.unwrap());
}

#[tokio::test]
async fn has_downloaded_without_metadata_hash_detects_missing_hash() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    let rec = TestAssetRecord::new("D1").build();
    db.upsert_seen(&rec).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "D1",
        "original",
        Path::new("/a.jpg"),
        "h",
        None,
    )
    .await
    .unwrap();
    // Manually null the hash to simulate a pre-v5 row
    {
        let conn = db.acquire_lock("test").unwrap();
        conn.execute("UPDATE assets SET metadata_hash = NULL WHERE id = 'D1'", [])
            .unwrap();
    }
    assert!(db.has_downloaded_without_metadata_hash().await.unwrap());
}

#[tokio::test]
async fn has_downloaded_without_metadata_hash_skips_soft_deleted() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    let rec = TestAssetRecord::new("D1").build();
    db.upsert_seen(&rec).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "D1",
        "original",
        Path::new("/a.jpg"),
        "h",
        None,
    )
    .await
    .unwrap();
    {
        let conn = db.acquire_lock("test").unwrap();
        conn.execute("UPDATE assets SET metadata_hash = NULL WHERE id = 'D1'", [])
            .unwrap();
    }
    db.mark_soft_deleted("PrimarySync", "D1", None)
        .await
        .unwrap();
    // A soft-deleted row is never re-enumerated, so its NULL hash must not drive full enumeration.
    assert!(!db.has_downloaded_without_metadata_hash().await.unwrap());
}

#[tokio::test]
async fn refresh_downloaded_asset_metadata_updates_every_live_downloaded_version() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    let snapshots: Vec<_> = [
        (VersionSizeKey::Original, 4032, 3024, None),
        (VersionSizeKey::Medium, 1920, 1440, None),
        (VersionSizeKey::Thumb, 320, 240, None),
        (VersionSizeKey::Adjusted, 3000, 2000, None),
        (VersionSizeKey::Alternative, 4000, 3000, None),
        (VersionSizeKey::LiveOriginal, 1920, 1080, Some(3.0)),
        (VersionSizeKey::LiveMedium, 1280, 720, Some(2.5)),
        (VersionSizeKey::LiveThumb, 640, 360, Some(2.0)),
        (VersionSizeKey::LiveAdjusted, 1600, 900, Some(1.5)),
    ]
    .into_iter()
    .map(|(version, width, height, duration_secs)| {
        (
            version,
            Arc::new(AssetMetadata {
                width: Some(width),
                height: Some(height),
                duration_secs,
                ..AssetMetadata::default()
            }),
        )
    })
    .collect();
    for (version, metadata) in &snapshots {
        let record = TestAssetRecord::new("PHOTO")
            .version_size(*version)
            .metadata((**metadata).clone())
            .build();
        db.upsert_seen(&record).await.unwrap();
        for path in ["/copy.jpg", "/photo.jpg"] {
            db.mark_downloaded(
                "PrimarySync",
                "PHOTO",
                version.as_str(),
                Path::new(path),
                "checksum",
                None,
            )
            .await
            .unwrap();
        }
    }
    for record in db.get_downloaded_page(0, 10).await.unwrap() {
        let expected = &snapshots
            .iter()
            .find(|(version, _)| *version == record.version_size)
            .unwrap()
            .1;
        assert_eq!(record.metadata.width, expected.width);
        assert_eq!(record.metadata.height, expected.height);
        assert_eq!(record.metadata.duration_secs, expected.duration_secs);
        assert_eq!(record.metadata.metadata_hash, Some(expected.compute_hash()));
    }
    let mut snapshots: Vec<_> = snapshots
        .into_iter()
        .map(|(version, mut metadata)| {
            let edited = Arc::make_mut(&mut metadata);
            edited.rating = Some(4);
            edited.width = edited.width.map(|width| width / 2);
            edited.height = edited.height.map(|height| height / 2);
            edited.duration_secs = edited.duration_secs.map(|duration| duration / 2.0);
            (version, metadata)
        })
        .collect();
    let created = DateTime::from_timestamp_millis(1_700_000_000_123).unwrap();
    let added = DateTime::from_timestamp_millis(-1).unwrap();
    {
        let conn = db.acquire_lock("inject refresh commit failure").unwrap();
        conn.execute_batch(
            "CREATE TRIGGER fail_capture_revision BEFORE INSERT ON asset_metadata_capture_revisions
                 BEGIN SELECT RAISE(FAIL, 'injected revision failure'); END;",
        )
        .unwrap();
    }
    db.refresh_downloaded_asset_metadata(
        "PrimarySync",
        "PHOTO",
        (&metadata_capture(&snapshots), created, Some(added)),
        true,
        true,
        METADATA_CAPTURE_REVISION,
    )
    .await
    .unwrap_err();
    let unchanged = db.get_downloaded_page(0, 30).await.unwrap();
    assert!(unchanged.iter().all(|row| row.created_at != created
        && row.added_at.is_none()
        && row.metadata.rating.is_none()));
    for queue in [
        MetadataRewriteQueue::Ordinary,
        MetadataRewriteQueue::CaptureRepair,
    ] {
        assert!(
            db.get_pending_metadata_rewrites_page_for_queue(queue, None, 0, 30)
                .await
                .unwrap()
                .is_empty()
        );
    }
    db.acquire_lock("remove refresh failure")
        .unwrap()
        .execute_batch("DROP TRIGGER fail_capture_revision")
        .unwrap();
    let updated = db
        .refresh_downloaded_asset_metadata(
            "PrimarySync",
            "PHOTO",
            (&metadata_capture(&snapshots), created, Some(added)),
            true,
            true,
            METADATA_CAPTURE_REVISION,
        )
        .await
        .unwrap();
    assert_eq!(updated, snapshots.len());

    // Ordinary refresh also advances existing pending capture intent on
    // every path, without requiring another explicit capture-repair flag.
    for (_, metadata) in &mut snapshots {
        Arc::make_mut(metadata).title = Some("edited".into());
    }
    assert_eq!(
        db.refresh_downloaded_asset_metadata(
            "PrimarySync",
            "PHOTO",
            (&metadata_capture(&snapshots), created, Some(added)),
            false,
            false,
            METADATA_CAPTURE_REVISION,
        )
        .await
        .unwrap(),
        snapshots.len()
    );

    let rewrites = db
        .get_pending_metadata_rewrites_page_for_queue(
            MetadataRewriteQueue::CaptureRepair,
            None,
            0,
            30,
        )
        .await
        .unwrap();
    assert_eq!(rewrites.len(), snapshots.len() * 2);
    for pending in &rewrites {
        let record = &pending.asset;
        let expected = &snapshots
            .iter()
            .find(|(version, _)| *version == record.version_size)
            .unwrap()
            .1;
        assert_eq!(record.library.as_ref(), "PrimarySync");
        assert_eq!(record.id.as_ref(), "PHOTO");
        assert_eq!(record.created_at, created);
        assert_eq!(record.added_at, Some(added));
        assert_eq!(record.metadata.rating, Some(4));
        assert_eq!(record.metadata.width, expected.width);
        assert_eq!(record.metadata.height, expected.height);
        assert_eq!(record.metadata.duration_secs, expected.duration_secs);
        assert_eq!(record.metadata.metadata_hash, Some(expected.compute_hash()));
        assert_eq!(
            pending.capture_repair_receipt,
            Some(CaptureRepairReceipt::Pending {
                metadata_hash: expected.compute_hash(),
            })
        );
        assert!(
            db.record_capture_repair_prepared(pending, "prepared", 2048)
                .await
                .unwrap()
                .is_some()
        );
    }
    // Different hashes are compatible when every prepared receipt still
    // matches its own rendition, including the additional tracked copies.
    assert_ne!(snapshots[0].1.compute_hash(), snapshots[5].1.compute_hash());
    for _ in 0..2 {
        for (version, metadata) in &snapshots {
            let record = TestAssetRecord::new("PHOTO")
                .version_size(*version)
                .created_at(created)
                .added_at(added)
                .metadata((**metadata).clone())
                .build();
            db.upsert_seen(&record).await.unwrap();
        }
        let preserved = db
            .get_pending_metadata_rewrites_page_for_queue(
                MetadataRewriteQueue::CaptureRepair,
                None,
                0,
                30,
            )
            .await
            .unwrap();
        assert_eq!(preserved.len(), rewrites.len());
        for pending in preserved {
            assert_eq!(
                pending.capture_repair_receipt,
                Some(CaptureRepairReceipt::Prepared {
                    metadata_hash: pending.asset.metadata.compute_hash(),
                    output_checksum: "prepared".into(),
                    output_size: 2048,
                })
            );
        }
    }
    assert_eq!(
        db.refresh_downloaded_asset_metadata(
            "PrimarySync",
            "PHOTO",
            (&metadata_capture(&snapshots), created, Some(added)),
            false,
            false,
            METADATA_CAPTURE_REVISION,
        )
        .await
        .unwrap(),
        snapshots.len()
    );
    for index in 0..snapshots.len() {
        let mut changed = snapshots.clone();
        Arc::make_mut(&mut changed[index].1).width = Some(1);
        assert_eq!(
            db.refresh_downloaded_asset_metadata(
                "PrimarySync",
                "PHOTO",
                (&metadata_capture(&changed), created, Some(added)),
                true,
                true,
                METADATA_CAPTURE_REVISION,
            )
            .await
            .unwrap(),
            0,
            "any rendition's prepared receipt must block the entire refresh"
        );
    }
    let prepared = db
        .get_pending_metadata_rewrites_page_for_queue(
            MetadataRewriteQueue::CaptureRepair,
            None,
            0,
            30,
        )
        .await
        .unwrap();
    for pending in &prepared {
        assert_eq!(
            pending.capture_repair_receipt,
            Some(CaptureRepairReceipt::Prepared {
                metadata_hash: pending.asset.metadata.compute_hash(),
                output_checksum: "prepared".into(),
                output_size: 2048,
            })
        );
    }

    // The refresh must not disturb download state: #707 requires every
    // live downloaded version to keep its status, local path, checksums
    // and download timestamp while its metadata is replaced.
    let downloaded = db.get_downloaded_page(0, 10).await.unwrap();
    assert_eq!(downloaded.len(), snapshots.len());
    for record in &downloaded {
        assert_eq!(record.local_path.as_deref(), Some(Path::new("/photo.jpg")));
        assert_eq!(record.local_checksum.as_deref(), Some("checksum"));
        assert_eq!(record.checksum.as_ref(), "checksum123");
        assert_eq!(record.metadata.rating, Some(4));
    }
}

fn original_metadata(metadata: &AssetMetadata) -> MetadataCapture {
    metadata_capture(&[(VersionSizeKey::Original, Arc::new(metadata.clone()))])
}

#[tokio::test]
async fn metadata_refresh_resolves_current_checksum_and_newly_completed_raw_sibling() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    let original = TestAssetRecord::new("RAW").checksum("jpeg").build();
    db.upsert_seen(&original).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "RAW",
        "original",
        Path::new("/photo.jpg"),
        "local-jpeg",
        None,
    )
    .await
    .unwrap();
    let capture = MetadataCapture {
        shared: Arc::new(AssetMetadata {
            title: Some("new title".into()),
            ..AssetMetadata::default()
        }),
        renditions: [VersionSizeKey::Original, VersionSizeKey::Alternative]
            .into_iter()
            .flat_map(|key| {
                [("jpeg", 4000), ("raw", 8000)].map(|(checksum, width)| {
                    (
                        key,
                        crate::state::RenditionMetadata {
                            checksum: Some(Arc::from(checksum)),
                            width: Some(width),
                            height: Some(width / 2),
                            duration_secs: None,
                        },
                    )
                })
            })
            .collect(),
    };
    // Planning saw only the JPEG. Replacement and sibling completion happen
    // before refresh acquires its transaction, so that preload is not authority.
    let replacement = TestAssetRecord::new("RAW").checksum("raw").build();
    db.upsert_seen(&replacement).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "RAW",
        "original",
        Path::new("/photo.dng"),
        "local-raw",
        None,
    )
    .await
    .unwrap();
    let sibling = TestAssetRecord::new("RAW")
        .version_size(VersionSizeKey::Alternative)
        .checksum("jpeg")
        .build();
    db.upsert_seen(&sibling).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "RAW",
        "alternative",
        Path::new("/photo.jpg"),
        "local-jpeg",
        None,
    )
    .await
    .unwrap();
    for _ in 0..2 {
        assert_eq!(
            db.refresh_downloaded_asset_metadata(
                "PrimarySync",
                "RAW",
                (&capture, DateTime::UNIX_EPOCH, None),
                false,
                false,
                METADATA_CAPTURE_REVISION
            )
            .await
            .unwrap(),
            2
        );
        for row in db.get_downloaded_page(0, 10).await.unwrap() {
            let expected = capture.resolve(row.version_size, &row.checksum);
            assert_eq!(row.metadata.metadata_hash, expected.metadata_hash);
            assert_eq!(
                row.metadata.width,
                Some(if row.version_size == VersionSizeKey::Original {
                    8000
                } else {
                    4000
                })
            );
        }
        assert!(db.get_metadata_retry_markers().await.unwrap().is_empty());
    }
}

#[tokio::test]
async fn metadata_refresh_late_failures_roll_back_family_paths_and_revision_on_reopen() {
    for phase in ["paths", "revision", "progress"] {
        let dir = test_dir();
        let path = dir.path().join("state.db");
        let db = SqliteStateDb::open(&path).await.unwrap();
        let snapshots: Vec<_> = [VersionSizeKey::Original, VersionSizeKey::LiveOriginal]
            .into_iter()
            .map(|key| {
                (
                    key,
                    Arc::new(AssetMetadata {
                        width: Some(if key == VersionSizeKey::Original {
                            4000
                        } else {
                            1920
                        }),
                        height: Some(1080),
                        duration_secs: (key == VersionSizeKey::LiveOriginal).then_some(2.3),
                        ..AssetMetadata::default()
                    }),
                )
            })
            .collect();
        for (key, metadata) in &snapshots {
            let record = TestAssetRecord::new("FAMILY")
                .version_size(*key)
                .metadata((**metadata).clone())
                .build();
            db.upsert_seen(&record).await.unwrap();
            for copy in ["first", "second"] {
                let media = dir.path().join(format!("{copy}-{}", key.as_str()));
                db.mark_downloaded("PrimarySync", "FAMILY", key.as_str(), &media, "local", None)
                    .await
                    .unwrap();
            }
        }
        db.refresh_downloaded_asset_metadata(
            "PrimarySync",
            "FAMILY",
            (&metadata_capture(&snapshots), DateTime::UNIX_EPOCH, None),
            true,
            true,
            METADATA_CAPTURE_REVISION,
        )
        .await
        .unwrap();
        db.set_metadata_capture_revision_for_test("PrimarySync", "FAMILY", 0);
        db.begin_metadata_capture_revision("PrimarySync", METADATA_CAPTURE_REVISION)
            .await
            .unwrap();
        let dump = |db: &SqliteStateDb| {
            let conn = db.acquire_lock("family durable snapshot").unwrap();
            [
                "assets",
                "asset_metadata_paths",
                "asset_metadata_capture_revisions",
                "metadata_capture_state",
            ]
            .map(|table| {
                let mut stmt = conn
                    .prepare(&format!("SELECT * FROM {table} ORDER BY 1, 2, 3"))
                    .unwrap();
                let columns = stmt.column_count();
                stmt.query_map([], |row| {
                    (0..columns)
                        .map(|column| row.get::<_, rusqlite::types::Value>(column))
                        .collect::<Result<Vec<_>, _>>()
                })
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
            })
        };
        let before = dump(&db);
        let mut capture = metadata_capture(&snapshots);
        Arc::make_mut(&mut capture.shared).title = Some("refreshed title".into());
        let trigger = match phase {
            "paths" => "AFTER UPDATE ON asset_metadata_paths",
            "revision" => "AFTER UPDATE ON asset_metadata_capture_revisions",
            _ => "AFTER UPDATE ON metadata_capture_state",
        };
        db.acquire_lock("inject late refresh failure").unwrap().execute_batch(&format!(
            "CREATE TEMP TRIGGER fail_late_refresh {trigger} BEGIN SELECT RAISE(ABORT, 'late refresh failure'); END;"
        )).unwrap();
        let error = db
            .refresh_downloaded_asset_metadata(
                "PrimarySync",
                "FAMILY",
                (&capture, DateTime::UNIX_EPOCH, None),
                true,
                true,
                METADATA_CAPTURE_REVISION,
            )
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("late refresh failure"),
            "{phase}: {error}"
        );
        drop(db);
        let db = SqliteStateDb::open(&path).await.unwrap();
        assert_eq!(
            dump(&db),
            before,
            "{phase}: every prior write must roll back"
        );
        assert_eq!(
            db.refresh_downloaded_asset_metadata(
                "PrimarySync",
                "FAMILY",
                (&capture, DateTime::UNIX_EPOCH, None),
                true,
                true,
                METADATA_CAPTURE_REVISION
            )
            .await
            .unwrap(),
            2
        );
        let pending_paths = db
            .get_pending_metadata_rewrites_page_for_queue(
                MetadataRewriteQueue::Ordinary,
                None,
                0,
                10,
            )
            .await
            .unwrap();
        assert_eq!(pending_paths.len(), 4);
        for pending in pending_paths {
            assert_eq!(
                pending.asset.metadata.title.as_deref(),
                Some("refreshed title")
            );
            assert_eq!(
                pending.capture_repair_receipt,
                Some(CaptureRepairReceipt::Pending {
                    metadata_hash: capture
                        .resolve(pending.asset.version_size, &pending.asset.checksum)
                        .compute_hash(),
                })
            );
            assert!(
                db.finish_metadata_rewrite(
                    &pending,
                    MetadataRewriteQueue::Ordinary,
                    Some("local"),
                    None,
                    MetadataRewriteCompletion::Both
                )
                .await
                .unwrap()
            );
        }
        assert!(
            db.get_pending_metadata_rewrites(10)
                .await
                .unwrap()
                .is_empty()
        );
        let completed = db
            .complete_metadata_capture_revision("PrimarySync", METADATA_CAPTURE_REVISION)
            .await
            .unwrap();
        assert_eq!(completed.remaining_assets, 0);
        assert_eq!(completed.processed_assets, 1);
        let steady = dump(&db);
        assert_eq!(
            db.refresh_downloaded_asset_metadata(
                "PrimarySync",
                "FAMILY",
                (&capture, DateTime::UNIX_EPOCH, None),
                false,
                false,
                METADATA_CAPTURE_REVISION
            )
            .await
            .unwrap(),
            2
        );
        assert_eq!(
            dump(&db),
            steady,
            "{phase}: unchanged followup creates no debt or transitions"
        );
    }
}

fn metadata_capture(snapshots: &[(VersionSizeKey, Arc<AssetMetadata>)]) -> MetadataCapture {
    MetadataCapture {
        shared: Arc::clone(&snapshots[0].1),
        renditions: snapshots
            .iter()
            .map(|(key, metadata)| {
                (
                    *key,
                    crate::state::RenditionMetadata {
                        checksum: Some(Arc::from("checksum123")),
                        width: metadata.width,
                        height: metadata.height,
                        duration_secs: metadata.duration_secs,
                    },
                )
            })
            .collect(),
    }
}

#[tokio::test]
async fn metadata_refresh_accepts_missing_but_rejects_unknown_renditions_without_partial_writes() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    let snapshots = [
        (
            VersionSizeKey::Original,
            Arc::new(AssetMetadata {
                title: Some("shared edit".into()),
                width: Some(4000),
                height: Some(3000),
                duration_secs: Some(9.0),
                ..AssetMetadata::default()
            }),
        ),
        (
            VersionSizeKey::Medium,
            Arc::new(AssetMetadata {
                width: Some(1920),
                height: Some(1080),
                duration_secs: Some(3.0),
                ..AssetMetadata::default()
            }),
        ),
    ];
    for (version, metadata) in &snapshots {
        let record = TestAssetRecord::new("INCOMPLETE")
            .version_size(*version)
            .metadata((**metadata).clone())
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "INCOMPLETE",
            version.as_str(),
            Path::new("/photo.jpg"),
            "local",
            None,
        )
        .await
        .unwrap();
    }
    db.set_metadata_capture_revision_for_test("PrimarySync", "INCOMPLETE", 0);
    for unknown in [false, true] {
        if unknown {
            db.acquire_lock("unknown rendition")
                .unwrap()
                .execute(
                    "UPDATE assets SET version_size = 'unknown' WHERE version_size = 'medium'",
                    [],
                )
                .unwrap();
        }
        let refreshed = db
            .refresh_downloaded_asset_metadata(
                "PrimarySync",
                "INCOMPLETE",
                (
                    &metadata_capture(&snapshots[..1]),
                    DateTime::UNIX_EPOCH,
                    None,
                ),
                true,
                true,
                METADATA_CAPTURE_REVISION,
            )
            .await;
        if !unknown {
            assert_eq!(refreshed.unwrap(), 2);
            let rows = db.get_downloaded_page(0, 10).await.unwrap();
            let medium = rows
                .iter()
                .find(|row| row.version_size == VersionSizeKey::Medium)
                .unwrap();
            assert_eq!(
                (
                    medium.metadata.width,
                    medium.metadata.height,
                    medium.metadata.duration_secs
                ),
                (None, None, None)
            );
            assert_eq!(medium.metadata.title.as_deref(), Some("shared edit"));
            assert_eq!(
                medium.metadata.metadata_hash,
                Some(medium.metadata.compute_hash())
            );
        } else {
            assert!(matches!(refreshed, Err(StateError::Invariant { .. })));
        }
        let conn = db.acquire_lock("no partial metadata refresh").unwrap();
        let changed: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM assets WHERE metadata_write_failed_at IS NOT NULL \
                     OR capture_repair_metadata_hash IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(changed, 2);
        let revision: i64 = conn
            .query_row(
                "SELECT revision FROM asset_metadata_capture_revisions WHERE asset_id = 'INCOMPLETE'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(revision, METADATA_CAPTURE_REVISION);
    }
}

async fn seed_downloaded_capture_asset(
    db: &SqliteStateDb,
    id: &str,
    metadata_hash: &str,
    provider_checksum: &str,
    local_checksum: &str,
) {
    let record = TestAssetRecord::new(id)
        .checksum(provider_checksum)
        .metadata(AssetMetadata {
            metadata_hash: Some(metadata_hash.to_owned()),
            ..AssetMetadata::default()
        })
        .build();
    db.upsert_seen(&record).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        id,
        "original",
        Path::new("/photos/capture.jpg"),
        local_checksum,
        None,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn capture_repair_receipt_is_guarded_and_markers_retire_independently() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    seed_downloaded_capture_asset(&db, "CAPTURE", "metadata-v1", "provider-v1", "local-v1").await;
    let metadata = AssetMetadata {
        metadata_hash: Some("metadata-v1".into()),
        ..AssetMetadata::default()
    };
    db.refresh_downloaded_asset_metadata(
        "PrimarySync",
        "CAPTURE",
        (&original_metadata(&metadata), DateTime::UNIX_EPOCH, None),
        true,
        true,
        METADATA_CAPTURE_REVISION,
    )
    .await
    .unwrap();

    let mut capture = db
        .get_pending_metadata_rewrites_page_for_queue(
            MetadataRewriteQueue::CaptureRepair,
            None,
            0,
            10,
        )
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(
        capture.capture_repair_receipt,
        Some(CaptureRepairReceipt::Pending {
            metadata_hash: metadata.compute_hash()
        })
    );
    let prepared = db
        .record_capture_repair_prepared(&capture, "local-v2", 2048)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        prepared,
        CaptureRepairReceipt::Prepared {
            metadata_hash: metadata.compute_hash(),
            output_checksum: "local-v2".into(),
            output_size: 2048,
        }
    );
    capture.capture_repair_receipt = Some(prepared);
    capture = db
        .get_pending_metadata_rewrites_page_for_queue(
            MetadataRewriteQueue::CaptureRepair,
            None,
            0,
            10,
        )
        .await
        .unwrap()
        .pop()
        .unwrap();
    let reprepared = db
        .record_capture_repair_prepared(&capture, "local-v3", 3072)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        reprepared,
        CaptureRepairReceipt::Prepared {
            metadata_hash: metadata.compute_hash(),
            output_checksum: "local-v3".into(),
            output_size: 3072,
        },
        "a crash-left prepared receipt may be replaced while input bytes still match"
    );
    capture.capture_repair_receipt = Some(reprepared);

    assert!(
        db.finish_metadata_rewrite(
            &capture,
            MetadataRewriteQueue::CaptureRepair,
            Some("local-v3"),
            Some("local-v1"),
            MetadataRewriteCompletion::CaptureRepair,
        )
        .await
        .unwrap()
    );
    assert!(
        db.get_pending_metadata_rewrites_page_for_queue(
            MetadataRewriteQueue::CaptureRepair,
            None,
            0,
            10,
        )
        .await
        .unwrap()
        .is_empty()
    );
    let ordinary = db
        .get_pending_metadata_rewrites_page_for_queue(MetadataRewriteQueue::Ordinary, None, 0, 10)
        .await
        .unwrap();
    assert_eq!(
        ordinary.len(),
        1,
        "capture completion must keep generic debt"
    );
    assert!(
        db.finish_metadata_rewrite(
            &ordinary[0],
            MetadataRewriteQueue::Ordinary,
            Some("local-v3"),
            None,
            MetadataRewriteCompletion::Ordinary,
        )
        .await
        .unwrap()
    );
    assert!(
        db.get_pending_metadata_rewrites(10)
            .await
            .unwrap()
            .is_empty()
    );

    let downloaded = db.get_downloaded_page(0, 10).await.unwrap();
    assert_eq!(downloaded[0].local_checksum.as_deref(), Some("local-v3"));
    assert_eq!(downloaded[0].download_checksum.as_deref(), Some("local-v1"));

    db.refresh_downloaded_asset_metadata(
        "PrimarySync",
        "CAPTURE",
        (&original_metadata(&metadata), DateTime::UNIX_EPOCH, None),
        false,
        true,
        METADATA_CAPTURE_REVISION,
    )
    .await
    .unwrap();
    let pending_no_write = db
        .get_pending_metadata_rewrites_page_for_queue(
            MetadataRewriteQueue::CaptureRepair,
            None,
            0,
            10,
        )
        .await
        .unwrap()
        .pop()
        .unwrap();
    db.record_capture_repair_prepared(&pending_no_write, "unused-output", 3072)
        .await
        .unwrap()
        .unwrap();
    let prepared_no_write = db
        .get_pending_metadata_rewrites_page_for_queue(
            MetadataRewriteQueue::CaptureRepair,
            None,
            0,
            10,
        )
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert!(
        db.finish_metadata_rewrite(
            &prepared_no_write,
            MetadataRewriteQueue::CaptureRepair,
            Some("local-v3"),
            None,
            MetadataRewriteCompletion::CaptureRepair,
        )
        .await
        .unwrap(),
        "verified no-write completion may retain the selected input checksum"
    );
    assert!(
        db.get_pending_metadata_rewrites_page_for_queue(
            MetadataRewriteQueue::CaptureRepair,
            None,
            0,
            10,
        )
        .await
        .unwrap()
        .is_empty()
    );

    db.refresh_downloaded_asset_metadata(
        "PrimarySync",
        "CAPTURE",
        (&original_metadata(&metadata), DateTime::UNIX_EPOCH, None),
        true,
        true,
        METADATA_CAPTURE_REVISION,
    )
    .await
    .unwrap();
    let both = db
        .get_pending_metadata_rewrites_page_for_queue(MetadataRewriteQueue::Ordinary, None, 0, 10)
        .await
        .unwrap();
    assert!(
        db.finish_metadata_rewrite(
            &both[0],
            MetadataRewriteQueue::Ordinary,
            Some("local-v3"),
            None,
            MetadataRewriteCompletion::Both,
        )
        .await
        .unwrap()
    );
    assert!(
        db.get_pending_metadata_rewrites(10)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        db.get_pending_metadata_rewrites_page_for_queue(
            MetadataRewriteQueue::CaptureRepair,
            None,
            0,
            10,
        )
        .await
        .unwrap()
        .is_empty()
    );
}

#[tokio::test]
async fn capture_repair_refresh_preserves_receipt_until_publication_is_finalised() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    seed_downloaded_capture_asset(&db, "REFRESH", "metadata-v1", "provider-v1", "local-v1").await;

    let unchanged = AssetMetadata {
        metadata_hash: Some("metadata-v1".into()),
        ..AssetMetadata::default()
    };
    db.refresh_downloaded_asset_metadata(
        "PrimarySync",
        "REFRESH",
        (&original_metadata(&unchanged), DateTime::UNIX_EPOCH, None),
        true,
        false,
        METADATA_CAPTURE_REVISION,
    )
    .await
    .unwrap();
    assert!(
        db.get_pending_metadata_rewrites_page_for_queue(
            MetadataRewriteQueue::CaptureRepair,
            None,
            0,
            10,
        )
        .await
        .unwrap()
        .is_empty(),
        "ordinary refresh must not create capture debt"
    );

    db.refresh_downloaded_asset_metadata(
        "PrimarySync",
        "REFRESH",
        (&original_metadata(&unchanged), DateTime::UNIX_EPOCH, None),
        true,
        true,
        METADATA_CAPTURE_REVISION,
    )
    .await
    .unwrap();
    let mut pending = db
        .get_pending_metadata_rewrites_page_for_queue(
            MetadataRewriteQueue::CaptureRepair,
            None,
            0,
            10,
        )
        .await
        .unwrap()
        .pop()
        .unwrap();
    let stale_pending = pending.clone();
    pending.capture_repair_receipt = db
        .record_capture_repair_prepared(&pending, "local-v2", 2048)
        .await
        .unwrap();

    let mut addition_only = pending.asset.clone();
    addition_only.added_at = Some(DateTime::from_timestamp_millis(1).unwrap());
    db.upsert_seen(&addition_only).await.unwrap();
    let stored = db.get_downloaded_page(0, 10).await.unwrap();
    assert_eq!(stored[0].added_at, addition_only.added_at);
    db.refresh_downloaded_asset_metadata(
        "PrimarySync",
        "REFRESH",
        (&original_metadata(&unchanged), DateTime::UNIX_EPOCH, None),
        false,
        false,
        METADATA_CAPTURE_REVISION,
    )
    .await
    .unwrap();
    let preserved = db
        .get_pending_metadata_rewrites_page_for_queue(
            MetadataRewriteQueue::CaptureRepair,
            None,
            0,
            10,
        )
        .await
        .unwrap();
    assert_eq!(
        preserved[0].capture_repair_receipt,
        pending.capture_repair_receipt
    );
    assert_eq!(preserved[0].asset.created_at, pending.asset.created_at);
    assert_eq!(preserved[0].asset.added_at, None);

    let ordinary = db
        .get_pending_metadata_rewrites_page_for_queue(MetadataRewriteQueue::Ordinary, None, 0, 10)
        .await
        .unwrap();
    assert!(
        !db.finish_metadata_rewrite(
            &ordinary[0],
            MetadataRewriteQueue::Ordinary,
            Some("local-ordinary"),
            Some("local-v1"),
            MetadataRewriteCompletion::None,
        )
        .await
        .unwrap(),
        "checksum-only completion must not retire the selected queue"
    );
    let ordinary_after_checksum = db
        .get_pending_metadata_rewrites_page_for_queue(MetadataRewriteQueue::Ordinary, None, 0, 10)
        .await
        .unwrap();
    assert_eq!(ordinary_after_checksum.len(), 1);
    assert_eq!(
        ordinary_after_checksum[0].asset.local_checksum.as_deref(),
        Some("local-ordinary")
    );
    assert_eq!(
        ordinary_after_checksum[0]
            .asset
            .download_checksum
            .as_deref(),
        Some("local-v1")
    );
    assert!(
        db.finish_metadata_rewrite(
            &ordinary_after_checksum[0],
            MetadataRewriteQueue::Ordinary,
            Some("local-ordinary"),
            None,
            MetadataRewriteCompletion::Ordinary,
        )
        .await
        .unwrap()
    );
    let mut pending_after_ordinary = db
        .get_pending_metadata_rewrites_page_for_queue(
            MetadataRewriteQueue::CaptureRepair,
            None,
            0,
            10,
        )
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(
        pending_after_ordinary.capture_repair_receipt,
        Some(CaptureRepairReceipt::Pending {
            metadata_hash: unchanged.compute_hash()
        }),
        "ordinary checksum changes must preserve capture intent as pending"
    );
    pending_after_ordinary.capture_repair_receipt = db
        .record_capture_repair_prepared(&pending_after_ordinary, "local-v2", 2048)
        .await
        .unwrap();

    let changed = AssetMetadata {
        metadata_hash: Some("metadata-v2".into()),
        rating: Some(5),
        ..AssetMetadata::default()
    };
    assert_eq!(
        db.refresh_downloaded_asset_metadata(
            "PrimarySync",
            "REFRESH",
            (&original_metadata(&changed), DateTime::UNIX_EPOCH, None),
            false,
            false,
            METADATA_CAPTURE_REVISION,
        )
        .await
        .unwrap(),
        0,
        "new provider metadata must wait until published repair bytes are finalised"
    );
    let preserved_after_change = db
        .get_pending_metadata_rewrites_page_for_queue(
            MetadataRewriteQueue::CaptureRepair,
            None,
            0,
            10,
        )
        .await
        .unwrap();
    assert_eq!(
        preserved_after_change[0].capture_repair_receipt,
        pending_after_ordinary.capture_repair_receipt
    );
    assert!(
        db.record_capture_repair_prepared(&stale_pending, "stale-output", 2048)
            .await
            .unwrap()
            .is_none(),
        "a stale metadata/input guard must not replace the reset receipt"
    );

    let incremental = TestAssetRecord::new("REFRESH")
        .checksum("provider-v1")
        .metadata(AssetMetadata {
            metadata_hash: Some("metadata-v3".into()),
            rating: Some(4),
            ..AssetMetadata::default()
        })
        .build();
    assert!(matches!(
        db.upsert_seen(&incremental).await,
        Err(StateError::Invariant {
            operation: "upsert_seen",
            ..
        })
    ));
    let incrementally_preserved = db
        .get_pending_metadata_rewrites_page_for_queue(
            MetadataRewriteQueue::CaptureRepair,
            None,
            0,
            10,
        )
        .await
        .unwrap();
    assert_eq!(
        incrementally_preserved[0].capture_repair_receipt,
        pending_after_ordinary.capture_repair_receipt
    );
}

#[tokio::test]
async fn prepared_capture_receipt_blocks_metadata_refresh_for_every_version() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    seed_downloaded_capture_asset(
        &db,
        "MULTI_REFRESH",
        "metadata-v1",
        "provider-v1",
        "local-original",
    )
    .await;
    let medium = TestAssetRecord::new("MULTI_REFRESH")
        .version_size(VersionSizeKey::Medium)
        .checksum("provider-medium")
        .metadata(AssetMetadata {
            metadata_hash: Some("metadata-v1".into()),
            ..AssetMetadata::default()
        })
        .build();
    db.upsert_seen(&medium).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "MULTI_REFRESH",
        "medium",
        Path::new("/photos/capture-medium.jpg"),
        "local-medium",
        None,
    )
    .await
    .unwrap();
    let current = AssetMetadata {
        metadata_hash: Some("metadata-v1".into()),
        ..AssetMetadata::default()
    };
    db.refresh_downloaded_asset_metadata(
        "PrimarySync",
        "MULTI_REFRESH",
        (&original_metadata(&current), DateTime::UNIX_EPOCH, None),
        true,
        true,
        METADATA_CAPTURE_REVISION,
    )
    .await
    .unwrap();
    let mut pending = db
        .get_pending_metadata_rewrites_page_for_queue(
            MetadataRewriteQueue::CaptureRepair,
            None,
            0,
            10,
        )
        .await
        .unwrap()
        .into_iter()
        .find(|pending| pending.asset.version_size == VersionSizeKey::Original)
        .unwrap();
    pending.capture_repair_receipt = db
        .record_capture_repair_prepared(&pending, "prepared-original", 2048)
        .await
        .unwrap();
    assert!(pending.capture_repair_receipt.is_some());

    // Sibling metadata edits do not invalidate the original's receipt.
    let changed = AssetMetadata {
        metadata_hash: Some("metadata-v2".into()),
        rating: Some(5),
        ..AssetMetadata::default()
    };
    let changed_medium = TestAssetRecord::new("MULTI_REFRESH")
        .version_size(VersionSizeKey::Medium)
        .checksum("provider-medium")
        .created_at(DateTime::UNIX_EPOCH)
        .metadata(changed.clone())
        .build();
    db.upsert_seen(&changed_medium).await.unwrap();
    let new_thumb = TestAssetRecord::new("MULTI_REFRESH")
        .version_size(VersionSizeKey::Thumb)
        .checksum("provider-thumb")
        .created_at(DateTime::UNIX_EPOCH)
        .metadata(changed.clone())
        .build();
    db.upsert_seen(&new_thumb).await.unwrap();

    // Capture-date drift remains blocked even when a sibling is replaced.
    for checksum in ["provider-medium", "provider-replacement"] {
        let mut guarded = changed_medium.clone();
        guarded.checksum = checksum.into();
        guarded.created_at = DateTime::from_timestamp_millis(1).unwrap();
        assert!(matches!(
            db.upsert_seen(&guarded).await,
            Err(StateError::Invariant {
                operation: "upsert_seen",
                ..
            })
        ));
    }

    // The prepared receipt still guards its own rendition against both an
    // edited catalogue and a drifted capture timestamp.
    for (metadata, created_at) in [
        (changed.clone(), DateTime::UNIX_EPOCH),
        (current.clone(), DateTime::from_timestamp_millis(1).unwrap()),
    ] {
        let guarded = TestAssetRecord::new("MULTI_REFRESH")
            .checksum("provider-v1")
            .metadata(metadata)
            .created_at(created_at)
            .build();
        assert!(matches!(
            db.upsert_seen(&guarded).await,
            Err(StateError::Invariant {
                operation: "upsert_seen",
                ..
            })
        ));
    }
    assert_eq!(
        db.refresh_downloaded_asset_metadata(
            "PrimarySync",
            "MULTI_REFRESH",
            (
                &original_metadata(&current),
                DateTime::from_timestamp_millis(1).unwrap(),
                None,
            ),
            false,
            false,
            METADATA_CAPTURE_REVISION,
        )
        .await
        .unwrap(),
        0,
        "a prepared receipt cannot be finalized against drifted capture dates"
    );
    let receipts_before = db
        .get_pending_metadata_rewrites_page_for_queue(
            MetadataRewriteQueue::CaptureRepair,
            None,
            0,
            10,
        )
        .await
        .unwrap();
    assert_eq!(
        receipts_before
            .iter()
            .find(|pending| pending.asset.version_size == VersionSizeKey::Original)
            .unwrap()
            .capture_repair_receipt,
        Some(CaptureRepairReceipt::Prepared {
            metadata_hash: current.compute_hash(),
            output_checksum: "prepared-original".into(),
            output_size: 2048,
        })
    );
    db.set_metadata_capture_revision_for_test("PrimarySync", "MULTI_REFRESH", 0);
    let mut next_medium = changed.clone();
    next_medium.metadata_hash = Some("metadata-v3".into());
    assert_eq!(
        db.refresh_downloaded_asset_metadata(
            "PrimarySync",
            "MULTI_REFRESH",
            (
                &metadata_capture(&[
                    (VersionSizeKey::Medium, Arc::new(next_medium)),
                    (VersionSizeKey::Original, Arc::new(changed)),
                ]),
                DateTime::UNIX_EPOCH,
                None,
            ),
            false,
            false,
            METADATA_CAPTURE_REVISION,
        )
        .await
        .unwrap(),
        0
    );

    let downloaded = db.get_downloaded_page(0, 10).await.unwrap();
    let versions = downloaded
        .iter()
        .filter(|record| record.id.as_ref() == "MULTI_REFRESH")
        .collect::<Vec<_>>();
    assert_eq!(versions.len(), 2);
    for record in versions {
        let expected = if record.version_size == VersionSizeKey::Original {
            current.compute_hash()
        } else {
            "metadata-v2".into()
        };
        assert_eq!(record.metadata.metadata_hash, Some(expected));
        assert_eq!(record.created_at, DateTime::UNIX_EPOCH);
        assert!(record.added_at.is_none());
    }
    let receipts_after = db
        .get_pending_metadata_rewrites_page_for_queue(
            MetadataRewriteQueue::CaptureRepair,
            None,
            0,
            10,
        )
        .await
        .unwrap();
    for (before, after) in receipts_before.iter().zip(&receipts_after) {
        assert_eq!(before.capture_repair_receipt, after.capture_repair_receipt);
    }
    assert_eq!(
        db.get_metadata_capture_candidates("PrimarySync", METADATA_CAPTURE_REVISION, 10)
            .await
            .unwrap()
            .len(),
        1,
        "a blocked family must not advance its capture revision"
    );
    let preserved = db
        .get_pending_metadata_rewrites_page_for_queue(
            MetadataRewriteQueue::CaptureRepair,
            None,
            0,
            10,
        )
        .await
        .unwrap();
    assert_eq!(
        preserved
            .iter()
            .find(|row| row.asset.version_size == VersionSizeKey::Original)
            .unwrap()
            .capture_repair_receipt,
        pending.capture_repair_receipt
    );
}

#[tokio::test]
async fn prepared_capture_receipt_blocks_source_deletion_transitions() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    seed_downloaded_capture_asset(
        &db,
        "CAPTURE_DELETE",
        "metadata-v1",
        "provider-v1",
        "local-v1",
    )
    .await;
    let metadata = AssetMetadata {
        metadata_hash: Some("metadata-v1".into()),
        ..AssetMetadata::default()
    };
    db.refresh_downloaded_asset_metadata(
        "PrimarySync",
        "CAPTURE_DELETE",
        (&original_metadata(&metadata), DateTime::UNIX_EPOCH, None),
        true,
        true,
        METADATA_CAPTURE_REVISION,
    )
    .await
    .unwrap();
    let pending = db
        .get_pending_metadata_rewrites_page_for_queue(
            MetadataRewriteQueue::CaptureRepair,
            None,
            0,
            1,
        )
        .await
        .unwrap()
        .pop()
        .unwrap();
    db.record_capture_repair_prepared(&pending, "prepared-output", 2048)
        .await
        .unwrap();

    assert!(matches!(
        db.mark_soft_deleted("PrimarySync", "CAPTURE_DELETE", None)
            .await,
        Err(StateError::Invariant {
            operation: "mark_soft_deleted",
            ..
        })
    ));
    assert!(matches!(
        db.resolve_source_deleted("PrimarySync", "CAPTURE_DELETE", None)
            .await,
        Err(StateError::Invariant {
            operation: "resolve_source_deleted",
            ..
        })
    ));
    db.upsert_asset_master_mapping("PrimarySync", "CAPTURE_DELETE", "MASTER_DELETE")
        .await
        .unwrap();
    assert!(matches!(
        db.mark_master_family_soft_deleted("PrimarySync", "MASTER_DELETE", None)
            .await,
        Err(StateError::Invariant {
            operation: "mark_master_family_soft_deleted",
            ..
        })
    ));
    assert!(matches!(
        db.resolve_master_family_source_deleted("PrimarySync", "MASTER_DELETE", None)
            .await,
        Err(StateError::Invariant {
            operation: "resolve_master_family_source_deleted",
            ..
        })
    ));
    assert_eq!(
        db.get_pending_metadata_rewrites_page_for_queue(
            MetadataRewriteQueue::CaptureRepair,
            None,
            0,
            1,
        )
        .await
        .unwrap()
        .len(),
        1
    );
}

#[tokio::test]
async fn capture_repair_receipt_fails_closed_and_tracks_installed_bytes() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    let mut record = TestAssetRecord::new("ADOPT_CAPTURE")
        .checksum("provider-v1")
        .metadata(AssetMetadata {
            metadata_hash: Some("metadata-v1".into()),
            ..AssetMetadata::default()
        })
        .build();
    Arc::make_mut(&mut record.metadata).refresh_hash();
    db.import_adopt(
        &record,
        Path::new("/photos/adopted.jpg"),
        "local-v1",
        2048,
        Some(1),
    )
    .await
    .unwrap();
    db.refresh_downloaded_asset_metadata(
        "PrimarySync",
        "ADOPT_CAPTURE",
        (
            &original_metadata(&record.metadata),
            record.created_at,
            record.added_at,
        ),
        false,
        true,
        METADATA_CAPTURE_REVISION,
    )
    .await
    .unwrap();
    let mut pending = db
        .get_pending_metadata_rewrites_page_for_queue(
            MetadataRewriteQueue::CaptureRepair,
            None,
            0,
            10,
        )
        .await
        .unwrap()
        .pop()
        .unwrap();
    pending.capture_repair_receipt = db
        .record_capture_repair_prepared(&pending, "local-v2", 2048)
        .await
        .unwrap();

    record.created_at += chrono::Duration::milliseconds(1);
    db.import_adopt(
        &record,
        Path::new("/photos/adopted.jpg"),
        "local-v1",
        2048,
        Some(1),
    )
    .await
    .unwrap_err();
    record.created_at = pending.asset.created_at;
    assert_eq!(
        db.get_downloaded_page(0, 10).await.unwrap()[0].created_at,
        record.created_at
    );
    db.import_adopt(
        &record,
        Path::new("/photos/readopted.jpg"),
        "local-v1",
        2048,
        Some(2),
    )
    .await
    .unwrap();
    assert_eq!(
        db.get_pending_metadata_rewrites_page_for_queue(
            MetadataRewriteQueue::CaptureRepair,
            None,
            0,
            10,
        )
        .await
        .unwrap()[0]
            .capture_repair_receipt,
        pending.capture_repair_receipt,
        "byte-identical re-adoption must preserve prepared evidence"
    );

    db.import_adopt(
        &record,
        Path::new("/photos/readopted.jpg"),
        "local-changed",
        2048,
        Some(3),
    )
    .await
    .unwrap();
    let preserved = db
        .get_pending_metadata_rewrites_page_for_queue(
            MetadataRewriteQueue::CaptureRepair,
            None,
            0,
            10,
        )
        .await
        .unwrap();
    assert_eq!(preserved.len(), 1);
    assert_eq!(
        preserved[0].asset.local_path.as_deref(),
        Some(Path::new("/photos/adopted.jpg")),
        "changing one copy must not erase another path's prepared receipt"
    );
    db.import_adopt(
        &record,
        Path::new("/photos/adopted.jpg"),
        "local-changed",
        2048,
        Some(4),
    )
    .await
    .unwrap();
    assert!(
        db.get_pending_metadata_rewrites_page_for_queue(
            MetadataRewriteQueue::CaptureRepair,
            None,
            0,
            10,
        )
        .await
        .unwrap()
        .is_empty(),
        "changed bytes at the same path invalidate capture evidence"
    );

    db.refresh_downloaded_asset_metadata(
        "PrimarySync",
        "ADOPT_CAPTURE",
        (
            &original_metadata(&record.metadata),
            record.created_at,
            record.added_at,
        ),
        false,
        true,
        METADATA_CAPTURE_REVISION,
    )
    .await
    .unwrap();
    let pending = db
        .get_pending_metadata_rewrites_page_for_queue(
            MetadataRewriteQueue::CaptureRepair,
            None,
            0,
            10,
        )
        .await
        .unwrap();
    assert_eq!(pending.len(), 2);
    for row in &pending {
        assert!(
            db.record_capture_repair_prepared(row, "local-v3", 2048)
                .await
                .unwrap()
                .is_some()
        );
    }
    record.checksum = "provider-v2".into();
    record.created_at += chrono::Duration::milliseconds(1);
    db.upsert_seen(&record).await.unwrap();
    let rows = db.get_pending().await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].created_at, record.created_at);
    assert_eq!(rows[0].checksum, record.checksum);
    let receipt: (Option<String>, Option<String>, Option<i64>) = {
        let conn = db.acquire_lock("read invalidated capture receipt").unwrap();
        conn.query_row(
            "SELECT capture_repair_metadata_hash, capture_repair_output_checksum, \
                    capture_repair_output_size FROM assets WHERE id = 'ADOPT_CAPTURE'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap()
    };
    assert_eq!(receipt, (None, None, None));

    seed_downloaded_capture_asset(&db, "MALFORMED", "metadata-v1", "provider-v1", "local-v1").await;
    {
        let conn = db.acquire_lock("write malformed capture receipt").unwrap();
        conn.execute(
            "UPDATE assets SET capture_repair_output_checksum = 'orphaned' \
                 WHERE id = 'MALFORMED'",
            [],
        )
        .unwrap();
    }
    let changed = AssetMetadata {
        metadata_hash: Some("metadata-v2".into()),
        rating: Some(5),
        ..AssetMetadata::default()
    };
    db.refresh_downloaded_asset_metadata(
        "PrimarySync",
        "MALFORMED",
        (&original_metadata(&changed), DateTime::UNIX_EPOCH, None),
        false,
        true,
        METADATA_CAPTURE_REVISION,
    )
    .await
    .unwrap();
    assert!(
        db.get_pending_metadata_rewrites_page_for_queue(
            MetadataRewriteQueue::CaptureRepair,
            None,
            0,
            10,
        )
        .await
        .is_err(),
        "partial capture receipt must fail closed"
    );
}

#[tokio::test]
async fn metadata_capture_revision_repair_is_resumable_and_preserves_file_state() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    let record = TestAssetRecord::new("ASSET_CHILD")
        .checksum("provider-checksum")
        .filename("photo.jpg")
        .size(2048)
        .metadata(AssetMetadata {
            metadata_hash: Some("revision-zero".to_string()),
            ..AssetMetadata::default()
        })
        .build();
    db.upsert_seen(&record).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "ASSET_CHILD",
        "original",
        Path::new("/photos/photo.jpg"),
        "local-checksum",
        Some("provider-checksum"),
    )
    .await
    .unwrap();
    db.upsert_asset_master_mapping("PrimarySync", "ASSET_CHILD", "MASTER")
        .await
        .unwrap();
    {
        let conn = db.acquire_lock("test_metadata_capture_revision").unwrap();
        conn.execute(
            "UPDATE asset_metadata_capture_revisions SET revision = 0 \
                 WHERE library = 'PrimarySync' AND asset_id = 'ASSET_CHILD'",
            [],
        )
        .unwrap();
    }

    let started = db
        .begin_metadata_capture_revision("PrimarySync", METADATA_CAPTURE_REVISION)
        .await
        .unwrap();
    assert_eq!(started.active_revision, 0);
    assert_eq!(started.pending_revision, Some(METADATA_CAPTURE_REVISION));
    assert_eq!(started.remaining_assets, 1);

    let candidates = db
        .get_metadata_capture_candidates("PrimarySync", METADATA_CAPTURE_REVISION, 10)
        .await
        .unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].asset_id, "ASSET_CHILD");
    assert_eq!(candidates[0].master_record_name, "MASTER");
    assert_eq!(
        candidates[0].asset_record_name.as_deref(),
        Some("ASSET_CHILD")
    );
    assert_eq!(candidates[0].versions.len(), 1);
    assert_eq!(candidates[0].versions[0].size_bytes, 2048);

    db.record_metadata_capture_failure(
        "PrimarySync",
        METADATA_CAPTURE_REVISION,
        "temporary provider failure",
    )
    .await
    .unwrap();
    let still_pending = db
        .complete_metadata_capture_revision("PrimarySync", METADATA_CAPTURE_REVISION)
        .await
        .unwrap();
    assert_eq!(still_pending.remaining_assets, 1);
    assert_eq!(still_pending.failed_assets, 1);

    let metadata = AssetMetadata {
        rating: Some(5),
        ..AssetMetadata::default()
    };
    db.refresh_downloaded_asset_metadata(
        "PrimarySync",
        "ASSET_CHILD",
        (
            &original_metadata(&metadata),
            record.created_at,
            record.added_at,
        ),
        true,
        false,
        METADATA_CAPTURE_REVISION,
    )
    .await
    .unwrap();
    let completed = db
        .complete_metadata_capture_revision("PrimarySync", METADATA_CAPTURE_REVISION)
        .await
        .unwrap();
    assert_eq!(completed.active_revision, METADATA_CAPTURE_REVISION);
    assert_eq!(completed.pending_revision, None);
    assert_eq!(completed.remaining_assets, 0);
    assert_eq!(completed.processed_assets, 1);
    assert_eq!(completed.failed_assets, 0);

    let downloaded = db.get_downloaded_page(0, 10).await.unwrap();
    assert_eq!(downloaded.len(), 1);
    assert_eq!(
        downloaded[0].local_path.as_deref(),
        Some(Path::new("/photos/photo.jpg"))
    );
    assert_eq!(
        downloaded[0].local_checksum.as_deref(),
        Some("local-checksum")
    );
    assert_eq!(
        downloaded[0].download_checksum.as_deref(),
        Some("provider-checksum")
    );
    assert_eq!(downloaded[0].metadata.rating, Some(5));
    assert_eq!(db.get_pending_metadata_rewrites(10).await.unwrap().len(), 1);
}

#[tokio::test]
async fn metadata_capture_candidates_are_bounded_and_library_scoped() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    for (library, id) in [
        ("PrimarySync", "A"),
        ("PrimarySync", "B"),
        ("PrimarySync", "C"),
        ("SharedSync-AAAA", "D"),
    ] {
        let mut record = TestAssetRecord::new(id).build();
        record.library = Arc::from(library);
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            library,
            id,
            "original",
            Path::new("/photos/photo.jpg"),
            "local-checksum",
            None,
        )
        .await
        .unwrap();
    }
    {
        let conn = db.acquire_lock("test_metadata_capture_scope").unwrap();
        conn.execute("DELETE FROM asset_metadata_capture_revisions", [])
            .unwrap();
    }

    assert!(
        db.has_metadata_capture_work(&["PrimarySync"], METADATA_CAPTURE_REVISION)
            .await
            .unwrap()
    );
    let primary = db
        .get_metadata_capture_candidates("PrimarySync", METADATA_CAPTURE_REVISION, 2)
        .await
        .unwrap();
    assert_eq!(primary.len(), 2);
    assert!(primary.iter().all(|row| row.library == "PrimarySync"));
    let shared = db
        .get_metadata_capture_candidates("SharedSync-AAAA", METADATA_CAPTURE_REVISION, 2)
        .await
        .unwrap();
    assert_eq!(shared.len(), 1);
    assert_eq!(shared[0].asset_id, "D");
}
