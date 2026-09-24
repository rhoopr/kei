//! Shared fixtures for metadata rewrite owner tests.

#[cfg(feature = "xmp")]
use crate::download::filter::MetadataPayload;
use chrono::{DateTime, FixedOffset, TimeZone};

/// Capture-local time for an asset whose stored offset is +11:00, which is
/// what every payload below carries. Production derives this offset and
/// the written `OffsetTimeOriginal` from that same stored value, so the
/// two always agree.
pub(super) fn now_local() -> DateTime<FixedOffset> {
    FixedOffset::east_opt(39_600)
        .unwrap()
        .with_ymd_and_hms(2024, 6, 15, 10, 0, 0)
        .unwrap()
}

#[cfg(feature = "xmp")]
pub(super) fn rich_payload() -> MetadataPayload {
    MetadataPayload {
        timezone_offset: Some(39_600),
        rating: Some(4),
        latitude: Some(37.7),
        longitude: Some(-122.4),
        altitude: Some(10.0),
        title: Some("T".into()),
        description: Some("D".into()),
        keywords: vec!["vacation".into(), "beach".into()],
        people: vec!["Alice".into()],
        is_hidden: true,
        is_archived: true,
        media_subtype: Some("portrait".into()),
        burst_id: Some("b1".into()),
    }
}

/// Minimal valid JPEG (SOI + APP0 JFIF + EOI). XMP Toolkit can write
/// into this container; small enough to keep the test hermetic.
pub(super) fn minimal_jpeg_bytes() -> Vec<u8> {
    vec![
        0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00, 0x00,
        0x01, 0x00, 0x01, 0x00, 0x00, 0xFF, 0xD9,
    ]
}

/// Seeds one downloaded asset carrying a rewrite marker into `db`, running
/// the same sequence a real download failure leaves behind: `upsert_seen`,
/// `mark_downloaded`, then `record_metadata_write_failure`. The caller
/// supplies the on-disk path and its checksum so both readable media and
/// deliberately unreadable sources can be staged.
#[cfg(feature = "xmp")]
pub(super) async fn seed_downloaded_marker(
    db: &crate::state::SqliteStateDb,
    asset_id: &str,
    filename: &str,
    path: &std::path::Path,
    checksum: &str,
    metadata: crate::state::types::AssetMetadata,
    created_at: Option<chrono::DateTime<chrono::Utc>>,
) {
    let mut record = crate::test_helpers::TestAssetRecord::new(asset_id).filename(filename);
    if let Some(created_at) = created_at {
        record = record.created_at(created_at);
    }
    let record = record.metadata(metadata).build();
    db.upsert_seen(&record).await.unwrap();
    db.mark_downloaded("PrimarySync", asset_id, "original", path, checksum, None)
        .await
        .unwrap();
    db.record_metadata_write_failure("PrimarySync", asset_id, "original")
        .await
        .unwrap();
}
