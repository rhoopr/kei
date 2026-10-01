use super::{MetadataWriteRequest, write_download_metadata};
use crate::download::filter::MetadataPayload;
use crate::download::metadata_rewrite::planning::{CaptureTimestampRepair, MetadataFlags};
#[cfg(feature = "xmp")]
use crate::download::metadata_rewrite::sidecar::write_sidecar_metadata;
#[cfg(feature = "xmp")]
use crate::download::metadata_rewrite::test_support::minimal_jpeg_bytes;
use crate::download::metadata_rewrite::test_support::now_local;
use std::sync::Arc;
#[cfg(feature = "xmp")]
use xmp_toolkit::{XmpMeta, xmp_ns};

#[tokio::test]
async fn contract_metadata_writes_require_opt_in_leaves_files_untouched() {
    let dir = tempfile::tempdir().expect("metadata temp dir");
    let photo_path = dir.path().join("photo.jpg");
    std::fs::write(&photo_path, b"original-media").expect("seed media");

    write_download_metadata(MetadataWriteRequest {
        final_path: &photo_path,
        embed_path: Some(&photo_path),
        expected_embed_fingerprint: None,
        source_checksum: None,
        sidecar_path: Some(&photo_path),
        payload: Arc::new(MetadataPayload::default()),
        created_local: now_local(),
        flags: MetadataFlags::default(),
        capture_timestamp_repair: CaptureTimestampRepair::Preserve,
        temp_suffix: ".metadata-test",
    })
    .await;

    assert_eq!(
        std::fs::read(&photo_path).expect("read media"),
        b"original-media",
        "disabled metadata options must not change media bytes"
    );
    let files = std::fs::read_dir(dir.path())
        .expect("read metadata temp dir")
        .count();
    assert_eq!(files, 1, "disabled metadata options created another file");
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn approved_heif_fingerprint_bypasses_changed_format_routing_gate() {
    let dir = tempfile::tempdir().unwrap();
    let photo_path = dir.path().join("extensionless-asset");
    let original = include_bytes!("../../../../tests/data/sample.heic");
    std::fs::write(&photo_path, original).unwrap();
    let approved = crate::download::file::fingerprint_regular_file(&photo_path)
        .await
        .unwrap();
    let changed = b"unsupported concurrent replacement";
    std::fs::write(&photo_path, changed).unwrap();

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

#[cfg(feature = "xmp")]
#[tokio::test]
async fn contract_xmp_gps_accuracy_requires_matching_location_new_writes() {
    use crate::test_helpers::ur64;
    use little_exif::exif_tag::ExifTag;
    use little_exif::filetype::FileExtension;
    use little_exif::metadata::Metadata;

    let source = crate::test_helpers::minimal_jpeg_with_source_gps_and_location();
    let dir = tempfile::tempdir().unwrap();
    // Even a one-ULP edit does not establish provenance. Invalid provider
    // evidence must not authorize accuracy, whether or not it is rendered.
    let provider_cases = [
        ("matching", Some(1.5), Some(2.5), true),
        ("edited-latitude", Some(12.3456), Some(2.5), false),
        ("edited-longitude", Some(1.5), Some(-78.9012), false),
        (
            "adjacent-float",
            Some(f64::from_bits(1.5_f64.to_bits() + 1)),
            Some(2.5),
            false,
        ),
        ("missing-latitude", None, Some(2.5), false),
        ("missing-longitude", Some(1.5), None, false),
        ("invalid-latitude", Some(91.0), Some(2.5), false),
        ("invalid-longitude", Some(1.5), Some(181.0), false),
        ("nan", Some(f64::NAN), Some(2.5), false),
        ("infinite", Some(1.5), Some(f64::INFINITY), false),
    ];
    for (name, latitude, longitude, expected) in provider_cases {
        let path = dir.path().join(format!("{name}.jpg"));
        std::fs::write(&path, &source).unwrap();
        let payload = Arc::new(MetadataPayload {
            latitude,
            longitude,
            ..MetadataPayload::default()
        });
        let checksum = crate::download::file::compute_sha256(&path).await.unwrap();
        assert!(
            write_sidecar_metadata(&path, payload, now_local(), Some(&checksum), ".gps-test").await
        );
        let xmp: XmpMeta = std::fs::read_to_string(path.with_extension("jpg.xmp"))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(
            xmp.contains_property(xmp_ns::EXIF, "GPSHPositioningError"),
            expected,
            "{name}"
        );
        if expected {
            assert_eq!(
                xmp.property(xmp_ns::EXIF, "GPSHPositioningError")
                    .unwrap()
                    .value,
                "3/2"
            );
        }
        assert_eq!(std::fs::read(&path).unwrap(), source, "{name}");
    }

    // Neither an absent legacy baseline nor a different recorded source
    // can certify matching coordinates in the current file.
    let path = dir.path().join("unproven.jpg");
    std::fs::write(&path, &source).unwrap();
    for checksum in [None, Some("different-source-checksum")] {
        assert!(
            write_sidecar_metadata(
                &path,
                Arc::new(MetadataPayload {
                    latitude: Some(1.5),
                    longitude: Some(2.5),
                    ..MetadataPayload::default()
                }),
                now_local(),
                checksum,
                ".gps-test"
            )
            .await
        );
        let xmp: XmpMeta = std::fs::read_to_string(path.with_extension("jpg.xmp"))
            .unwrap()
            .parse()
            .unwrap();
        assert!(!xmp.contains_property(xmp_ns::EXIF, "GPSHPositioningError"));
        assert_eq!(std::fs::read(&path).unwrap(), source);
    }

    // Remove a tag, or replace it with invalid readable EXIF. Both are
    // known absence, not retryable I/O failures.
    let native_cases = [
        (
            "missing-native-latitude",
            ExifTag::GPSLatitude(vec![]),
            None,
        ),
        (
            "missing-native-longitude",
            ExifTag::GPSLongitude(vec![]),
            None,
        ),
        (
            "missing-native-reference",
            ExifTag::GPSLatitudeRef(String::new()),
            None,
        ),
        (
            "missing-accuracy",
            ExifTag::GPSHPositioningError(vec![]),
            None,
        ),
        (
            "invalid-native-latitude",
            ExifTag::GPSLatitude(vec![]),
            Some(ExifTag::GPSLatitude(vec![
                ur64(91, 1),
                ur64(0, 1),
                ur64(0, 1),
            ])),
        ),
        (
            "invalid-native-longitude",
            ExifTag::GPSLongitude(vec![]),
            Some(ExifTag::GPSLongitude(vec![
                ur64(181, 1),
                ur64(0, 1),
                ur64(0, 1),
            ])),
        ),
        (
            "invalid-minutes",
            ExifTag::GPSLatitude(vec![]),
            Some(ExifTag::GPSLatitude(vec![
                ur64(0, 1),
                ur64(90, 1),
                ur64(0, 1),
            ])),
        ),
        (
            "invalid-seconds",
            ExifTag::GPSLatitude(vec![]),
            Some(ExifTag::GPSLatitude(vec![
                ur64(1, 1),
                ur64(29, 1),
                ur64(60, 1),
            ])),
        ),
        (
            "invalid-reference",
            ExifTag::GPSLatitudeRef(String::new()),
            Some(ExifTag::GPSLatitudeRef("E".into())),
        ),
        (
            "zero-coordinate-denominator",
            ExifTag::GPSLatitude(vec![]),
            Some(ExifTag::GPSLatitude(vec![
                ur64(1, 0),
                ur64(30, 1),
                ur64(0, 1),
            ])),
        ),
        (
            "zero-accuracy-denominator",
            ExifTag::GPSHPositioningError(vec![]),
            Some(ExifTag::GPSHPositioningError(vec![ur64(3, 0)])),
        ),
    ];
    for (name, removed, replacement) in native_cases {
        let mut metadata = Metadata::new_from_vec(&source, FileExtension::JPEG).unwrap();
        assert!(metadata.remove_tag(removed) > 0);
        if let Some(tag) = replacement {
            metadata.set_tag(tag);
        }
        let mut bytes = minimal_jpeg_bytes();
        metadata
            .write_to_vec(&mut bytes, FileExtension::JPEG)
            .unwrap();
        let path = dir.path().join(format!("{name}.jpg"));
        std::fs::write(&path, &bytes).unwrap();
        let checksum = crate::download::file::compute_sha256(&path).await.unwrap();
        // Out-of-range native values deliberately match the provider so
        // coordinate validation, not mere inequality, must reject them.
        let (latitude, longitude, rendered_latitude) = match name {
            "invalid-native-latitude" => (91.0, 2.5, "91,0.0000N"),
            "invalid-native-longitude" => (1.5, 181.0, "1,30.0000N"),
            _ => (1.5, 2.5, "1,30.0000N"),
        };
        assert!(
            write_sidecar_metadata(
                &path,
                Arc::new(MetadataPayload {
                    latitude: Some(latitude),
                    longitude: Some(longitude),
                    ..MetadataPayload::default()
                }),
                now_local(),
                Some(&checksum),
                ".gps-test"
            )
            .await,
            "{name}"
        );
        let xmp: XmpMeta = std::fs::read_to_string(path.with_extension("jpg.xmp"))
            .unwrap()
            .parse()
            .unwrap();
        assert!(
            !xmp.contains_property(xmp_ns::EXIF, "GPSHPositioningError"),
            "{name}"
        );
        assert_eq!(
            xmp.property(xmp_ns::EXIF, "GPSLatitude").unwrap().value,
            rendered_latitude,
            "{name}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes, "{name}");
    }
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn contract_xmp_gps_accuracy_requires_matching_location_after_embed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("source.jpg");
    std::fs::write(&path, crate::test_helpers::minimal_jpeg_with_source_gps()).unwrap();
    let checksum = crate::download::file::compute_sha256(&path).await.unwrap();
    let native = crate::download::metadata::read_source_gps(&path).unwrap();
    assert!(native.latitude.is_none());
    assert!(native.longitude.is_none());
    assert!(native.horizontal_positioning_error.is_some());
    // Match the download path: embed before publication, then write the
    // sidecar from the published media in a separate call.
    for (embed_path, sidecar_path) in [(Some(path.as_path()), None), (None, Some(path.as_path()))] {
        let outcome = write_download_metadata(MetadataWriteRequest {
            final_path: &path,
            embed_path,
            expected_embed_fingerprint: None,
            source_checksum: Some(&checksum),
            sidecar_path,
            payload: Arc::new(MetadataPayload {
                latitude: Some(1.5),
                longitude: Some(2.5),
                ..MetadataPayload::default()
            }),
            created_local: now_local(),
            flags: MetadataFlags::GPS | MetadataFlags::XMP_SIDECAR,
            capture_timestamp_repair: CaptureTimestampRepair::Preserve,
            temp_suffix: ".gps-test",
        })
        .await;
        assert!(!outcome.any_failed());
    }
    let rewritten = crate::download::metadata::read_source_gps(&path).unwrap();
    assert_eq!(rewritten.latitude, Some(1.5));
    assert_eq!(rewritten.longitude, Some(2.5));
    let xmp: XmpMeta = std::fs::read_to_string(path.with_extension("jpg.xmp"))
        .unwrap()
        .parse()
        .unwrap();
    assert!(!xmp.contains_property(xmp_ns::EXIF, "GPSHPositioningError"));
}
