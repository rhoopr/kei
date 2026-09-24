#[cfg(feature = "xmp")]
use super::run_pending_page;
use super::{METADATA_REWRITE_BATCH, run_pending};
#[cfg(feature = "xmp")]
use crate::download::AssetGroupings;
#[cfg(feature = "xmp")]
use crate::download::metadata_rewrite::capture::{
    fingerprint_checksum, receipt_matches_fingerprint,
};
#[cfg(feature = "xmp")]
use crate::download::metadata_rewrite::immediate::{MetadataWriteRequest, write_download_metadata};
use crate::download::metadata_rewrite::planning::{CaptureTimestampRepair, MetadataFlags};
#[cfg(feature = "xmp")]
use crate::download::metadata_rewrite::test_support::seed_downloaded_marker;
use crate::download::metadata_rewrite::test_support::{minimal_jpeg_bytes, now_local};
#[cfg(feature = "xmp")]
use crate::state::db::CaptureRepairReceipt;
use crate::state::db::MetadataRewriteQueue;
#[cfg(feature = "xmp")]
use chrono::{FixedOffset, TimeZone};
#[cfg(feature = "xmp")]
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
#[cfg(feature = "xmp")]
use xmp_toolkit::{XmpMeta, xmp_ns};

#[cfg(feature = "xmp")]
async fn stored_checksums(db: &crate::state::SqliteStateDb) -> (Option<String>, Option<String>) {
    let row = db.get_downloaded_page(0, 1).await.unwrap().remove(0);
    (row.local_checksum, row.download_checksum)
}

#[cfg(feature = "xmp")]
fn embedded_rating_flags() -> MetadataFlags {
    MetadataFlags::RATING | MetadataFlags::EMBED_XMP
}

/// Seeds a downloaded JPEG carrying a rewrite marker. Returns the
/// database, the media path, and the checksum recorded for the file.
/// `rating` drives whether the embedded writer has anything to write.
#[cfg(feature = "xmp")]
async fn seed_marked_jpeg(
    dir: &std::path::Path,
    asset_id: &'static str,
    rating: Option<u8>,
) -> (crate::state::SqliteStateDb, PathBuf, String) {
    use crate::state::SqliteStateDb;
    use crate::state::types::AssetMetadata;

    let path = dir.join(format!("{asset_id}.jpg"));
    std::fs::write(&path, minimal_jpeg_bytes()).unwrap();
    let recorded = crate::download::file::compute_sha256(&path).await.unwrap();

    // File backed so a test can reopen it and drop connection-scoped
    // failure triggers between passes.
    let db = SqliteStateDb::open(&dir.join("state.db")).await.unwrap();
    seed_downloaded_marker(
        &db,
        asset_id,
        &format!("{asset_id}.jpg"),
        &path,
        &recorded,
        AssetMetadata {
            rating,
            metadata_hash: Some("fresh-hash".to_string()),
            ..AssetMetadata::default()
        },
        None,
    )
    .await;

    (db, path, recorded)
}

#[cfg(feature = "xmp")]
fn grouped_xmp(meta: &xmp_toolkit::XmpMeta) -> (Vec<String>, Vec<String>) {
    let keywords: Vec<String> = meta
        .property_array(xmp_toolkit::xmp_ns::DC, "subject")
        .map(|value| value.value)
        .collect();
    let people: Vec<String> = meta
        .property_array(xmp_toolkit::xmp_ns::IPTC_EXT, "PersonInImage")
        .map(|value| value.value)
        .collect();
    (keywords, people)
}

async fn queue_capture_repair(
    db: &crate::state::SqliteStateDb,
    record: &crate::state::types::AssetRecord,
) {
    db.refresh_downloaded_asset_metadata(
        &record.library,
        &record.id,
        (
            &crate::state::MetadataCapture {
                shared: Arc::clone(&record.metadata),
                renditions: Arc::from([(
                    record.version_size,
                    crate::state::RenditionMetadata {
                        checksum: Some(Arc::from(record.checksum.as_ref())),
                        width: record.metadata.width,
                        height: record.metadata.height,
                        duration_secs: record.metadata.duration_secs,
                    },
                )]),
            },
            record.created_at,
            record.added_at,
        ),
        true,
        true,
        crate::state::METADATA_CAPTURE_REVISION,
    )
    .await
    .unwrap();
}

#[cfg(feature = "xmp")]
fn with_capture_xmp(input: &[u8]) -> Vec<u8> {
    XmpMeta::register_namespace("http://cipa.jp/exif/1.0/", "exifEX").unwrap();
    let mut xmp = XmpMeta::new().unwrap();
    xmp.set_property(
        xmp_ns::EXIF,
        "DateTimeOriginal",
        &xmp_toolkit::XmpValue::new("2024-06-15T10:00:00+11:00".to_string()),
    )
    .unwrap();
    xmp.set_property(
        "http://cipa.jp/exif/1.0/",
        "OffsetTimeOriginal",
        &xmp_toolkit::XmpValue::new("+11:00".to_string()),
    )
    .unwrap();
    let mut output = Vec::new();
    crate::download::heif::rewrite_xmp(input, xmp.to_string().as_bytes(), &mut output).unwrap();
    output
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn pending_heif_rewrite_detects_drift_before_format_routing() {
    use crate::state::SqliteStateDb;
    use crate::state::types::AssetMetadata;

    let dir = tempfile::tempdir().unwrap();
    let photo_path = dir.path().join("extensionless-asset");
    let original = include_bytes!("../../../../tests/data/sample.heic");
    std::fs::write(&photo_path, original).unwrap();
    let recorded = crate::download::file::compute_sha256(&photo_path)
        .await
        .unwrap();
    let db = SqliteStateDb::open_in_memory().unwrap();
    seed_downloaded_marker(
        &db,
        "ROUTING_DRIFT",
        "extensionless-asset",
        &photo_path,
        &recorded,
        AssetMetadata {
            rating: Some(5),
            metadata_hash: Some("routing-drift".into()),
            ..AssetMetadata::default()
        },
        None,
    )
    .await;
    let changed = b"unsupported concurrent replacement";
    std::fs::write(&photo_path, changed).unwrap();

    let pass = run_pending(
        &db,
        embedded_rating_flags(),
        Arc::from(".metadata-test"),
        &CancellationToken::new(),
    )
    .await;

    assert_eq!(pass.failed, 1);
    assert_eq!(std::fs::read(&photo_path).unwrap(), changed);
    assert_eq!(db.get_pending_metadata_rewrites(10).await.unwrap().len(), 1);
    let (local, download) = stored_checksums(&db).await;
    assert_eq!(local.as_deref(), Some(recorded.as_str()));
    assert_eq!(download, None);
}

#[tokio::test]
async fn refresh_drain_repairs_a_state_recorded_host_local_timestamp() {
    use crate::config::MetadataConfig;
    use crate::download::DownloadStore;
    use crate::state::SqliteStateDb;
    use crate::state::types::AssetMetadata;

    let dir = tempfile::tempdir().unwrap();
    let photo_path = dir.path().join("host-local-drain.jpg");
    std::fs::write(&photo_path, minimal_jpeg_bytes()).unwrap();
    crate::download::metadata::apply_metadata(
        &photo_path,
        &crate::download::metadata::MetadataWrite {
            datetime: Some("2024:06:14 23:00:00".into()),
            ..crate::download::metadata::MetadataWrite::default()
        },
        ".seed-tmp",
    )
    .unwrap();
    let checksum = crate::download::file::compute_sha256(&photo_path)
        .await
        .unwrap();

    let created_local = now_local();
    let db = SqliteStateDb::open_in_memory().unwrap();
    let record = crate::test_helpers::TestAssetRecord::new("CAPTURE_REPAIR")
        .filename("host-local-drain.jpg")
        .checksum("provider-checksum")
        .created_at(created_local.with_timezone(&chrono::Utc))
        .metadata(AssetMetadata {
            timezone_offset: Some(39_600),
            ..AssetMetadata::default()
        })
        .build();
    db.upsert_seen(&record).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "CAPTURE_REPAIR",
        "original",
        &photo_path,
        &checksum,
        None,
    )
    .await
    .unwrap();
    queue_capture_repair(&db, &record).await;

    let residual = crate::download::drain_pending_metadata_rewrites(
        &db as &dyn DownloadStore,
        &MetadataConfig {
            set_exif_datetime: true,
            ..MetadataConfig::default()
        },
        CaptureTimestampRepair::ReplaceWithCaptureLocal,
        &["PrimarySync"],
        Arc::from(".metadata-test"),
        &CancellationToken::new(),
    )
    .await;

    assert_eq!(residual, 0);
    assert!(
        db.get_pending_metadata_rewrites(1)
            .await
            .unwrap()
            .is_empty()
    );
    let probe = crate::download::metadata::probe_exif(&photo_path).unwrap();
    assert!(probe.denotes_capture_time(&created_local));
    assert_eq!(probe.offset_time_original.as_deref(), Some("+11:00"));
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn capture_repair_recovers_publication_after_state_finalisation_failure() {
    use crate::config::MetadataConfig;
    use crate::download::DownloadStore;
    use crate::state::SqliteStateDb;
    use crate::state::types::AssetMetadata;

    let dir = tempfile::tempdir().unwrap();
    let photo_path = dir.path().join("capture-repair-recovery.jpg");
    let db_path = dir.path().join("state.sqlite");
    std::fs::write(&photo_path, minimal_jpeg_bytes()).unwrap();
    crate::download::metadata::apply_metadata(
        &photo_path,
        &crate::download::metadata::MetadataWrite {
            datetime: Some("2024:06:14 23:00:00".into()),
            offset_time_original: Some("+05:00".into()),
            ..crate::download::metadata::MetadataWrite::default()
        },
        ".seed-tmp",
    )
    .unwrap();
    let checksum = crate::download::file::compute_sha256(&photo_path)
        .await
        .unwrap();

    let created_local = now_local() + chrono::Duration::milliseconds(629);
    let record = crate::test_helpers::TestAssetRecord::new("CAPTURE_REPAIR_RECOVERY")
        .filename("capture-repair-recovery.jpg")
        .checksum("provider-checksum")
        .created_at(created_local.with_timezone(&chrono::Utc))
        .metadata(AssetMetadata {
            timezone_offset: Some(39_600),
            ..AssetMetadata::default()
        })
        .build();
    let db = SqliteStateDb::open(&db_path).await.unwrap();
    db.upsert_seen(&record).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "CAPTURE_REPAIR_RECOVERY",
        "original",
        &photo_path,
        &checksum,
        None,
    )
    .await
    .unwrap();
    queue_capture_repair(&db, &record).await;
    db.fail_metadata_checksum_write_for_test();

    let config = MetadataConfig {
        set_exif_datetime: true,
        ..MetadataConfig::default()
    };
    let first = crate::download::drain_pending_metadata_rewrites(
        &db as &dyn DownloadStore,
        &config,
        CaptureTimestampRepair::ReplaceWithCaptureLocal,
        &["PrimarySync"],
        Arc::from(".metadata-test"),
        &CancellationToken::new(),
    )
    .await;

    assert_eq!(first, 1);
    let pending = db
        .get_pending_metadata_rewrites_page_for_queue(
            MetadataRewriteQueue::CaptureRepair,
            None,
            0,
            1,
        )
        .await
        .unwrap();
    assert!(matches!(
        pending[0].capture_repair_receipt,
        Some(CaptureRepairReceipt::Prepared { .. })
    ));
    assert_eq!(pending[0].asset.created_at, record.created_at);
    let published = std::fs::read(&photo_path).unwrap();
    let fingerprint = crate::download::file::fingerprint_regular_file(&photo_path)
        .await
        .unwrap();
    assert!(receipt_matches_fingerprint(
        pending[0].capture_repair_receipt.as_ref().unwrap(),
        fingerprint
    ));
    assert_eq!(stored_checksums(&db).await, (Some(checksum.clone()), None));
    let probe = crate::download::metadata::probe_exif(&photo_path).unwrap();
    assert!(probe.denotes_capture_time(&created_local));
    assert_eq!(probe.offset_time_original.as_deref(), Some("+11:00"));
    drop(db);

    let reopened = SqliteStateDb::open(&db_path).await.unwrap();
    let second = crate::download::drain_pending_metadata_rewrites(
        &reopened as &dyn DownloadStore,
        &config,
        CaptureTimestampRepair::ReplaceWithCaptureLocal,
        &["PrimarySync"],
        Arc::from(".metadata-test"),
        &CancellationToken::new(),
    )
    .await;

    assert_eq!(second, 0);
    assert_eq!(std::fs::read(&photo_path).unwrap(), published);
    assert_eq!(
        stored_checksums(&reopened).await,
        (
            Some(fingerprint_checksum(fingerprint)),
            Some(checksum.clone())
        )
    );
    assert!(
        reopened
            .get_pending_metadata_rewrites_page_for_queue(
                MetadataRewriteQueue::CaptureRepair,
                None,
                0,
                1,
            )
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        reopened
            .get_pending_metadata_rewrites(1)
            .await
            .unwrap()
            .len(),
        1,
        "receipt recovery proves only capture completion, not current ordinary metadata"
    );
    assert_eq!(
        crate::download::drain_pending_metadata_rewrites(
            &reopened as &dyn DownloadStore,
            &config,
            CaptureTimestampRepair::Preserve,
            &["PrimarySync"],
            Arc::from(".metadata-test"),
            &CancellationToken::new(),
        )
        .await,
        0
    );
    assert!(
        reopened
            .get_pending_metadata_rewrites(1)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(std::fs::read(&photo_path).unwrap(), published);
    assert_eq!(
        stored_checksums(&reopened).await,
        (Some(fingerprint_checksum(fingerprint)), Some(checksum))
    );
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn capture_repair_reprepares_an_unpublished_receipt() {
    use crate::config::MetadataConfig;
    use crate::download::DownloadStore;
    use crate::state::SqliteStateDb;
    use crate::state::types::AssetMetadata;

    let dir = tempfile::tempdir().unwrap();
    let photo_path = dir.path().join("capture-repair-reprepare.jpg");
    std::fs::write(&photo_path, minimal_jpeg_bytes()).unwrap();
    crate::download::metadata::apply_metadata(
        &photo_path,
        &crate::download::metadata::MetadataWrite {
            datetime: Some("2024:06:14 23:00:00".into()),
            offset_time_original: Some("+05:00".into()),
            ..crate::download::metadata::MetadataWrite::default()
        },
        ".seed-tmp",
    )
    .unwrap();
    let checksum = crate::download::file::compute_sha256(&photo_path)
        .await
        .unwrap();

    let db = SqliteStateDb::open_in_memory().unwrap();
    let record = crate::test_helpers::TestAssetRecord::new("CAPTURE_REPAIR_REPREPARE")
        .filename("capture-repair-reprepare.jpg")
        .checksum("provider-checksum")
        .created_at(now_local().with_timezone(&chrono::Utc))
        .metadata(AssetMetadata {
            timezone_offset: Some(39_600),
            ..AssetMetadata::default()
        })
        .build();
    db.upsert_seen(&record).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "CAPTURE_REPAIR_REPREPARE",
        "original",
        &photo_path,
        &checksum,
        None,
    )
    .await
    .unwrap();
    queue_capture_repair(&db, &record).await;
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
    assert!(
        db.record_capture_repair_prepared(&pending, "unpublished-output", 1)
            .await
            .unwrap()
            .is_some()
    );

    let residual = crate::download::drain_pending_metadata_rewrites(
        &db as &dyn DownloadStore,
        &MetadataConfig {
            set_exif_datetime: true,
            ..MetadataConfig::default()
        },
        CaptureTimestampRepair::ReplaceWithCaptureLocal,
        &["PrimarySync"],
        Arc::from(".metadata-test"),
        &CancellationToken::new(),
    )
    .await;

    assert_eq!(residual, 0);
    assert!(
        db.get_pending_metadata_rewrites_page_for_queue(
            MetadataRewriteQueue::CaptureRepair,
            None,
            0,
            1,
        )
        .await
        .unwrap()
        .is_empty()
    );
    let probe = crate::download::metadata::probe_exif(&photo_path).unwrap();
    assert!(probe.denotes_capture_time(&now_local()));
    assert_eq!(probe.offset_time_original.as_deref(), Some("+11:00"));
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn capture_timestamp_repair_requires_a_recorded_local_checksum() {
    use crate::config::MetadataConfig;
    use crate::download::DownloadStore;
    use crate::state::SqliteStateDb;
    use crate::state::types::AssetMetadata;

    let dir = tempfile::tempdir().unwrap();
    let photo_path = dir.path().join("unverified-capture-repair.jpg");
    std::fs::write(&photo_path, minimal_jpeg_bytes()).unwrap();
    crate::download::metadata::apply_metadata(
        &photo_path,
        &crate::download::metadata::MetadataWrite {
            datetime: Some("2024:06:14 23:00:00".into()),
            offset_time_original: Some("+05:00".into()),
            ..crate::download::metadata::MetadataWrite::default()
        },
        ".seed-tmp",
    )
    .unwrap();
    let checksum = crate::download::file::compute_sha256(&photo_path)
        .await
        .unwrap();
    let before = std::fs::read(&photo_path).unwrap();

    let db = SqliteStateDb::open_in_memory().unwrap();
    let record = crate::test_helpers::TestAssetRecord::new("UNVERIFIED_CAPTURE_REPAIR")
        .filename("unverified-capture-repair.jpg")
        .checksum("provider-checksum")
        .created_at(now_local().with_timezone(&chrono::Utc))
        .metadata(AssetMetadata {
            timezone_offset: Some(39_600),
            ..AssetMetadata::default()
        })
        .build();
    db.upsert_seen(&record).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "UNVERIFIED_CAPTURE_REPAIR",
        "original",
        &photo_path,
        &checksum,
        None,
    )
    .await
    .unwrap();
    db.clear_local_checksum_for_test("PrimarySync", "UNVERIFIED_CAPTURE_REPAIR", "original");
    queue_capture_repair(&db, &record).await;

    let residual = crate::download::drain_pending_metadata_rewrites(
        &db as &dyn DownloadStore,
        &MetadataConfig {
            set_exif_datetime: true,
            ..MetadataConfig::default()
        },
        CaptureTimestampRepair::ReplaceWithCaptureLocal,
        &["PrimarySync"],
        Arc::from(".metadata-test"),
        &CancellationToken::new(),
    )
    .await;

    assert_eq!(residual, 1);
    assert_eq!(db.get_pending_metadata_rewrites(1).await.unwrap().len(), 1);
    assert_eq!(
        std::fs::read(&photo_path).unwrap(),
        before,
        "a checksum-less row must not change the file"
    );
}

#[tokio::test]
async fn capture_timestamp_repair_without_offset_keeps_marker_pending() {
    use crate::config::MetadataConfig;
    use crate::download::DownloadStore;
    use crate::state::SqliteStateDb;

    let dir = tempfile::tempdir().unwrap();
    let photo_path = dir.path().join("capture-repair-without-offset.jpg");
    std::fs::write(&photo_path, minimal_jpeg_bytes()).unwrap();
    crate::download::metadata::apply_metadata(
        &photo_path,
        &crate::download::metadata::MetadataWrite {
            datetime: Some("2024:06:14 23:00:00".into()),
            ..crate::download::metadata::MetadataWrite::default()
        },
        ".seed-tmp",
    )
    .unwrap();
    let checksum = crate::download::file::compute_sha256(&photo_path)
        .await
        .unwrap();
    let before = std::fs::read(&photo_path).unwrap();

    let db = SqliteStateDb::open_in_memory().unwrap();
    let record = crate::test_helpers::TestAssetRecord::new("CAPTURE_REPAIR_WITHOUT_OFFSET")
        .filename("capture-repair-without-offset.jpg")
        .checksum("provider-checksum")
        .created_at(now_local().with_timezone(&chrono::Utc))
        .build();
    db.upsert_seen(&record).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "CAPTURE_REPAIR_WITHOUT_OFFSET",
        "original",
        &photo_path,
        &checksum,
        None,
    )
    .await
    .unwrap();
    queue_capture_repair(&db, &record).await;

    let residual = crate::download::drain_pending_metadata_rewrites(
        &db as &dyn DownloadStore,
        &MetadataConfig {
            set_exif_datetime: true,
            ..MetadataConfig::default()
        },
        CaptureTimestampRepair::ReplaceWithCaptureLocal,
        &["PrimarySync"],
        Arc::from(".metadata-test"),
        &CancellationToken::new(),
    )
    .await;

    assert_eq!(residual, 1);
    assert_eq!(db.get_pending_metadata_rewrites(1).await.unwrap().len(), 1);
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

    let ordinary_residual = crate::download::drain_pending_metadata_rewrites(
        &db as &dyn DownloadStore,
        &MetadataConfig {
            set_exif_datetime: true,
            ..MetadataConfig::default()
        },
        CaptureTimestampRepair::Preserve,
        &["PrimarySync"],
        Arc::from(".metadata-test"),
        &CancellationToken::new(),
    )
    .await;

    assert_eq!(ordinary_residual, 0);
    assert!(
        db.get_pending_metadata_rewrites(1)
            .await
            .unwrap()
            .is_empty()
    );
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
        1,
        "ordinary metadata completion must not consume capture-repair debt"
    );
    assert_eq!(std::fs::read(&photo_path).unwrap(), before);
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn ordinary_embed_waits_for_pending_capture_repair() {
    use crate::config::MetadataConfig;
    use crate::download::DownloadStore;
    use crate::state::SqliteStateDb;
    use crate::state::types::AssetMetadata;

    let dir = tempfile::tempdir().unwrap();
    let photo_path = dir.path().join("ordinary-before-capture.jpg");
    std::fs::write(&photo_path, minimal_jpeg_bytes()).unwrap();
    let checksum = crate::download::file::compute_sha256(&photo_path)
        .await
        .unwrap();
    let before = std::fs::read(&photo_path).unwrap();

    let db = SqliteStateDb::open_in_memory().unwrap();
    let record = crate::test_helpers::TestAssetRecord::new("ORDINARY_BEFORE_CAPTURE")
        .filename("ordinary-before-capture.jpg")
        .checksum("provider-checksum")
        .created_at(now_local().with_timezone(&chrono::Utc))
        .metadata(AssetMetadata {
            rating: Some(5),
            timezone_offset: Some(39_600),
            ..AssetMetadata::default()
        })
        .build();
    db.upsert_seen(&record).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "ORDINARY_BEFORE_CAPTURE",
        "original",
        &photo_path,
        &checksum,
        None,
    )
    .await
    .unwrap();
    queue_capture_repair(&db, &record).await;

    let residual = crate::download::drain_pending_metadata_rewrites(
        &db as &dyn DownloadStore,
        &MetadataConfig {
            set_exif_rating: true,
            ..MetadataConfig::default()
        },
        CaptureTimestampRepair::Preserve,
        &["PrimarySync"],
        Arc::from(".metadata-test"),
        &CancellationToken::new(),
    )
    .await;

    assert_eq!(residual, 1);
    assert_eq!(std::fs::read(&photo_path).unwrap(), before);
    assert_eq!(db.get_pending_metadata_rewrites(1).await.unwrap().len(), 1);
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

#[cfg(not(feature = "xmp"))]
#[tokio::test]
async fn capture_timestamp_repair_defers_unsupported_heif_without_xmp() {
    use crate::config::MetadataConfig;
    use crate::download::DownloadStore;
    use crate::state::SqliteStateDb;
    use crate::state::types::AssetMetadata;

    let dir = tempfile::tempdir().unwrap();
    let photo_path = dir.path().join("unsupported-capture-repair.heic");
    let source = include_bytes!("../../../../tests/data/sample.heic");
    std::fs::write(&photo_path, source).unwrap();
    let checksum = crate::download::file::compute_sha256(&photo_path)
        .await
        .unwrap();

    let db = SqliteStateDb::open_in_memory().unwrap();
    let record = crate::test_helpers::TestAssetRecord::new("UNSUPPORTED_CAPTURE_REPAIR")
        .filename("unsupported-capture-repair.heic")
        .checksum("provider-checksum")
        .created_at(now_local().with_timezone(&chrono::Utc))
        .metadata(AssetMetadata {
            timezone_offset: Some(39_600),
            ..AssetMetadata::default()
        })
        .build();
    db.upsert_seen(&record).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "UNSUPPORTED_CAPTURE_REPAIR",
        "original",
        &photo_path,
        &checksum,
        None,
    )
    .await
    .unwrap();
    queue_capture_repair(&db, &record).await;

    let residual = crate::download::drain_pending_metadata_rewrites(
        &db as &dyn DownloadStore,
        &MetadataConfig {
            set_exif_datetime: true,
            ..MetadataConfig::default()
        },
        CaptureTimestampRepair::ReplaceWithCaptureLocal,
        &["PrimarySync"],
        Arc::from(".metadata-test"),
        &CancellationToken::new(),
    )
    .await;

    assert_eq!(residual, 1);
    assert_eq!(db.get_pending_metadata_rewrites(1).await.unwrap().len(), 1);
    assert_eq!(std::fs::read(&photo_path).unwrap(), source);
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn refresh_drain_repairs_native_heif_capture_time_before_retiring_marker() {
    use crate::config::MetadataConfig;
    use crate::download::DownloadStore;
    use crate::state::SqliteStateDb;
    use crate::state::types::AssetMetadata;
    use little_exif::exif_tag::ExifTag;
    use little_exif::filetype::FileExtension;
    use little_exif::metadata::Metadata;

    let dir = tempfile::tempdir().unwrap();
    let photo_path = dir.path().join("native-capture-repair.heic");
    std::fs::write(
        &photo_path,
        with_capture_xmp(include_bytes!("../../../../tests/data/sample.heic")),
    )
    .unwrap();
    let checksum = crate::download::file::compute_sha256(&photo_path)
        .await
        .unwrap();

    let created_local = now_local();
    let db = SqliteStateDb::open_in_memory().unwrap();
    let record = crate::test_helpers::TestAssetRecord::new("HEIF_CAPTURE_REPAIR")
        .filename("native-capture-repair.heic")
        .checksum("provider-checksum")
        .created_at(created_local.with_timezone(&chrono::Utc))
        .metadata(AssetMetadata {
            timezone_offset: Some(39_600),
            ..AssetMetadata::default()
        })
        .build();
    db.upsert_seen(&record).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "HEIF_CAPTURE_REPAIR",
        "original",
        &photo_path,
        &checksum,
        None,
    )
    .await
    .unwrap();
    queue_capture_repair(&db, &record).await;

    let residual = crate::download::drain_pending_metadata_rewrites(
        &db as &dyn DownloadStore,
        &MetadataConfig {
            set_exif_datetime: true,
            ..MetadataConfig::default()
        },
        CaptureTimestampRepair::ReplaceWithCaptureLocal,
        &["PrimarySync"],
        Arc::from(".metadata-test"),
        &CancellationToken::new(),
    )
    .await;

    assert_eq!(residual, 0);
    assert!(
        db.get_pending_metadata_rewrites(1)
            .await
            .unwrap()
            .is_empty(),
        "the marker may retire only after native Exif is repaired"
    );
    let bytes = std::fs::read(&photo_path).unwrap();
    let tiff = crate::download::heif::extract_exif_tiff_bytes(&bytes)
        .unwrap()
        .expect("native Exif item");
    let metadata = Metadata::new_from_vec(&tiff, FileExtension::TIFF).unwrap();
    assert!(metadata
        .get_tag(&ExifTag::DateTimeOriginal(String::new()))
        .any(|tag| matches!(tag, ExifTag::DateTimeOriginal(value) if value == "2024:06:15 10:00:00")));
    assert!(
        metadata
            .get_tag(&ExifTag::OffsetTimeOriginal(String::new()))
            .any(|tag| matches!(tag, ExifTag::OffsetTimeOriginal(value) if value == "+11:00"))
    );
    let probe = crate::download::metadata::probe_exif(&photo_path).unwrap();
    assert!(probe.denotes_capture_time(&created_local));
    assert_eq!(probe.offset_time_original.as_deref(), Some("+11:00"));
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn heif_capture_repair_writes_missing_xmp_before_retiring_marker() {
    use crate::config::MetadataConfig;
    use crate::download::DownloadStore;
    use crate::state::SqliteStateDb;
    use crate::state::types::AssetMetadata;

    let dir = tempfile::tempdir().unwrap();
    let photo_path = dir.path().join("missing-xmp-capture-repair.heic");
    let source = include_bytes!("../../../../tests/data/sample.heic");
    assert!(
        crate::download::heif::extract_xmp_strict(source)
            .unwrap()
            .is_none(),
        "the fixture must start without XMP"
    );
    std::fs::write(&photo_path, source).unwrap();
    let checksum = crate::download::file::compute_sha256(&photo_path)
        .await
        .unwrap();

    let created_local = FixedOffset::east_opt(10_800)
        .unwrap()
        .with_ymd_and_hms(2023, 9, 3, 9, 28, 14)
        .unwrap();
    let db = SqliteStateDb::open_in_memory().unwrap();
    let record = crate::test_helpers::TestAssetRecord::new("HEIF_MISSING_XMP_REPAIR")
        .filename("missing-xmp-capture-repair.heic")
        .checksum("provider-checksum")
        .created_at(created_local.with_timezone(&chrono::Utc))
        .metadata(AssetMetadata {
            timezone_offset: Some(10_800),
            ..AssetMetadata::default()
        })
        .build();
    db.upsert_seen(&record).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "HEIF_MISSING_XMP_REPAIR",
        "original",
        &photo_path,
        &checksum,
        None,
    )
    .await
    .unwrap();
    queue_capture_repair(&db, &record).await;

    let residual = crate::download::drain_pending_metadata_rewrites(
        &db as &dyn DownloadStore,
        &MetadataConfig {
            set_exif_datetime: true,
            ..MetadataConfig::default()
        },
        CaptureTimestampRepair::ReplaceWithCaptureLocal,
        &["PrimarySync"],
        Arc::from(".metadata-test"),
        &CancellationToken::new(),
    )
    .await;

    assert_eq!(residual, 0);
    assert!(
        db.get_pending_metadata_rewrites(1)
            .await
            .unwrap()
            .is_empty()
    );
    let bytes = std::fs::read(&photo_path).unwrap();
    assert!(
        crate::download::heif::extract_xmp_strict(&bytes)
            .unwrap()
            .is_some(),
        "the marker may retire only after XMP is present"
    );
    let probe = crate::download::metadata::probe_exif(&photo_path).unwrap();
    assert!(probe.denotes_capture_time(&created_local));
    assert_eq!(probe.offset_time_original.as_deref(), Some("+03:00"));
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn heif_capture_repair_without_native_exif_keeps_marker_pending() {
    use crate::config::MetadataConfig;
    use crate::download::DownloadStore;
    use crate::state::SqliteStateDb;
    use crate::state::types::AssetMetadata;

    let dir = tempfile::tempdir().unwrap();
    let photo_path = dir.path().join("xmp-only-capture-repair.avif");
    let source = with_capture_xmp(include_bytes!("../../../../tests/data/white_1x1.avif"));
    assert!(
        crate::download::heif::extract_exif_tiff_bytes(&source)
            .unwrap()
            .is_none(),
        "the fixture must have no native Exif item"
    );
    std::fs::write(&photo_path, &source).unwrap();
    let checksum = crate::download::file::compute_sha256(&photo_path)
        .await
        .unwrap();

    let db = SqliteStateDb::open_in_memory().unwrap();
    let record = crate::test_helpers::TestAssetRecord::new("XMP_ONLY_CAPTURE_REPAIR")
        .filename("xmp-only-capture-repair.avif")
        .checksum("provider-checksum")
        .created_at(now_local().with_timezone(&chrono::Utc))
        .metadata(AssetMetadata {
            timezone_offset: Some(39_600),
            ..AssetMetadata::default()
        })
        .build();
    db.upsert_seen(&record).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "XMP_ONLY_CAPTURE_REPAIR",
        "original",
        &photo_path,
        &checksum,
        None,
    )
    .await
    .unwrap();
    queue_capture_repair(&db, &record).await;

    let residual = crate::download::drain_pending_metadata_rewrites(
        &db as &dyn DownloadStore,
        &MetadataConfig {
            set_exif_datetime: true,
            ..MetadataConfig::default()
        },
        CaptureTimestampRepair::ReplaceWithCaptureLocal,
        &["PrimarySync"],
        Arc::from(".metadata-test"),
        &CancellationToken::new(),
    )
    .await;

    assert_eq!(residual, 1);
    assert_eq!(
        db.get_pending_metadata_rewrites(1).await.unwrap().len(),
        1,
        "a HEIF without native Exif must retain its repair marker"
    );
    assert_eq!(
        std::fs::read(&photo_path).unwrap(),
        source,
        "a refused repair must not change the file"
    );
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn contract_xmp_gps_accuracy_requires_matching_location_embed_retry() {
    use crate::state::{AssetMetadata, SqliteStateDb};

    for fail_state_write in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.jpg");
        let sidecar = path.with_extension("jpg.xmp");
        let source = crate::test_helpers::minimal_jpeg_with_source_gps();
        std::fs::write(&path, &source).unwrap();
        let checksum = crate::download::file::compute_sha256(&path).await.unwrap();
        let db_path = dir.path().join("state.db");
        let db = SqliteStateDb::open(&db_path).await.unwrap();
        seed_downloaded_marker(
            &db,
            "GPS_EMBED_RETRY",
            "source.jpg",
            &path,
            &checksum,
            AssetMetadata {
                latitude: Some(1.5),
                longitude: Some(2.5),
                ..AssetMetadata::default()
            },
            None,
        )
        .await;
        // The native coordinate mutation succeeds, then publication and,
        // in the second case, checksum finalization fail.
        std::fs::create_dir(&sidecar).unwrap();
        if fail_state_write {
            db.fail_metadata_checksum_write_for_test();
        }
        let pass = run_pending(
            &db,
            MetadataFlags::GPS | MetadataFlags::XMP_SIDECAR,
            Arc::from(".gps-test"),
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(pass.failed, 1);
        assert_eq!(db.get_pending_metadata_rewrites(10).await.unwrap().len(), 1);
        let rewritten = std::fs::read(&path).unwrap();
        assert_ne!(rewritten, source);
        let native = crate::download::metadata::read_source_gps(&path).unwrap();
        assert_eq!((native.latitude, native.longitude), (Some(1.5), Some(2.5)));
        assert!(
            native.horizontal_positioning_error.is_some(),
            "native accuracy must not be deleted"
        );
        let checksums = stored_checksums(&db).await;
        if fail_state_write {
            assert_eq!(checksums, (None, None));
        } else {
            assert_eq!(checksums.1.as_deref(), Some(checksum.as_str()));
        }
        drop(db);
        std::fs::remove_dir(&sidecar).unwrap();
        let db = SqliteStateDb::open(&db_path).await.unwrap();
        let pass = run_pending(
            &db,
            MetadataFlags::XMP_SIDECAR,
            Arc::from(".gps-test"),
            &CancellationToken::new(),
        )
        .await;
        assert_eq!((pass.applied, pass.failed), (1, 0));
        let xmp: XmpMeta = std::fs::read_to_string(&sidecar).unwrap().parse().unwrap();
        assert!(!xmp.contains_property(xmp_ns::EXIF, "GPSHPositioningError"));
        assert_eq!(
            xmp.property(xmp_ns::EXIF, "GPSLatitude").unwrap().value,
            "1,30.0000N"
        );
        assert_eq!(std::fs::read(&path).unwrap(), rewritten);
        assert!(
            db.get_pending_metadata_rewrites(10)
                .await
                .unwrap()
                .is_empty()
        );

        // A later no-op embed may establish a local checksum after failed
        // finalization. It must not promote those rewritten bytes into
        // original-source evidence for the next sidecar-only retry.
        for flags in [
            MetadataFlags::GPS | MetadataFlags::XMP_SIDECAR,
            MetadataFlags::XMP_SIDECAR,
        ] {
            db.record_metadata_write_failure("PrimarySync", "GPS_EMBED_RETRY", "original")
                .await
                .unwrap();
            let pass = run_pending(
                &db,
                flags,
                Arc::from(".gps-test"),
                &CancellationToken::new(),
            )
            .await;
            assert_eq!((pass.applied, pass.failed), (1, 0));
            let xmp: XmpMeta = std::fs::read_to_string(&sidecar).unwrap().parse().unwrap();
            assert!(!xmp.contains_property(xmp_ns::EXIF, "GPSHPositioningError"));
            assert_eq!(std::fs::read(&path).unwrap(), rewritten);
        }
        let before = std::fs::read(&sidecar).unwrap();
        let steady = run_pending(
            &db,
            MetadataFlags::XMP_SIDECAR,
            Arc::from(".gps-test"),
            &CancellationToken::new(),
        )
        .await;
        assert_eq!((steady.applied, steady.failed), (0, 0));
        assert_eq!(std::fs::read(&sidecar).unwrap(), before);
        assert!(
            db.get_pending_metadata_rewrites(10)
                .await
                .unwrap()
                .is_empty()
        );
    }
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn contract_xmp_gps_accuracy_requires_matching_location_checksum_recovery() {
    use crate::state::{AssetMetadata, SqliteStateDb};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("source.jpg");
    let sidecar = path.with_extension("jpg.xmp");
    std::fs::write(&path, crate::test_helpers::minimal_jpeg_with_source_gps()).unwrap();
    let original_checksum = crate::download::file::compute_sha256(&path).await.unwrap();
    let original_gps = crate::download::metadata::read_source_gps(&path).unwrap();
    assert!(original_gps.latitude.is_none() && original_gps.longitude.is_none());
    assert!(original_gps.horizontal_positioning_error.is_some());
    let db_path = dir.path().join("state.db");
    let db = SqliteStateDb::open(&db_path).await.unwrap();
    let mut metadata = AssetMetadata {
        rating: Some(1),
        latitude: Some(1.5),
        longitude: Some(2.5),
        ..AssetMetadata::default()
    };
    seed_downloaded_marker(
        &db,
        "PROVENANCE",
        "source.jpg",
        &path,
        &original_checksum,
        metadata.clone(),
        Some(now_local().to_utc()),
    )
    .await;
    db.fail_metadata_checksum_write_for_test();
    let first = run_pending(
        &db,
        MetadataFlags::GPS | MetadataFlags::RATING | MetadataFlags::XMP_SIDECAR,
        Arc::from(".review"),
        &CancellationToken::new(),
    )
    .await;
    assert_eq!(first.failed, 1);
    assert_eq!(stored_checksums(&db).await, (None, None));
    let gps = crate::download::metadata::read_source_gps(&path).unwrap();
    assert_eq!((gps.latitude, gps.longitude), (Some(1.5), Some(2.5)));
    drop(db);

    let db = SqliteStateDb::open(&db_path).await.unwrap();
    let retry = run_pending(
        &db,
        MetadataFlags::RATING | MetadataFlags::XMP_SIDECAR,
        Arc::from(".review"),
        &CancellationToken::new(),
    )
    .await;
    assert_eq!((retry.applied, retry.failed), (1, 0));
    let rewritten_checksum = crate::download::file::compute_sha256(&path).await.unwrap();
    assert_ne!(rewritten_checksum, original_checksum);
    assert_eq!(
        stored_checksums(&db).await,
        (Some(rewritten_checksum.clone()), None)
    );
    for rating in [2, 1] {
        metadata.rating = Some(rating);
        db.refresh_downloaded_asset_metadata(
            "PrimarySync",
            "PROVENANCE",
            (
                &crate::state::MetadataCapture {
                    shared: Arc::new(metadata.clone()),
                    renditions: Arc::from([]),
                },
                now_local().to_utc(),
                None,
            ),
            true,
            false,
            crate::state::METADATA_CAPTURE_REVISION,
        )
        .await
        .unwrap();
        let pass = run_pending(
            &db,
            MetadataFlags::RATING | MetadataFlags::XMP_SIDECAR,
            Arc::from(".review"),
            &CancellationToken::new(),
        )
        .await;
        assert_eq!((pass.applied, pass.failed), (1, 0));
    }
    assert_eq!(
        stored_checksums(&db).await,
        (Some(rewritten_checksum.clone()), Some(rewritten_checksum))
    );
    let xmp: XmpMeta = std::fs::read_to_string(&sidecar).unwrap().parse().unwrap();
    assert!(
        !xmp.contains_property(xmp_ns::EXIF, "GPSHPositioningError"),
        "kei-inserted coordinates acquired fabricated original-byte provenance"
    );
    let media = std::fs::read(&path).unwrap();
    let sidecar_bytes = std::fs::read(&sidecar).unwrap();
    drop(db);
    let db = SqliteStateDb::open(&db_path).await.unwrap();
    db.record_metadata_write_failure("PrimarySync", "PROVENANCE", "original")
        .await
        .unwrap();
    let pending = db
        .get_pending_metadata_rewrites_page_for_queue(MetadataRewriteQueue::Ordinary, None, 0, 10)
        .await
        .unwrap();
    assert!(pending[0].source_checksum.is_none());
    let retry = run_pending(
        &db,
        MetadataFlags::XMP_SIDECAR,
        Arc::from(".review"),
        &CancellationToken::new(),
    )
    .await;
    assert_eq!((retry.applied, retry.failed), (1, 0));
    let steady = run_pending(
        &db,
        MetadataFlags::XMP_SIDECAR,
        Arc::from(".review"),
        &CancellationToken::new(),
    )
    .await;
    assert_eq!((steady.applied, steady.failed), (0, 0));
    assert_eq!(std::fs::read(&path).unwrap(), media);
    assert_eq!(std::fs::read(&sidecar).unwrap(), sidecar_bytes);
    assert!(
        db.get_pending_metadata_rewrites(10)
            .await
            .unwrap()
            .is_empty()
    );
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn contract_xmp_gps_accuracy_requires_matching_location_rewrite_and_retry() {
    use crate::state::{AssetMetadata, DownloadStateStore, SqliteStateDb};
    use xmp_toolkit::XmpValue;

    for owned in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.jpg");
        let sidecar_path = path.with_extension("jpg.xmp");
        let source = crate::test_helpers::minimal_jpeg_with_source_gps_and_location();
        std::fs::write(&path, &source).unwrap();
        let checksum = crate::download::file::compute_sha256(&path).await.unwrap();
        let db = SqliteStateDb::open(&dir.path().join("state.db"))
            .await
            .unwrap();
        let mut metadata = AssetMetadata {
            latitude: Some(1.5),
            longitude: Some(2.5),
            ..AssetMetadata::default()
        };
        seed_downloaded_marker(
            &db,
            "GPS_PROVENANCE",
            "source.jpg",
            &path,
            &checksum,
            metadata.clone(),
            Some(now_local().to_utc()),
        )
        .await;
        db.mark_verified_download(
            "PrimarySync",
            "GPS_PROVENANCE",
            "original",
            &path,
            &checksum,
            Some(&checksum),
            false,
        )
        .await
        .unwrap();
        db.record_metadata_write_failure("PrimarySync", "GPS_PROVENANCE", "original")
            .await
            .unwrap();
        let token = CancellationToken::new();
        let drain = || {
            run_pending(
                &db,
                MetadataFlags::XMP_SIDECAR,
                Arc::from(".gps-test"),
                &token,
            )
        };
        assert_eq!(drain().await.applied, 1);
        let mut initial: XmpMeta = std::fs::read_to_string(&sidecar_path)
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(
            initial
                .property(xmp_ns::EXIF, "GPSHPositioningError")
                .unwrap()
                .value,
            "3/2"
        );
        if !owned {
            initial
                .delete_property("https://github.com/rhoopr/kei/ns/1.0/", "managedFields")
                .unwrap();
        }
        initial
            .set_property(xmp_ns::XMP, "Label", &XmpValue::new("keep me".to_owned()))
            .unwrap();
        std::fs::write(&sidecar_path, initial.to_string()).unwrap();
        assert!(
            db.get_pending_metadata_rewrites(10)
                .await
                .unwrap()
                .is_empty()
        );

        metadata.latitude = Some(12.3456);
        metadata.longitude = Some(-78.9012);
        db.refresh_downloaded_asset_metadata(
            "PrimarySync",
            "GPS_PROVENANCE",
            (
                &crate::state::MetadataCapture {
                    shared: Arc::new(metadata.clone()),
                    renditions: Arc::from([]),
                },
                now_local().to_utc(),
                None,
            ),
            true,
            false,
            crate::state::METADATA_CAPTURE_REVISION,
        )
        .await
        .unwrap();

        // A source read failure is unknown: preserve the old accuracy and
        // its ownership while publishing current provider coordinates.
        let saved_media = dir.path().join("saved.jpg");
        std::fs::rename(&path, &saved_media).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert_eq!(drain().await.failed, 1);
        let unknown: XmpMeta = std::fs::read_to_string(&sidecar_path)
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(
            unknown
                .property(xmp_ns::EXIF, "GPSHPositioningError")
                .unwrap()
                .value,
            "3/2"
        );
        assert_eq!(
            unknown.property(xmp_ns::EXIF, "GPSLatitude").unwrap().value,
            "12,20.7360N"
        );
        assert_eq!(db.get_pending_metadata_rewrites(10).await.unwrap().len(), 1);
        std::fs::remove_dir(&path).unwrap();
        std::fs::rename(&saved_media, &path).unwrap();

        // Publication failure must not consume the pending correction.
        let saved_sidecar = dir.path().join("saved.xmp");
        let before_failure = std::fs::read(&sidecar_path).unwrap();
        std::fs::rename(&sidecar_path, &saved_sidecar).unwrap();
        std::fs::create_dir(&sidecar_path).unwrap();
        assert_eq!(drain().await.failed, 1);
        assert!(sidecar_path.is_dir());
        assert_eq!(std::fs::read(&saved_sidecar).unwrap(), before_failure);
        assert_eq!(db.get_pending_metadata_rewrites(10).await.unwrap().len(), 1);
        std::fs::remove_dir(&sidecar_path).unwrap();
        std::fs::rename(&saved_sidecar, &sidecar_path).unwrap();

        assert_eq!(drain().await.applied, 1);
        let corrected_bytes = std::fs::read(&sidecar_path).unwrap();
        let corrected: XmpMeta = std::str::from_utf8(&corrected_bytes)
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(
            corrected.contains_property(xmp_ns::EXIF, "GPSHPositioningError"),
            !owned
        );
        if !owned {
            assert_eq!(
                corrected
                    .property(xmp_ns::EXIF, "GPSHPositioningError")
                    .unwrap()
                    .value,
                "3/2"
            );
        }
        assert_eq!(
            corrected
                .property(xmp_ns::EXIF, "GPSLatitude")
                .unwrap()
                .value,
            "12,20.7360N"
        );
        assert_eq!(
            corrected
                .property(xmp_ns::EXIF, "GPSLongitude")
                .unwrap()
                .value,
            "78,54.0720W"
        );
        assert_eq!(
            corrected.property(xmp_ns::XMP, "Label").unwrap().value,
            "keep me"
        );
        assert!(
            db.get_pending_metadata_rewrites(10)
                .await
                .unwrap()
                .is_empty()
        );
        let steady = drain().await;
        assert_eq!((steady.applied, steady.failed), (0, 0));
        assert_eq!(std::fs::read(&sidecar_path).unwrap(), corrected_bytes);
        assert_eq!(std::fs::read(&path).unwrap(), source);
        assert_eq!(
            stored_checksums(&db).await.0.as_deref(),
            Some(checksum.as_str())
        );
    }
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn sidecar_retry_recovers_exif_less_media_and_keeps_marker_for_unreadable_source() {
    use crate::state::{AssetMetadata, SqliteStateDb};

    let dir = tempfile::tempdir().expect("metadata temp dir");

    // A structurally valid HEIC with no EXIF block. There is no source GPS
    // to read, so the sidecar carries only the CloudKit payload and the
    // marker must retire.
    let exif_less_path = dir.path().join("exif-less.heic");
    std::fs::write(
        &exif_less_path,
        crate::test_helpers::heif_ftyp_without_meta_bytes(),
    )
    .expect("write EXIF-less HEIC");

    // A source the reader cannot open at all. Planning the sidecar fails,
    // so the durable marker survives for a future retry.
    let unreadable_path = dir.path().join("unreadable.heic");
    std::fs::create_dir(&unreadable_path).expect("create unreadable source");

    let db = SqliteStateDb::open_in_memory().expect("metadata state DB");
    let exif_less_checksum = crate::download::file::compute_sha256(&exif_less_path)
        .await
        .expect("media checksum");
    seed_downloaded_marker(
        &db,
        "GPS_EXIF_LESS",
        "exif-less.heic",
        &exif_less_path,
        &exif_less_checksum,
        AssetMetadata {
            metadata_hash: Some("exif-less-hash".into()),
            ..AssetMetadata::default()
        },
        None,
    )
    .await;
    seed_downloaded_marker(
        &db,
        "GPS_UNREADABLE",
        "unreadable.heic",
        &unreadable_path,
        "0000000000000000000000000000000000000000000000000000000000000000",
        AssetMetadata {
            metadata_hash: Some("unreadable-hash".into()),
            ..AssetMetadata::default()
        },
        None,
    )
    .await;

    let pass = run_pending(
        &db,
        MetadataFlags::XMP_SIDECAR,
        Arc::from(".gps-parse-retry-test"),
        &CancellationToken::new(),
    )
    .await;

    assert_eq!(
        pass.applied, 1,
        "EXIF-less media must still complete its sidecar"
    );
    assert_eq!(
        pass.failed, 1,
        "an unreadable source must fail so the retry marker survives"
    );
    assert!(
        exif_less_path.with_file_name("exif-less.heic.xmp").exists(),
        "EXIF-less media must still publish its sidecar"
    );
    assert!(
        unreadable_path
            .with_file_name("unreadable.heic.xmp")
            .exists(),
        "an unreadable source still publishes the CloudKit-only sidecar"
    );

    let pending = db
        .get_pending_metadata_rewrites(10)
        .await
        .expect("read retry markers");
    assert_eq!(
        pending.len(),
        1,
        "only the unreadable source keeps its durable marker"
    );
    assert_eq!(
        pending[0].id.as_ref(),
        "GPS_UNREADABLE",
        "the retained marker must be the unreadable source"
    );
}

/// End-to-end test of the metadata-rewrite pass. Seeds a downloaded row
/// with a `metadata_write_failed_at` marker, then proves the configured
/// metadata is applied while durable download state remains coherent.
#[cfg(feature = "xmp")]
#[tokio::test]
async fn run_pending_applies_embed_and_clears_marker() {
    use crate::state::types::AssetMetadata;
    use crate::state::{AssetStatus, SqliteStateDb};

    let dir = tempfile::tempdir().unwrap();
    let photo_path = dir.path().join("rewrite_target.jpg");
    std::fs::write(&photo_path, minimal_jpeg_bytes()).unwrap();

    let db = SqliteStateDb::open_in_memory().unwrap();
    let seeded_checksum = crate::download::file::compute_sha256(&photo_path)
        .await
        .unwrap();

    let seeded_hash = "seed_hash_before_rewrite".to_string();
    let metadata = AssetMetadata {
        rating: Some(4),
        timezone_offset: Some(39_600),
        metadata_hash: Some(seeded_hash.clone()),
        ..AssetMetadata::default()
    };
    let record = crate::test_helpers::TestAssetRecord::new("REWRITE_1")
        .filename("rewrite_target.jpg")
        .checksum("rewrite_ck")
        .size(22)
        .created_at(
            chrono::Utc
                .with_ymd_and_hms(2026, 1, 31, 22, 31, 59)
                .unwrap(),
        )
        .metadata(metadata)
        .build();
    db.upsert_seen(&record).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "REWRITE_1",
        "original",
        &photo_path,
        &seeded_checksum,
        None,
    )
    .await
    .unwrap();
    db.record_metadata_write_failure("PrimarySync", "REWRITE_1", "original")
        .await
        .unwrap();

    // Sanity: the rewrite pass sees our row.
    let pending = db.get_pending_metadata_rewrites(32).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(&*pending[0].id, "REWRITE_1");

    let flags = MetadataFlags::DATETIME | MetadataFlags::RATING | MetadataFlags::EMBED_XMP;
    let token = CancellationToken::new();
    run_pending(&db, flags, Arc::from(".meta-tmp"), &token).await;

    // Marker must be gone; row must still be `downloaded`.
    let remaining = db.get_pending_metadata_rewrites(32).await.unwrap();
    assert!(
        remaining.is_empty(),
        "marker must be cleared after successful rewrite"
    );
    let summary = db.get_summary().await.unwrap();
    assert_eq!(summary.downloaded, 1);

    // metadata_hash must have been refreshed. We don't care what the
    // new hash value is - only that it reflects the rewrite pass ran
    // to completion (not the seeded placeholder).
    let hashes = db.get_downloaded_metadata_hashes().await.unwrap();
    let new_hash = hashes
        .get(&(
            "PrimarySync".to_string(),
            "REWRITE_1".to_string(),
            "original".to_string(),
        ))
        .expect("row must remain in the downloaded set");
    assert_eq!(
        new_hash, &seeded_hash,
        "a successful rewrite leaves the recorded metadata_hash in place"
    );

    let current_checksum = crate::download::file::compute_sha256(&photo_path)
        .await
        .unwrap();
    assert_ne!(current_checksum, seeded_checksum);
    let downloaded = db.get_downloaded_page(0, 1).await.unwrap();
    assert_eq!(downloaded[0].checksum.as_ref(), "rewrite_ck");
    assert_eq!(
        downloaded[0].local_checksum.as_deref(),
        Some(current_checksum.as_str())
    );

    let probe = crate::download::metadata::probe_exif(&photo_path).unwrap();
    assert_eq!(
        probe.datetime_original.as_deref(),
        Some("2026-02-01T09:31:59")
    );
    assert_eq!(probe.offset_time_original.as_deref(), Some("+11:00"));

    let bytes = std::fs::read(&photo_path).unwrap();
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        text.contains("Rating") || text.contains("rating"),
        "embed should have written a Rating property into the JPEG"
    );

    // summary.downloaded == 1 above already proves the row stayed in
    // the downloaded state; AssetStatus is referenced here for
    // documentation and as an import check.
    let _ = AssetStatus::Downloaded;
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn run_pending_matches_normal_grouped_xmp_payload() {
    use crate::state::SqliteStateDb;
    use crate::state::types::AssetMetadata;
    use xmp_toolkit::{OpenFileOptions, XmpFile};

    let dir = tempfile::tempdir().unwrap();
    let photo_path = dir.path().join("grouped.jpg");
    std::fs::write(&photo_path, minimal_jpeg_bytes()).unwrap();
    let checksum = crate::download::file::compute_sha256(&photo_path)
        .await
        .unwrap();
    let metadata = AssetMetadata {
        keywords: Some(r#"["beach"]"#.into()),
        metadata_hash: Some("grouped-hash".into()),
        ..AssetMetadata::default()
    };
    let record = crate::test_helpers::TestAssetRecord::new("GROUPED")
        .filename("grouped.jpg")
        .metadata(metadata)
        .build();
    let db = SqliteStateDb::open_in_memory().unwrap();
    db.upsert_seen(&record).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "GROUPED",
        "original",
        &photo_path,
        &checksum,
        None,
    )
    .await
    .unwrap();
    db.record_metadata_write_failure("PrimarySync", "GROUPED", "original")
        .await
        .unwrap();
    db.add_asset_album("PrimarySync", "GROUPED", "Vacation", "icloud")
        .await
        .unwrap();
    db.add_asset_album("PrimarySync", "GROUPED", "beach", "icloud")
        .await
        .unwrap();
    {
        let conn = db.acquire_lock("seed person").unwrap();
        conn.execute(
            "INSERT INTO asset_people (library, asset_id, person_name) \
             VALUES ('PrimarySync', 'GROUPED', 'Alice')",
            [],
        )
        .unwrap();
    }

    let grouping_rows = db
        .get_asset_groupings("PrimarySync", &["GROUPED"])
        .await
        .unwrap();
    let mut normal_groupings = AssetGroupings::default();
    for (asset_id, album) in grouping_rows.albums {
        normal_groupings
            .albums
            .entry(asset_id)
            .or_default()
            .push(album);
    }
    for (asset_id, person) in grouping_rows.people {
        normal_groupings
            .people
            .entry(asset_id)
            .or_default()
            .push(person);
    }
    let normal_payload = normal_groupings.metadata_payload("GROUPED", &record.metadata);
    assert_eq!(normal_payload.keywords, vec!["beach", "Vacation"]);
    assert_eq!(normal_payload.people, vec!["Alice"]);

    let normal_path = dir.path().join("normal.jpg");
    std::fs::write(&normal_path, minimal_jpeg_bytes()).unwrap();
    let normal_outcome = write_download_metadata(MetadataWriteRequest {
        final_path: &normal_path,
        embed_path: Some(&normal_path),
        expected_embed_fingerprint: None,
        source_checksum: None,
        sidecar_path: Some(&normal_path),
        payload: Arc::new(normal_payload),
        created_local: record.metadata.capture_local(record.created_at),
        flags: MetadataFlags::EMBED_XMP | MetadataFlags::XMP_SIDECAR,
        capture_timestamp_repair: CaptureTimestampRepair::Preserve,
        temp_suffix: ".meta-tmp",
    })
    .await;
    assert!(!normal_outcome.any_failed());

    let mut normal_file = XmpFile::new().unwrap();
    normal_file
        .open_file(&normal_path, OpenFileOptions::default().for_read())
        .unwrap();
    let normal_embedded = normal_file.xmp().expect("normal embedded XMP");
    let normal_sidecar = std::fs::read_to_string(dir.path().join("normal.jpg.xmp"))
        .unwrap()
        .parse()
        .unwrap();

    let pass = run_pending(
        &db,
        MetadataFlags::EMBED_XMP | MetadataFlags::XMP_SIDECAR,
        Arc::from(".meta-tmp"),
        &CancellationToken::new(),
    )
    .await;
    assert_eq!(pass.applied, 1);

    let mut embedded_file = XmpFile::new().unwrap();
    embedded_file
        .open_file(&photo_path, OpenFileOptions::default().for_read())
        .unwrap();
    assert_eq!(
        grouped_xmp(&embedded_file.xmp().expect("rewritten embedded XMP")),
        grouped_xmp(&normal_embedded)
    );

    let sidecar = std::fs::read_to_string(dir.path().join("grouped.jpg.xmp"))
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(grouped_xmp(&sidecar), grouped_xmp(&normal_sidecar));
    assert_eq!(
        grouped_xmp(&sidecar),
        (
            vec!["beach".into(), "Vacation".into()],
            vec!["Alice".into()]
        )
    );
    assert!(
        db.get_pending_metadata_rewrites(1)
            .await
            .unwrap()
            .is_empty()
    );
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn run_pending_applies_heic_rating_and_records_rewrite_checksums() {
    use crate::state::SqliteStateDb;
    use crate::state::types::AssetMetadata;

    let dir = tempfile::tempdir().unwrap();
    let photo_path = dir.path().join("rewrite_target.heic");
    let source = crate::download::heif::apple_tmap_insertion_heic(
        &crate::test_helpers::minimal_tiff_with_source_gps(),
        b"<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF><rdf:Description xmlns:HDRGainMap='http://ns.apple.com/HDRGainMap/1.0/' HDRGainMap:HDRGainMapHeadroom='2.67'/></rdf:RDF></x:xmpmeta>",
    );
    std::fs::write(&photo_path, &source).unwrap();

    let db = SqliteStateDb::open_in_memory().unwrap();
    let seeded_checksum = crate::download::file::compute_sha256(&photo_path)
        .await
        .unwrap();
    let record = crate::test_helpers::TestAssetRecord::new("REWRITE_HEIC")
        .filename("rewrite_target.heic")
        .checksum("rewrite_heic_ck")
        .size(source.len() as u64)
        .metadata(AssetMetadata {
            rating: Some(5),
            metadata_hash: Some("heic_hash".to_string()),
            ..AssetMetadata::default()
        })
        .build();
    db.upsert_seen(&record).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "REWRITE_HEIC",
        "original",
        &photo_path,
        &seeded_checksum,
        None,
    )
    .await
    .unwrap();
    db.record_metadata_write_failure("PrimarySync", "REWRITE_HEIC", "original")
        .await
        .unwrap();

    let pass = run_pending(
        &db,
        MetadataFlags::RATING | MetadataFlags::EMBED_XMP | MetadataFlags::XMP_SIDECAR,
        Arc::from(".meta-tmp"),
        &CancellationToken::new(),
    )
    .await;
    assert_eq!(pass.applied, 1);
    assert!(
        db.get_pending_metadata_rewrites(10)
            .await
            .unwrap()
            .is_empty()
    );

    let bytes = std::fs::read(&photo_path).unwrap();
    let xmp = crate::download::heif::extract_xmp_bytes(&bytes).expect("HEIC XMP");
    let meta: xmp_toolkit::XmpMeta = std::str::from_utf8(&xmp).unwrap().parse().unwrap();
    assert_eq!(
        meta.property_i32(xmp_toolkit::xmp_ns::XMP, "Rating")
            .unwrap()
            .value,
        5
    );
    let sidecar = std::fs::read_to_string(photo_path.with_file_name("rewrite_target.heic.xmp"))
        .expect("read HEIC sidecar");
    let sidecar: xmp_toolkit::XmpMeta = sidecar.parse().expect("parse HEIC sidecar");
    crate::test_helpers::assert_source_gps_in_xmp(&sidecar);

    let (local_checksum, download_checksum) = stored_checksums(&db).await;
    assert_ne!(local_checksum.as_deref(), Some(seeded_checksum.as_str()));
    assert_eq!(download_checksum.as_deref(), Some(seeded_checksum.as_str()));
    assert!(
        !photo_path
            .with_file_name("rewrite_target.heic.meta-tmp")
            .exists()
    );
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn run_pending_keeps_marker_for_conflicting_tone_map_xmp_ownership() {
    use crate::state::SqliteStateDb;
    use crate::state::types::AssetMetadata;

    let dir = tempfile::tempdir().unwrap();
    let photo_path = dir.path().join("conflicting_tone_map_xmp.heic");
    let source = crate::download::heif::apple_tmap_conflicting_xmp_heic(
        &crate::test_helpers::minimal_tiff_with_source_gps(),
        b"<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF><rdf:Description xmlns:HDRGainMap='http://ns.apple.com/HDRGainMap/1.0/' HDRGainMap:HDRGainMapHeadroom='2.67'/></rdf:RDF></x:xmpmeta>",
    );
    std::fs::write(&photo_path, &source).unwrap();

    let db = SqliteStateDb::open_in_memory().unwrap();
    let seeded_checksum = crate::download::file::compute_sha256(&photo_path)
        .await
        .unwrap();
    let record = crate::test_helpers::TestAssetRecord::new("CONFLICTING_TMAP_XMP")
        .filename("conflicting_tone_map_xmp.heic")
        .checksum("conflicting_tmap_xmp_ck")
        .size(source.len() as u64)
        .metadata(AssetMetadata {
            rating: Some(5),
            metadata_hash: Some("conflicting_tmap_xmp_hash".to_string()),
            ..AssetMetadata::default()
        })
        .build();
    db.upsert_seen(&record).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "CONFLICTING_TMAP_XMP",
        "original",
        &photo_path,
        &seeded_checksum,
        None,
    )
    .await
    .unwrap();
    db.record_metadata_write_failure("PrimarySync", "CONFLICTING_TMAP_XMP", "original")
        .await
        .unwrap();

    let pass = run_pending(
        &db,
        MetadataFlags::RATING | MetadataFlags::EMBED_XMP,
        Arc::from(".meta-tmp"),
        &CancellationToken::new(),
    )
    .await;

    assert_eq!(pass.applied, 0);
    assert_eq!(pass.failed, 1);
    assert_eq!(std::fs::read(&photo_path).unwrap(), source);
    assert_eq!(db.get_pending_metadata_rewrites(10).await.unwrap().len(), 1);
    let (local_checksum, download_checksum) = stored_checksums(&db).await;
    assert_eq!(local_checksum.as_deref(), Some(seeded_checksum.as_str()));
    assert_eq!(download_checksum, None);
    assert!(
        !photo_path
            .with_file_name("conflicting_tone_map_xmp.heic.meta-tmp")
            .exists()
    );
}

/// If the on-disk file has vanished between tagging and the rewrite
/// pass, the pass must not error out. The marker stays, so a future
/// sync that re-downloads the asset re-drives the writer.
#[cfg(feature = "xmp")]
#[tokio::test]
async fn run_pending_skips_missing_file_and_leaves_marker() {
    use crate::state::SqliteStateDb;
    use crate::state::types::AssetMetadata;

    let dir = tempfile::tempdir().unwrap();
    let vanished_path = dir.path().join("never_written.jpg");

    let db = SqliteStateDb::open_in_memory().unwrap();

    let metadata = AssetMetadata {
        rating: Some(3),
        metadata_hash: Some("untouched_hash".to_string()),
        ..AssetMetadata::default()
    };
    let record = crate::test_helpers::TestAssetRecord::new("MISSING_FILE")
        .filename("never_written.jpg")
        .metadata(metadata)
        .build();
    db.upsert_seen(&record).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "MISSING_FILE",
        "original",
        &vanished_path,
        "checksum123",
        None,
    )
    .await
    .unwrap();
    db.record_metadata_write_failure("PrimarySync", "MISSING_FILE", "original")
        .await
        .unwrap();

    let flags = MetadataFlags::RATING | MetadataFlags::EMBED_XMP;
    let token = CancellationToken::new();
    run_pending(&db, flags, Arc::from(".meta-tmp"), &token).await;

    let still_pending = db.get_pending_metadata_rewrites(32).await.unwrap();
    assert_eq!(
        still_pending.len(),
        1,
        "marker must survive when the file is absent so a future sync retries"
    );
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn cancel_returns_partial_and_keeps_retry_marker() {
    use crate::state::SqliteStateDb;
    use crate::state::types::AssetMetadata;

    let dir = tempfile::tempdir().unwrap();
    let photo_path = dir.path().join("rewrite_cancel.jpg");
    std::fs::write(&photo_path, minimal_jpeg_bytes()).unwrap();

    let db = SqliteStateDb::open_in_memory().unwrap();
    let metadata = AssetMetadata {
        rating: Some(5),
        metadata_hash: Some("retry_hash".to_string()),
        ..AssetMetadata::default()
    };
    let record = crate::test_helpers::TestAssetRecord::new("REWRITE_CANCEL")
        .filename("rewrite_cancel.jpg")
        .checksum("rewrite_cancel_ck")
        .metadata(metadata)
        .build();
    db.upsert_seen(&record).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "REWRITE_CANCEL",
        "original",
        &photo_path,
        "rewrite_cancel_ck",
        None,
    )
    .await
    .unwrap();
    db.record_metadata_write_failure("PrimarySync", "REWRITE_CANCEL", "original")
        .await
        .unwrap();

    let flags = MetadataFlags::RATING | MetadataFlags::EMBED_XMP;
    let token = CancellationToken::new();
    token.cancel();
    let deferred = run_pending(&db, flags, Arc::from(".meta-tmp"), &token)
        .await
        .failed;

    assert_eq!(
        deferred, 1,
        "cancelled metadata rewrite must count as a partial retryable item"
    );
    let still_pending = db.get_pending_metadata_rewrites(32).await.unwrap();
    assert_eq!(
        still_pending.len(),
        1,
        "cancelled metadata rewrite must keep marker for retry"
    );
}

#[tokio::test]
async fn run_pending_batch_is_bounded() {
    use crate::state::SqliteStateDb;

    let db = SqliteStateDb::open_in_memory().unwrap();
    for i in 0..(METADATA_REWRITE_BATCH + 100) {
        let id = format!("A{i}");
        let record = crate::test_helpers::TestAssetRecord::new(&id).build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            &id,
            "original",
            std::path::Path::new("/nonexistent/missing.jpg"),
            "ck",
            None,
        )
        .await
        .unwrap();
        db.record_metadata_write_failure("PrimarySync", &id, "original")
            .await
            .unwrap();
    }

    let token = CancellationToken::new();
    let pass = run_pending(
        &db,
        MetadataFlags::RATING,
        std::sync::Arc::from(".meta-tmp"),
        &token,
    )
    .await;
    assert_eq!(
        pass.fetched, METADATA_REWRITE_BATCH,
        "one pass fetches at most a bounded batch, never the whole queue"
    );
    assert_eq!(pass.applied, 0, "missing files apply nothing");
}

#[tokio::test]
async fn drain_scope_skips_unselected_and_soft_deleted_failures() {
    use crate::config::MetadataConfig;
    use crate::download::DownloadStore;
    use crate::state::SqliteStateDb;

    let db = SqliteStateDb::open_in_memory().unwrap();
    for i in 0..3 {
        let id = format!("M{i}");
        let record = crate::test_helpers::TestAssetRecord::new(&id).build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            &id,
            "original",
            std::path::Path::new("/nonexistent/missing.jpg"),
            "ck",
            None,
        )
        .await
        .unwrap();
        db.record_metadata_write_failure("PrimarySync", &id, "original")
            .await
            .unwrap();
    }

    let invalid_dir = tempfile::tempdir().unwrap();
    let invalid_path = invalid_dir.path().join("invalid.jpg");
    std::fs::write(&invalid_path, b"not an image").unwrap();
    for (library, id, soft_deleted) in [
        ("SharedSync-OTHER", "UNSELECTED", false),
        ("PrimarySync", "SOFT_DELETED", true),
    ] {
        let record = crate::test_helpers::TestAssetRecord::new(id)
            .library(library)
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(library, id, "original", &invalid_path, "ck", None)
            .await
            .unwrap();
        db.record_metadata_write_failure(library, id, "original")
            .await
            .unwrap();
        if soft_deleted {
            db.mark_soft_deleted(library, id, None).await.unwrap();
        }
    }

    let cfg = MetadataConfig {
        set_exif_rating: true,
        ..MetadataConfig::default()
    };
    let token = CancellationToken::new();
    let residual = crate::download::drain_pending_metadata_rewrites(
        &db as &dyn DownloadStore,
        &cfg,
        CaptureTimestampRepair::Preserve,
        &["PrimarySync"],
        std::sync::Arc::from(".meta-tmp"),
        &token,
    )
    .await;
    assert_eq!(
        residual, 0,
        "unselected and soft-deleted failures must not fail the selected repair"
    );
    let pending = db.get_pending_metadata_rewrites(32).await.unwrap();
    assert_eq!(pending.len(), 4);
    assert!(
        pending
            .iter()
            .any(|record| record.id.as_ref() == "UNSELECTED"),
        "unselected library marker must remain untouched"
    );
    assert!(
        !pending
            .iter()
            .any(|record| record.id.as_ref() == "SOFT_DELETED"),
        "a soft-deleted row must not be offered for rewrite, since its \
         metadata is frozen at the values held before the deletion"
    );
}

#[tokio::test]
async fn drain_reports_residual_on_cancellation() {
    use crate::config::MetadataConfig;
    use crate::download::DownloadStore;
    use crate::state::SqliteStateDb;

    let db = SqliteStateDb::open_in_memory().unwrap();
    let record = crate::test_helpers::TestAssetRecord::new("C1").build();
    db.upsert_seen(&record).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "C1",
        "original",
        std::path::Path::new("/x/c1.jpg"),
        "ck",
        None,
    )
    .await
    .unwrap();
    db.record_metadata_write_failure("PrimarySync", "C1", "original")
        .await
        .unwrap();

    let cfg = MetadataConfig {
        set_exif_rating: true,
        ..MetadataConfig::default()
    };
    let token = CancellationToken::new();
    token.cancel();
    let residual = crate::download::drain_pending_metadata_rewrites(
        &db as &dyn DownloadStore,
        &cfg,
        CaptureTimestampRepair::Preserve,
        &["PrimarySync"],
        std::sync::Arc::from(".meta-tmp"),
        &token,
    )
    .await;
    assert!(
        residual >= 1,
        "a cancelled drain must report a non-zero residual so the sync exits non-zero"
    );
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn drain_stops_when_rewrite_marker_cannot_be_cleared() {
    use crate::config::MetadataConfig;
    use crate::state::types::AssetMetadata;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("clear-failure.jpg");
    std::fs::write(&path, minimal_jpeg_bytes()).unwrap();
    let seeded_checksum = crate::download::file::compute_sha256(&path).await.unwrap();
    let db = crate::state::SqliteStateDb::open_in_memory().unwrap();
    let record = crate::test_helpers::TestAssetRecord::new("CLEAR_FAIL")
        .filename("clear-failure.jpg")
        .metadata(AssetMetadata {
            rating: Some(3),
            metadata_hash: Some("fresh-hash".to_string()),
            ..AssetMetadata::default()
        })
        .build();
    db.upsert_seen(&record).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "CLEAR_FAIL",
        "original",
        &path,
        &seeded_checksum,
        None,
    )
    .await
    .unwrap();
    db.record_metadata_write_failure("PrimarySync", "CLEAR_FAIL", "original")
        .await
        .unwrap();
    db.fail_metadata_marker_clear_for_test();

    let residual = crate::download::drain_pending_metadata_rewrites(
        &db,
        &MetadataConfig {
            set_exif_rating: true,
            embed_xmp: true,
            ..MetadataConfig::default()
        },
        CaptureTimestampRepair::Preserve,
        &["PrimarySync"],
        Arc::from(".meta-tmp"),
        &CancellationToken::new(),
    )
    .await;

    assert_eq!(residual, 1);
    assert_eq!(db.get_pending_metadata_rewrites(10).await.unwrap().len(), 1);
}

/// #707 review: the drain must not re-hash a file it did not write. A file
/// that already drifted from its recorded checksum is damage `verify` and
/// `reconcile` must keep reporting, so the rewrite is refused outright.
#[cfg(feature = "xmp")]
#[tokio::test]
async fn drain_refuses_to_rewrite_a_file_that_drifted_from_its_checksum() {
    let dir = tempfile::tempdir().unwrap();
    let (db, path, recorded) = seed_marked_jpeg(dir.path(), "DRIFTED", Some(3)).await;
    // Trailing bytes after EOI keep the container readable, so the writer
    // would still accept the file if the drain offered it.
    let mut drifted_bytes = minimal_jpeg_bytes();
    drifted_bytes.push(0x00);
    std::fs::write(&path, &drifted_bytes).unwrap();

    let pass = run_pending_page(
        &db,
        embedded_rating_flags() | MetadataFlags::XMP_SIDECAR,
        CaptureTimestampRepair::Preserve,
        Arc::from(".meta-tmp"),
        &CancellationToken::new(),
        None,
        0,
    )
    .await;

    assert_eq!(
        std::fs::read(&path).unwrap(),
        drifted_bytes,
        "a file that does not match its checksum must not be rewritten"
    );
    let mut sidecar = path.clone().into_os_string();
    sidecar.push(".xmp");
    assert!(
        PathBuf::from(sidecar).exists(),
        "the sidecar is a separate file, so the export still runs"
    );
    assert_eq!(
        pass.failed, 1,
        "a refused rewrite is unfinished work, not a clean pass"
    );
    let (local, download) = stored_checksums(&db).await;
    assert_eq!(
        local.as_deref(),
        Some(recorded.as_str()),
        "the recorded checksum is the evidence of damage and must survive"
    );
    assert_eq!(download, None, "a refused rewrite records no download hash");
    assert_eq!(
        db.get_pending_metadata_rewrites(10).await.unwrap().len(),
        1,
        "the marker stays because the metadata never reached the file"
    );
}

/// #707 review: an embedded rewrite changes the media, so the row must
/// carry the new hash and keep the pre-rewrite hash as the provider
/// download checksum.
#[cfg(feature = "xmp")]
#[tokio::test]
async fn drain_records_the_rewritten_media_checksum() {
    let dir = tempfile::tempdir().unwrap();
    let (db, path, recorded) = seed_marked_jpeg(dir.path(), "REWRITTEN", Some(3)).await;

    run_pending(
        &db,
        embedded_rating_flags(),
        Arc::from(".meta-tmp"),
        &CancellationToken::new(),
    )
    .await;

    let on_disk = crate::download::file::compute_sha256(&path).await.unwrap();
    assert_ne!(
        on_disk, recorded,
        "precondition: the rewrite must change the media bytes"
    );
    let (local, download) = stored_checksums(&db).await;
    assert_eq!(local.as_deref(), Some(on_disk.as_str()));
    assert_eq!(download.as_deref(), Some(recorded.as_str()));
    assert!(
        db.get_pending_metadata_rewrites(10)
            .await
            .unwrap()
            .is_empty(),
        "a complete rewrite retires its marker"
    );
}

/// #707 review: the checksum is stored before the marker retires, so a
/// failure in between leaves a rewritten file the next pass can still
/// recognise as its own.
#[cfg(feature = "xmp")]
#[tokio::test]
async fn drain_keeps_the_marker_when_the_rewritten_checksum_cannot_be_stored() {
    let dir = tempfile::tempdir().unwrap();
    let (db, path, recorded) = seed_marked_jpeg(dir.path(), "CKFAIL", Some(3)).await;
    db.fail_metadata_checksum_write_for_test();

    run_pending(
        &db,
        embedded_rating_flags(),
        Arc::from(".meta-tmp"),
        &CancellationToken::new(),
    )
    .await;

    let on_disk = crate::download::file::compute_sha256(&path).await.unwrap();
    assert_ne!(on_disk, recorded, "precondition: the media was rewritten");
    assert_eq!(
        db.get_pending_metadata_rewrites(10).await.unwrap().len(),
        1,
        "the marker must survive so the rewrite is retried"
    );
    let (local, _) = stored_checksums(&db).await;
    assert_eq!(
        local, None,
        "a hash kei could not confirm must read as unknown, not as the              pre-rewrite value the next pass would treat as damage"
    );

    // A later pass, once the state write works again, must be able to
    // finish the job rather than refuse its own rewrite forever.
    let db = crate::state::SqliteStateDb::open(db.path()).await.unwrap();
    run_pending(
        &db,
        embedded_rating_flags(),
        Arc::from(".meta-tmp"),
        &CancellationToken::new(),
    )
    .await;

    let healed = crate::download::file::compute_sha256(&path).await.unwrap();
    let (local, _) = stored_checksums(&db).await;
    assert_eq!(
        local.as_deref(),
        Some(healed.as_str()),
        "the retry must record the bytes on disk"
    );
    assert!(
        db.get_pending_metadata_rewrites(10)
            .await
            .unwrap()
            .is_empty(),
        "the retry must retire the marker"
    );
}

/// #707 review: a sidecar-only rewrite leaves the media untouched, so it
/// must not restate either checksum.
#[cfg(feature = "xmp")]
#[tokio::test]
async fn drain_leaves_checksums_untouched_for_a_sidecar_only_rewrite() {
    let dir = tempfile::tempdir().unwrap();
    let (db, path, recorded) = seed_marked_jpeg(dir.path(), "SIDECAR", Some(3)).await;
    let before = std::fs::read(&path).unwrap();

    run_pending(
        &db,
        MetadataFlags::XMP_SIDECAR,
        Arc::from(".meta-tmp"),
        &CancellationToken::new(),
    )
    .await;

    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "a sidecar write must not touch the media"
    );
    let (local, download) = stored_checksums(&db).await;
    assert_eq!(local.as_deref(), Some(recorded.as_str()));
    assert_eq!(
        download, None,
        "no media rewrite happened, so no pre-rewrite hash is established"
    );
    assert!(
        db.get_pending_metadata_rewrites(10)
            .await
            .unwrap()
            .is_empty(),
        "the sidecar rewrite completed, so its marker retires"
    );
}

/// #682: a metadata-only drain must remove source fields that a prior kei
/// sidecar write established, then retire the marker only after that
/// updated sidecar lands.
#[cfg(feature = "xmp")]
#[tokio::test]
async fn drain_clears_previously_owned_sidecar_fields() {
    use crate::state::SqliteStateDb;
    use crate::state::types::AssetMetadata;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("SIDECAR_CLEAR.jpg");
    std::fs::write(&path, minimal_jpeg_bytes()).unwrap();
    let checksum = crate::download::file::compute_sha256(&path).await.unwrap();
    let db = SqliteStateDb::open_in_memory().unwrap();

    let initial = crate::test_helpers::TestAssetRecord::new("SIDECAR_CLEAR")
        .filename("SIDECAR_CLEAR.jpg")
        .metadata(AssetMetadata {
            rating: Some(5),
            description: Some("Old description".into()),
            metadata_hash: Some("initial-hash".into()),
            ..AssetMetadata::default()
        })
        .build();
    db.upsert_seen(&initial).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "SIDECAR_CLEAR",
        "original",
        &path,
        &checksum,
        None,
    )
    .await
    .unwrap();
    db.record_metadata_write_failure("PrimarySync", "SIDECAR_CLEAR", "original")
        .await
        .unwrap();

    run_pending(
        &db,
        MetadataFlags::XMP_SIDECAR,
        Arc::from(".meta-tmp"),
        &CancellationToken::new(),
    )
    .await;
    let sidecar_path = dir.path().join("SIDECAR_CLEAR.jpg.xmp");
    let initial_sidecar = std::fs::read_to_string(&sidecar_path).unwrap();
    assert!(initial_sidecar.contains("Rating"));
    assert!(initial_sidecar.contains("Old description"));

    let cleared = crate::test_helpers::TestAssetRecord::new("SIDECAR_CLEAR")
        .filename("SIDECAR_CLEAR.jpg")
        .metadata(AssetMetadata {
            metadata_hash: Some("cleared-hash".into()),
            ..AssetMetadata::default()
        })
        .build();
    db.upsert_seen(&cleared).await.unwrap();
    db.record_metadata_write_failure("PrimarySync", "SIDECAR_CLEAR", "original")
        .await
        .unwrap();

    let pass = run_pending(
        &db,
        MetadataFlags::XMP_SIDECAR,
        Arc::from(".meta-tmp"),
        &CancellationToken::new(),
    )
    .await;

    assert_eq!(pass.applied, 1);
    assert_eq!(pass.failed, 0);
    assert!(
        db.get_pending_metadata_rewrites(10)
            .await
            .unwrap()
            .is_empty(),
        "the marker retires after the cleared sidecar lands"
    );
    let cleared_sidecar = std::fs::read_to_string(&sidecar_path).unwrap();
    assert!(!cleared_sidecar.contains("Old description"));
    assert!(
        !cleared_sidecar.contains("xmp:Rating"),
        "the old rating and its ownership marker must both be gone"
    );
}

/// #707 review: a legacy row carries no checksum, so there is nothing to
/// verify against and nothing to protect. The rewrite proceeds and
/// establishes the baseline.
#[cfg(feature = "xmp")]
#[tokio::test]
async fn drain_establishes_a_checksum_baseline_when_none_was_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let (db, path, _recorded) = seed_marked_jpeg(dir.path(), "LEGACY", Some(3)).await;
    db.clear_local_checksum_for_test("PrimarySync", "LEGACY", "original");

    run_pending(
        &db,
        embedded_rating_flags(),
        Arc::from(".meta-tmp"),
        &CancellationToken::new(),
    )
    .await;

    let on_disk = crate::download::file::compute_sha256(&path).await.unwrap();
    let (local, download) = stored_checksums(&db).await;
    assert_eq!(
        local.as_deref(),
        Some(on_disk.as_str()),
        "the rewrite establishes the checksum a legacy row never had"
    );
    assert_eq!(
        download, None,
        "the pre-rewrite bytes were never verified, so kei cannot claim              them as the provider download"
    );
    assert!(
        db.get_pending_metadata_rewrites(10)
            .await
            .unwrap()
            .is_empty(),
        "a legacy row must not be stranded behind a missing checksum"
    );
    let (counts, _) = crate::commands::reconcile::scan_local_drift(
        &db,
        |_: &crate::commands::reconcile::LocalDriftAsset| {},
        |_: &str| {},
    )
    .await
    .unwrap();
    assert_eq!(
        counts.damaged, 1,
        "a legacy file short of its provider size must still read as damaged"
    );
}

/// #707 review: the rewritten hash is stored before the marker retires, so
/// a failure later in the same attempt still leaves the row describing the
/// bytes on disk. Without that, the next pass would read its own rewrite as
/// damage and refuse to touch it again.
#[cfg(feature = "xmp")]
#[tokio::test]
async fn drain_records_the_rewritten_checksum_even_when_the_sidecar_fails() {
    let dir = tempfile::tempdir().unwrap();
    let (db, path, recorded) = seed_marked_jpeg(dir.path(), "SIDEFAIL", Some(3)).await;

    // A directory where the sidecar belongs fails the sidecar write while
    // leaving the embedded write free to change the media.
    let mut sidecar = path.clone().into_os_string();
    sidecar.push(".xmp");
    std::fs::create_dir(PathBuf::from(sidecar)).unwrap();

    run_pending(
        &db,
        embedded_rating_flags() | MetadataFlags::XMP_SIDECAR,
        Arc::from(".meta-tmp"),
        &CancellationToken::new(),
    )
    .await;

    let on_disk = crate::download::file::compute_sha256(&path).await.unwrap();
    assert_ne!(
        on_disk, recorded,
        "precondition: the embedded write must land before the sidecar fails"
    );
    let (local, download) = stored_checksums(&db).await;
    assert_eq!(
        local.as_deref(),
        Some(on_disk.as_str()),
        "the row must describe the rewritten bytes even though the attempt failed"
    );
    assert_eq!(download.as_deref(), Some(recorded.as_str()));
    assert_eq!(
        db.get_pending_metadata_rewrites(10).await.unwrap().len(),
        1,
        "the sidecar still owes a write, so the marker stays"
    );
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn capture_queue_pagination_counts_retired_capture_debt() {
    use crate::config::MetadataConfig;
    use crate::download::DownloadStore;
    use crate::state::SqliteStateDb;
    use crate::state::types::AssetMetadata;

    let dir = tempfile::tempdir().unwrap();
    let seed_path = dir.path().join("seed.jpg");
    std::fs::write(&seed_path, minimal_jpeg_bytes()).unwrap();
    crate::download::metadata::apply_metadata(
        &seed_path,
        &crate::download::metadata::MetadataWrite {
            datetime: Some("2024:06:14 23:00:00".into()),
            offset_time_original: Some("+05:00".into()),
            ..crate::download::metadata::MetadataWrite::default()
        },
        ".seed-tmp",
    )
    .unwrap();
    let seed = std::fs::read(&seed_path).unwrap();
    let checksum = crate::download::file::compute_sha256(&seed_path)
        .await
        .unwrap();

    let db = SqliteStateDb::open_in_memory().unwrap();
    for index in 0..=METADATA_REWRITE_BATCH {
        let id = format!("A{index:04}");
        let path = dir.path().join(format!("{id}.jpg"));
        std::fs::write(&path, &seed).unwrap();
        let record = crate::test_helpers::TestAssetRecord::new(&id)
            .filename(&format!("{id}.jpg"))
            .checksum(&format!("provider-{id}"))
            .created_at(now_local().with_timezone(&chrono::Utc))
            .metadata(AssetMetadata {
                timezone_offset: Some(39_600),
                ..AssetMetadata::default()
            })
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded("PrimarySync", &id, "original", &path, &checksum, None)
            .await
            .unwrap();
        queue_capture_repair(&db, &record).await;
    }
    std::fs::create_dir(dir.path().join("A0000.jpg.xmp")).unwrap();

    let residual = crate::download::drain_pending_metadata_rewrites(
        &db as &dyn DownloadStore,
        &MetadataConfig {
            set_exif_datetime: true,
            xmp_sidecar: true,
            ..MetadataConfig::default()
        },
        CaptureTimestampRepair::ReplaceWithCaptureLocal,
        &["PrimarySync"],
        Arc::from(".metadata-test"),
        &CancellationToken::new(),
    )
    .await;

    assert_eq!(residual, 1);
    assert!(
        db.get_pending_metadata_rewrites_page_for_queue(
            MetadataRewriteQueue::CaptureRepair,
            None,
            0,
            METADATA_REWRITE_BATCH + 1,
        )
        .await
        .unwrap()
        .is_empty(),
        "every capture row, including the second page, must retire"
    );
    let ordinary = db
        .get_pending_metadata_rewrites(METADATA_REWRITE_BATCH + 1)
        .await
        .unwrap();
    assert_eq!(ordinary.len(), 1);
    assert_eq!(ordinary[0].id.as_ref(), "A0000");
    let last = crate::download::metadata::probe_exif(
        &dir.path().join(format!("A{METADATA_REWRITE_BATCH:04}.jpg")),
    )
    .unwrap();
    assert!(last.denotes_capture_time(&now_local()));
}

/// #707 review: a rewrite with nothing to write leaves the media alone, so
/// the row must not gain a checksum for bytes kei never wrote. Vouching for
/// an untouched file would hide damage that arrived some other way.
#[cfg(feature = "xmp")]
#[tokio::test]
async fn drain_writing_nothing_does_not_vouch_for_the_file() {
    let dir = tempfile::tempdir().unwrap();
    // No rating to write, and only the rating writer is enabled, so the
    // embedded plan comes out empty.
    let (db, path, recorded) = seed_marked_jpeg(dir.path(), "UNTOUCHED", None).await;
    let before = std::fs::read(&path).unwrap();

    run_pending(
        &db,
        MetadataFlags::RATING,
        Arc::from(".meta-tmp"),
        &CancellationToken::new(),
    )
    .await;

    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "precondition: an empty plan must leave the media alone"
    );
    let (local, download) = stored_checksums(&db).await;
    assert_eq!(local.as_deref(), Some(recorded.as_str()));
    assert_eq!(
        download, None,
        "kei must not claim a pre-rewrite hash for a file it did not rewrite"
    );
}

/// #718 taught reconcile that a metadata-rewritten file is legitimately
/// smaller than the provider size, proving it by hashing against
/// `local_checksum`. That proof needs both checksums, so a drained file
/// must not be reported as truncated damage.
#[cfg(feature = "xmp")]
#[tokio::test]
async fn drained_file_is_not_reported_as_truncated_damage() {
    let dir = tempfile::tempdir().unwrap();
    let (db, _path, _recorded) = seed_marked_jpeg(dir.path(), "SHRUNK", Some(3)).await;

    let (before, _) = stored_checksums(&db).await;
    let (counts, drift) = crate::commands::reconcile::scan_local_drift(
        &db,
        |_: &crate::commands::reconcile::LocalDriftAsset| {},
        |_: &str| {},
    )
    .await
    .unwrap();
    assert_eq!(
        counts.damaged, 1,
        "precondition: the provider size is larger than the file, so an \
         unexplained difference reads as damage"
    );
    assert_eq!(drift.len(), 1);

    run_pending(
        &db,
        embedded_rating_flags(),
        Arc::from(".meta-tmp"),
        &CancellationToken::new(),
    )
    .await;

    let (after, download) = stored_checksums(&db).await;
    assert_ne!(after, before, "precondition: the drain rewrote the media");
    assert!(
        download.is_some(),
        "reconcile needs the pre-rewrite hash to run its proof"
    );

    let (counts, drift) = crate::commands::reconcile::scan_local_drift(
        &db,
        |_: &crate::commands::reconcile::LocalDriftAsset| {},
        |_: &str| {},
    )
    .await
    .unwrap();
    assert_eq!(counts.present, 1);
    assert_eq!(counts.damaged, 0, "a drained file is intact, not truncated");
    assert!(drift.is_empty());
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn drain_reaches_newer_marker_after_retained_batch() {
    use crate::config::MetadataConfig;
    use crate::download::DownloadStore;
    use crate::state::SqliteStateDb;
    use crate::state::types::AssetMetadata;

    let dir = tempfile::tempdir().unwrap();
    let db = SqliteStateDb::open_in_memory().unwrap();
    let invalid_path = dir.path().join("invalid.jpg");
    std::fs::write(&invalid_path, b"not a jpeg").unwrap();
    let invalid_checksum = crate::download::file::compute_sha256(&invalid_path)
        .await
        .unwrap();
    for i in 0..METADATA_REWRITE_BATCH {
        let id = format!("A{i:04}");
        let metadata = AssetMetadata {
            rating: Some(3),
            metadata_hash: Some(format!("h{i}")),
            ..AssetMetadata::default()
        };
        let record = crate::test_helpers::TestAssetRecord::new(&id)
            .filename(&format!("{id}.jpg"))
            .metadata(metadata)
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            &id,
            "original",
            &invalid_path,
            &invalid_checksum,
            None,
        )
        .await
        .unwrap();
        db.record_metadata_write_failure("PrimarySync", &id, "original")
            .await
            .unwrap();
    }
    let valid_path = dir.path().join("valid.jpg");
    std::fs::write(&valid_path, minimal_jpeg_bytes()).unwrap();
    let valid_checksum = crate::download::file::compute_sha256(&valid_path)
        .await
        .unwrap();
    let valid = crate::test_helpers::TestAssetRecord::new("Z_VALID")
        .filename("valid.jpg")
        .metadata(AssetMetadata {
            rating: Some(3),
            metadata_hash: Some("valid-hash".to_string()),
            ..AssetMetadata::default()
        })
        .build();
    db.upsert_seen(&valid).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "Z_VALID",
        "original",
        &valid_path,
        &valid_checksum,
        None,
    )
    .await
    .unwrap();
    db.record_metadata_write_failure("PrimarySync", "Z_VALID", "original")
        .await
        .unwrap();

    let cfg = MetadataConfig {
        set_exif_rating: true,
        embed_xmp: true,
        ..MetadataConfig::default()
    };
    let token = CancellationToken::new();
    let residual = crate::download::drain_pending_metadata_rewrites(
        &db as &dyn DownloadStore,
        &cfg,
        CaptureTimestampRepair::Preserve,
        &["PrimarySync"],
        std::sync::Arc::from(".meta-tmp"),
        &token,
    )
    .await;
    assert_eq!(residual, METADATA_REWRITE_BATCH);
    let pending = db
        .get_pending_metadata_rewrites(METADATA_REWRITE_BATCH + 1)
        .await
        .unwrap();
    assert_eq!(pending.len(), METADATA_REWRITE_BATCH);
    assert!(
        pending.iter().all(|record| record.id.as_ref() != "Z_VALID"),
        "retained older markers must not prevent newer work from completing"
    );
}
