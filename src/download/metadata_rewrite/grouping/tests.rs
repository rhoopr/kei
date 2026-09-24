#[cfg(feature = "xmp")]
use crate::download::metadata_rewrite::planning::MetadataFlags;
#[cfg(feature = "xmp")]
use crate::download::metadata_rewrite::queued::run_pending;
#[cfg(feature = "xmp")]
use crate::download::metadata_rewrite::test_support::minimal_jpeg_bytes;
#[cfg(feature = "xmp")]
use std::sync::Arc;
#[cfg(feature = "xmp")]
use tokio_util::sync::CancellationToken;

#[cfg(feature = "xmp")]
#[tokio::test]
async fn grouping_read_failure_keeps_marker_without_writing() {
    use crate::state::SqliteStateDb;
    use crate::state::types::AssetMetadata;

    let dir = tempfile::tempdir().unwrap();
    let photo_path = dir.path().join("grouping_read_failure.jpg");
    let before = minimal_jpeg_bytes();
    std::fs::write(&photo_path, &before).unwrap();
    let checksum = crate::download::file::compute_sha256(&photo_path)
        .await
        .unwrap();
    let record = crate::test_helpers::TestAssetRecord::new("GROUPING_READ_FAILURE")
        .filename("grouping_read_failure.jpg")
        .metadata(AssetMetadata {
            keywords: Some(r#"["beach"]"#.into()),
            ..AssetMetadata::default()
        })
        .build();
    let db = SqliteStateDb::open_in_memory().unwrap();
    db.upsert_seen(&record).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "GROUPING_READ_FAILURE",
        "original",
        &photo_path,
        &checksum,
        None,
    )
    .await
    .unwrap();
    db.record_metadata_write_failure("PrimarySync", "GROUPING_READ_FAILURE", "original")
        .await
        .unwrap();
    db.acquire_lock("break grouping read")
        .unwrap()
        .execute("DROP TABLE asset_people", [])
        .unwrap();

    let pass = run_pending(
        &db,
        MetadataFlags::EMBED_XMP | MetadataFlags::XMP_SIDECAR,
        Arc::from(".meta-tmp"),
        &CancellationToken::new(),
    )
    .await;

    assert_eq!(pass.failed, 1);
    assert_eq!(std::fs::read(&photo_path).unwrap(), before);
    assert!(!dir.path().join("grouping_read_failure.jpg.xmp").exists());
    assert_eq!(db.get_pending_metadata_rewrites(1).await.unwrap().len(), 1);
}
