use super::{EmbedWriteResult, write_embed_metadata};
use crate::download::filter::MetadataPayload;
#[cfg(feature = "xmp")]
use crate::download::metadata_rewrite::immediate::{MetadataWriteRequest, write_download_metadata};
use crate::download::metadata_rewrite::planning::{CaptureTimestampRepair, MetadataFlags};
use crate::download::metadata_rewrite::test_support::{minimal_jpeg_bytes, now_local};
use std::sync::Arc;

#[cfg(feature = "xmp")]
#[tokio::test]
async fn enabled_heif_embed_failure_preserves_invalid_media() {
    const FIXTURE_LEN: u64 = 1024 * 1024;
    let dir = tempfile::tempdir().expect("metadata temp dir");
    let photo_path = dir.path().join("large.heic");
    let mut file = std::fs::File::create(&photo_path).expect("create sparse HEIF fixture");
    std::io::Write::write_all(&mut file, b"\0\0\0\x18ftypheic\0\0\0\0heicmif1")
        .expect("write HEIF routing header");
    file.set_len(FIXTURE_LEN)
        .expect("extend sparse HEIF fixture");
    drop(file);

    let outcome = write_download_metadata(MetadataWriteRequest {
        final_path: &photo_path,
        embed_path: Some(&photo_path),
        expected_embed_fingerprint: None,
        source_checksum: None,
        sidecar_path: None,
        payload: Arc::new(MetadataPayload {
            rating: Some(5),
            ..MetadataPayload::default()
        }),
        created_local: now_local(),
        flags: MetadataFlags::RATING | MetadataFlags::EMBED_XMP,
        capture_timestamp_repair: CaptureTimestampRepair::Preserve,
        temp_suffix: ".metadata-test",
    })
    .await;

    assert!(outcome.any_failed());
    assert_eq!(
        std::fs::metadata(&photo_path)
            .expect("stat sparse HEIF fixture")
            .len(),
        FIXTURE_LEN,
        "a failed HEIF embed must leave the media length unchanged"
    );
    assert_eq!(
        std::fs::read(&photo_path)
            .expect("read sparse HEIF fixture")
            .get(..24),
        Some(b"\0\0\0\x18ftypheic\0\0\0\0heicmif1".as_slice()),
        "a failed HEIF embed must leave the routing header unchanged"
    );
    assert!(!dir.path().join("large.heic.metadata-test").exists());
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn heif_embed_reports_change_after_approved_retry_fingerprint() {
    let dir = tempfile::tempdir().unwrap();
    let photo_path = dir.path().join("changed-after-approval.heic");
    let original = include_bytes!("../../../../tests/data/sample.heic");
    std::fs::write(&photo_path, original).unwrap();
    let approved = crate::download::file::fingerprint_regular_file(&photo_path)
        .await
        .unwrap();
    let mut changed = original.to_vec();
    changed.push(0);
    std::fs::write(&photo_path, &changed).unwrap();

    let outcome = write_download_metadata(MetadataWriteRequest {
        final_path: &photo_path,
        embed_path: Some(&photo_path),
        expected_embed_fingerprint: Some(approved),
        source_checksum: None,
        sidecar_path: None,
        payload: Arc::new(MetadataPayload {
            rating: Some(5),
            ..MetadataPayload::default()
        }),
        created_local: now_local(),
        flags: MetadataFlags::RATING | MetadataFlags::EMBED_XMP,
        capture_timestamp_repair: CaptureTimestampRepair::Preserve,
        temp_suffix: ".metadata-test",
    })
    .await;

    assert!(outcome.any_failed());
    assert!(outcome.embed_input_changed);
    assert_eq!(std::fs::read(&photo_path).unwrap(), changed);
}

#[tokio::test]
async fn embed_path_preserves_unverified_host_local_timestamp() {
    let dir = tempfile::tempdir().unwrap();
    let photo_path = dir.path().join("host-local.jpg");
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
    let before = crate::download::metadata::probe_exif(&photo_path).unwrap();
    assert!(before.datetime_original.is_some());
    assert!(before.offset_time_original.is_none());

    assert!(matches!(
        write_embed_metadata(
            &photo_path,
            None,
            Arc::new(MetadataPayload {
                timezone_offset: Some(39_600),
                ..MetadataPayload::default()
            }),
            now_local(),
            MetadataFlags::DATETIME,
            CaptureTimestampRepair::Preserve,
            ".metadata-test",
        )
        .await,
        EmbedWriteResult::NoWrite
    ));

    let after = crate::download::metadata::probe_exif(&photo_path).unwrap();
    assert_eq!(after.datetime_original, before.datetime_original);
    assert!(after.offset_time_original.is_none());
}

#[tokio::test]
async fn embed_path_repairs_host_local_timestamp_and_offset_together() {
    let dir = tempfile::tempdir().unwrap();
    let photo_path = dir.path().join("host-local-repair.jpg");
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

    let created_local = now_local();
    assert!(matches!(
        write_embed_metadata(
            &photo_path,
            None,
            Arc::new(MetadataPayload {
                timezone_offset: Some(39_600),
                ..MetadataPayload::default()
            }),
            created_local,
            MetadataFlags::DATETIME,
            CaptureTimestampRepair::ReplaceWithCaptureLocal,
            ".metadata-test",
        )
        .await,
        EmbedWriteResult::Applied(Some(_))
    ));

    let after = crate::download::metadata::probe_exif(&photo_path).unwrap();
    assert!(after.denotes_capture_time(&created_local));
    assert_eq!(after.offset_time_original.as_deref(), Some("+11:00"));

    assert!(
        matches!(
            write_embed_metadata(
                &photo_path,
                None,
                Arc::new(MetadataPayload {
                    timezone_offset: Some(39_600),
                    ..MetadataPayload::default()
                }),
                created_local,
                MetadataFlags::DATETIME,
                CaptureTimestampRepair::ReplaceWithCaptureLocal,
                ".metadata-test",
            )
            .await,
            EmbedWriteResult::NoWrite
        ),
        "a repaired timestamp and offset must be idempotent"
    );
}

#[tokio::test]
async fn embed_path_replaces_orphaned_offset_before_writing_timestamp() {
    let dir = tempfile::tempdir().unwrap();
    let photo_path = dir.path().join("orphaned-offset.jpg");
    std::fs::write(&photo_path, minimal_jpeg_bytes()).unwrap();
    crate::download::metadata::apply_metadata(
        &photo_path,
        &crate::download::metadata::MetadataWrite {
            offset_time_original: Some("+10:00".into()),
            ..crate::download::metadata::MetadataWrite::default()
        },
        ".seed-tmp",
    )
    .unwrap();
    let before = crate::download::metadata::probe_exif(&photo_path).unwrap();
    assert!(before.datetime_original.is_none());
    assert_eq!(before.offset_time_original.as_deref(), Some("+10:00"));

    let created_local = now_local();
    assert!(matches!(
        write_embed_metadata(
            &photo_path,
            None,
            Arc::new(MetadataPayload {
                timezone_offset: Some(39_600),
                ..MetadataPayload::default()
            }),
            created_local,
            MetadataFlags::DATETIME,
            CaptureTimestampRepair::Preserve,
            ".metadata-test",
        )
        .await,
        EmbedWriteResult::Applied(Some(_))
    ));

    let after = crate::download::metadata::probe_exif(&photo_path).unwrap();
    assert!(after.denotes_capture_time(&created_local));
    assert_eq!(after.offset_time_original.as_deref(), Some("+11:00"));
}
