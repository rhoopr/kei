use super::write_reconciled_sidecar;
#[cfg(feature = "xmp")]
use super::{plan_sidecar_write, write_reconciled_sidecar_with_reader, write_sidecar_metadata};
#[cfg(feature = "xmp")]
use crate::download::filter::MetadataPayload;
#[cfg(feature = "xmp")]
use crate::download::metadata_rewrite::immediate::{MetadataWriteRequest, write_download_metadata};
#[cfg(feature = "xmp")]
use crate::download::metadata_rewrite::planning::{CaptureTimestampRepair, MetadataFlags};
#[cfg(feature = "xmp")]
use crate::download::metadata_rewrite::queued::run_pending;
#[cfg(feature = "xmp")]
use crate::download::metadata_rewrite::test_support::seed_downloaded_marker;
use crate::download::metadata_rewrite::test_support::{
    minimal_jpeg_bytes, now_local, rich_payload,
};
#[cfg(feature = "xmp")]
use chrono::{DateTime, FixedOffset, TimeZone};
use std::sync::Arc;
#[cfg(feature = "xmp")]
use tokio_util::sync::CancellationToken;
#[cfg(feature = "xmp")]
use xmp_toolkit::{XmpMeta, xmp_ns};

#[cfg(feature = "xmp")]
fn assert_cloudkit_authority_with_source_gps(meta: &XmpMeta) {
    const CAPTURE: &str = "2019-03-04T05:06:07+11:00";
    for (namespace, property) in [
        (xmp_ns::XMP, "CreateDate"),
        (xmp_ns::XMP, "ModifyDate"),
        (xmp_ns::EXIF, "DateTimeOriginal"),
        (xmp_ns::PHOTOSHOP, "DateCreated"),
    ] {
        assert_eq!(
            meta.property(namespace, property).expect(property).value,
            CAPTURE
        );
    }
    let coordinate = |name: &str| meta.property(xmp_ns::EXIF, name).expect(name).value;
    assert_eq!(coordinate("GPSLatitude"), "12,20.7360N");
    assert_eq!(coordinate("GPSLongitude"), "78,54.0720W");
    assert_eq!(coordinate("GPSAltitude"), CLOUDKIT_ALTITUDE_XMP);
    crate::test_helpers::assert_source_gps_in_xmp(meta);
    assert_ne!(
        meta.property(xmp_ns::EXIF, "GPSTimeStamp")
            .expect("GPSTimeStamp")
            .value,
        CAPTURE
    );
}

#[cfg(feature = "xmp")]
const CLOUDKIT_ALTITUDE_XMP: &str = "2032686204/219746927";

#[cfg(feature = "xmp")]
const CLOUDKIT_ALTITUDE: f64 = 9.250_123_456_789;

#[cfg(feature = "xmp")]
fn authority_cloudkit_time() -> DateTime<FixedOffset> {
    FixedOffset::east_opt(39_600)
        .unwrap()
        .with_ymd_and_hms(2019, 3, 4, 5, 6, 7)
        .unwrap()
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn reconciled_sidecar_source_read_failure_can_retry_without_conflicting_output() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source.jpg");
    let destination = root.path().join("destination.jpg");
    let sidecar = root.path().join("destination.jpg.xmp");
    let bytes = crate::test_helpers::minimal_jpeg_with_source_gps();
    std::fs::write(&source, &bytes).unwrap();
    let copy = crate::download::file::copy_local_file_no_replace(
        root.path(),
        &source,
        &destination,
        ".part",
    )
    .await
    .unwrap()
    .unwrap();
    let payload = Arc::new(rich_payload());
    let failed = write_reconciled_sidecar_with_reader(
        Arc::clone(&copy),
        Arc::clone(&payload),
        now_local(),
        ".part".to_owned(),
        |_, _| Err(std::io::Error::other("injected source GPS read failure").into()),
    )
    .await;
    assert!(failed.is_err());
    assert!(!sidecar.exists());
    assert_eq!(std::fs::read(&source).unwrap(), bytes);
    let completed = write_reconciled_sidecar(
        Arc::clone(&copy),
        Arc::clone(&payload),
        now_local(),
        ".part".to_owned(),
    )
    .await
    .unwrap();
    completed.validate().await.unwrap();
    let packet = std::fs::read_to_string(&sidecar).unwrap();
    let xmp: XmpMeta = packet.parse().unwrap();
    assert_eq!(xmp.property(xmp_ns::XMP, "Rating").unwrap().value, "4");
    crate::test_helpers::assert_source_gps_in_xmp(&xmp);
    let repeated = write_reconciled_sidecar(copy, payload, now_local(), ".part".to_owned())
        .await
        .unwrap();
    repeated.validate().await.unwrap();
    assert_eq!(std::fs::read_to_string(&sidecar).unwrap(), packet);
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 3);
}

#[cfg(all(feature = "xmp", unix))]
#[tokio::test]
async fn reconciled_sidecar_rejects_unsafe_metadata_entries() {
    use std::os::unix::fs::symlink;
    for source_link in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let source = root.path().join("source.jpg");
        let destination = root.path().join("destination.jpg");
        let external = outside.path().join("user.xmp");
        std::fs::write(&source, minimal_jpeg_bytes()).unwrap();
        std::fs::write(&external, b"external custom packet").unwrap();
        let link = root.path().join(if source_link {
            "source.jpg.xmp"
        } else {
            "destination.jpg.xmp"
        });
        symlink(&external, &link).unwrap();
        let copy = crate::download::file::copy_local_file_no_replace(
            root.path(),
            &source,
            &destination,
            ".part",
        )
        .await
        .unwrap()
        .unwrap();
        let result = write_reconciled_sidecar(
            copy,
            Arc::new(rich_payload()),
            now_local(),
            ".part".to_owned(),
        )
        .await;
        assert!(result.is_err());
        assert_eq!(std::fs::read_link(&link).unwrap(), external);
        assert_eq!(std::fs::read(&external).unwrap(), b"external custom packet");
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 1);
    }
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let source = root.path().join("source.jpg");
    let parent = root.path().join("parent");
    let destination = parent.join("destination.jpg");
    std::fs::write(&source, minimal_jpeg_bytes()).unwrap();
    let copy = crate::download::file::copy_local_file_no_replace(
        root.path(),
        &source,
        &destination,
        ".part",
    )
    .await
    .unwrap()
    .unwrap();
    std::fs::rename(&parent, root.path().join("retained")).unwrap();
    symlink(outside.path(), &parent).unwrap();
    assert!(
        write_reconciled_sidecar(
            copy,
            Arc::new(rich_payload()),
            now_local(),
            ".part".to_owned()
        )
        .await
        .is_err()
    );
    assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
}

#[cfg(feature = "xmp")]
#[test]
fn plan_sidecar_write_is_comprehensive_regardless_of_flags() {
    let payload = rich_payload();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("photo.jpg");
    std::fs::write(&path, minimal_jpeg_bytes()).unwrap();
    let created_local = now_local() + chrono::Duration::milliseconds(629);
    let (w, source_error) = plan_sidecar_write(&path, &payload, &created_local, None);
    assert!(source_error.is_none());
    // Every payload field should land in the sidecar write, no flag gating.
    assert_eq!(w.datetime.as_deref(), Some("2024-06-15T10:00:00.629"));
    assert_eq!(w.offset_time_original.as_deref(), Some("+11:00"));
    assert_eq!(w.rating, Some(4));
    assert!(w.gps.is_some());
    assert_eq!(w.title.as_deref(), Some("T"));
    assert_eq!(w.description.as_deref(), Some("D"));
    assert_eq!(w.keywords.len(), 2);
    assert_eq!(w.people, vec!["Alice"]);
    assert!(w.is_hidden);
    assert!(w.is_archived);
    assert_eq!(w.media_subtype.as_deref(), Some("portrait"));
    assert_eq!(w.burst_id.as_deref(), Some("b1"));
}

#[cfg(feature = "xmp")]
#[test]
fn plan_sidecar_write_empty_payload_yields_datetime_only() {
    // datetime comes from the local clock; the rest stays empty.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("photo.jpg");
    std::fs::write(&path, minimal_jpeg_bytes()).unwrap();
    let (w, source_error) =
        plan_sidecar_write(&path, &MetadataPayload::default(), &now_local(), None);
    assert!(source_error.is_none());
    assert!(w.datetime.is_some());
    assert!(w.gps_datetime.is_none());
    assert!(w.gps_speed.is_none());
    assert!(w.gps_speed_ref.is_none());
    assert!(w.gps_h_positioning_error.is_none());
    assert!(w.rating.is_none());
    assert!(w.gps.is_none());
    assert!(w.title.is_none());
    assert!(w.keywords.is_empty());
    assert!(!w.is_hidden);
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn contract_xmp_gps_accuracy_requires_matching_location_native_boundaries() {
    use crate::test_helpers::ur64;
    use little_exif::exif_tag::ExifTag;

    let dir = tempfile::tempdir().unwrap();
    for (name, latitude, longitude, lat_ref, lon_ref) in [
        ("south-west", 1, 2, "S", "W"),
        ("north-east-limits", 90, 180, "N", "E"),
        ("south-west-limits", 90, 180, "S", "W"),
        ("zero", 0, 0, "N", "E"),
    ] {
        let mut metadata = crate::test_helpers::exif_with_source_gps();
        metadata.set_tag(ExifTag::GPSLatitude(vec![
            ur64(latitude, 1),
            ur64(0, 1),
            ur64(0, 1),
        ]));
        metadata.set_tag(ExifTag::GPSLongitude(vec![
            ur64(longitude, 1),
            ur64(0, 1),
            ur64(0, 1),
        ]));
        metadata.set_tag(ExifTag::GPSLatitudeRef(lat_ref.into()));
        metadata.set_tag(ExifTag::GPSLongitudeRef(lon_ref.into()));
        metadata.set_tag(ExifTag::GPSHPositioningError(vec![ur64(0, 1)]));
        let tiff = metadata.encode().unwrap();
        let heic = crate::download::heif::apple_tmap_insertion_heic(&tiff, b"");
        for (extension, bytes) in [("dng", &tiff), ("heic", &heic)] {
            let path = dir.path().join(format!("{name}.{extension}"));
            std::fs::write(&path, bytes).unwrap();
            let payload = Arc::new(MetadataPayload {
                latitude: Some(f64::from(latitude) * if lat_ref == "S" { -1.0 } else { 1.0 }),
                longitude: Some(f64::from(longitude) * if lon_ref == "W" { -1.0 } else { 1.0 }),
                ..MetadataPayload::default()
            });
            let checksum = crate::download::file::compute_sha256(&path).await.unwrap();
            assert!(
                write_sidecar_metadata(&path, payload, now_local(), Some(&checksum), ".gps-test")
                    .await
            );
            let xmp: XmpMeta =
                std::fs::read_to_string(path.with_extension(format!("{extension}.xmp")))
                    .unwrap()
                    .parse()
                    .unwrap();
            assert_eq!(
                xmp.property(xmp_ns::EXIF, "GPSHPositioningError")
                    .unwrap()
                    .value,
                "0/1",
                "{name}.{extension}"
            );
            assert_eq!(std::fs::read(&path).unwrap(), *bytes);
        }
    }
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn sidecar_write_carries_source_gps_and_corrected_coordinates() {
    let dir = tempfile::tempdir().expect("metadata temp dir");
    let media_path = dir.path().join("source.jpg");
    let source_bytes = crate::test_helpers::minimal_jpeg_with_source_gps_and_location();
    std::fs::write(&media_path, &source_bytes).expect("write source media");
    let payload = MetadataPayload {
        timezone_offset: Some(39_600),
        latitude: Some(12.3456),
        longitude: Some(-78.9012),
        altitude: Some(CLOUDKIT_ALTITUDE),
        ..MetadataPayload::default()
    };
    let cloudkit_created = authority_cloudkit_time();
    let request = || MetadataWriteRequest {
        final_path: &media_path,
        embed_path: None,
        expected_embed_fingerprint: None,
        source_checksum: None,
        sidecar_path: Some(&media_path),
        payload: Arc::new(payload.clone()),
        created_local: cloudkit_created,
        flags: MetadataFlags::XMP_SIDECAR,
        capture_timestamp_repair: CaptureTimestampRepair::Preserve,
        temp_suffix: ".gps-sidecar-test",
    };

    let outcome = write_download_metadata(request()).await;
    assert!(!outcome.any_failed());
    let sidecar_path = media_path.with_file_name("source.jpg.xmp");
    let first = std::fs::read(&sidecar_path).expect("read generated sidecar");
    let xmp = String::from_utf8(first.clone())
        .expect("sidecar UTF-8")
        .parse::<XmpMeta>()
        .expect("parse generated sidecar");
    assert_cloudkit_authority_with_source_gps(&xmp);
    assert_eq!(
        std::fs::read(&media_path).expect("read media after sidecar"),
        source_bytes
    );

    let second_outcome = write_download_metadata(request()).await;
    assert!(!second_outcome.any_failed());
    assert_eq!(
        std::fs::read(&sidecar_path).expect("read repeated sidecar"),
        first,
        "repeating the same sidecar write must be idempotent"
    );

    use crate::state::{AssetMetadata, SqliteStateDb};
    let db = SqliteStateDb::open_in_memory().expect("metadata state DB");
    let checksum = crate::download::file::compute_sha256(&media_path)
        .await
        .expect("media checksum");
    seed_downloaded_marker(
        &db,
        "GPS_RETRY",
        "source.jpg",
        &media_path,
        &checksum,
        AssetMetadata {
            timezone_offset: Some(39_600),
            latitude: Some(12.3456),
            longitude: Some(-78.9012),
            altitude: Some(CLOUDKIT_ALTITUDE),
            metadata_hash: Some("gps-retry-hash".into()),
            ..AssetMetadata::default()
        },
        Some(cloudkit_created.with_timezone(&chrono::Utc)),
    )
    .await;
    run_pending(
        &db,
        MetadataFlags::XMP_SIDECAR,
        Arc::from(".gps-retry-test"),
        &CancellationToken::new(),
    )
    .await;
    assert!(
        db.get_pending_metadata_rewrites(10)
            .await
            .expect("read retry markers")
            .is_empty(),
        "successful sidecar retry must retire its marker"
    );
    let retry_xmp = std::fs::read_to_string(&sidecar_path)
        .expect("read sidecar after retry")
        .parse::<XmpMeta>()
        .expect("parse sidecar after retry");
    assert_cloudkit_authority_with_source_gps(&retry_xmp);
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn sidecar_write_reads_source_gps_from_dng_content() {
    for (millis, expected) in [(0, "2024-06-15T10:00:00"), (629, "2024-06-15T10:00:00.629")] {
        let dir = tempfile::tempdir().expect("metadata temp dir");
        let media_path = dir.path().join("source.DNG");
        let source_bytes = crate::test_helpers::minimal_tiff_with_source_gps();
        std::fs::write(&media_path, &source_bytes).expect("write source DNG");
        let payload = MetadataPayload {
            rating: Some(4),
            latitude: Some(12.3456),
            longitude: Some(-78.9012),
            altitude: Some(9.25),
            ..MetadataPayload::default()
        };

        let outcome = write_download_metadata(MetadataWriteRequest {
            final_path: &media_path,
            embed_path: None,
            expected_embed_fingerprint: None,
            source_checksum: None,
            sidecar_path: Some(&media_path),
            payload: Arc::new(payload),
            created_local: now_local() + chrono::Duration::milliseconds(millis),
            flags: MetadataFlags::XMP_SIDECAR,
            capture_timestamp_repair: CaptureTimestampRepair::Preserve,
            temp_suffix: ".gps-dng-test",
        })
        .await;

        assert!(!outcome.any_failed());
        let sidecar_path = media_path.with_file_name("source.DNG.xmp");
        let xmp = std::fs::read_to_string(&sidecar_path)
            .expect("read DNG sidecar")
            .parse::<XmpMeta>()
            .expect("parse DNG sidecar");
        crate::test_helpers::assert_source_gps_in_xmp(&xmp);
        let value = |namespace, name| xmp.property(namespace, name).expect(name).value;
        for (namespace, property) in [
            (xmp_ns::XMP, "CreateDate"),
            (xmp_ns::XMP, "ModifyDate"),
            (xmp_ns::EXIF, "DateTimeOriginal"),
            (xmp_ns::PHOTOSHOP, "DateCreated"),
        ] {
            assert_eq!(value(namespace, property), expected);
        }
        assert!(
            xmp.property("http://cipa.jp/exif/1.0/", "OffsetTimeOriginal")
                .is_none()
        );
        assert_eq!(value(xmp_ns::XMP, "Rating"), "4");
        assert_eq!(value(xmp_ns::EXIF, "GPSLatitude"), "12,20.7360N");
        assert_eq!(value(xmp_ns::EXIF, "GPSLongitude"), "78,54.0720W");
        assert_eq!(value(xmp_ns::EXIF, "GPSAltitude"), "37/4");
        assert_eq!(
            std::fs::read(&media_path).expect("read source DNG after sidecar"),
            source_bytes,
            "sidecar-only metadata must not alter DNG bytes"
        );
        assert!(
            !media_path
                .with_file_name("source.DNG.xmp.gps-dng-test")
                .exists(),
            "successful sidecar publication must remove its temporary file"
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
    }
}
