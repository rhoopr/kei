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
async fn original_orientation_requires_byte_proven_unrendered_pixels_and_is_fill_only() {
    use crate::state::VersionSizeKey;
    use little_exif::{exif_tag::ExifTag, metadata::Metadata};
    use serde_json::json;

    for (extension, file_type, source) in [
        (
            "jpg",
            "public.jpeg",
            include_bytes!("../../../../tests/data/media/pattern.jpg").to_vec(),
        ),
        ("tif", "public.tiff", orientation_test_tiff()),
    ] {
        for scenario in [
            "original",
            "edited",
            "asset-resource",
            "unobserved",
            "existing",
            "unproven-bytes",
            "disabled",
            "invalid",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(format!("original.{extension}"));
            std::fs::write(&path, &source).unwrap();
            let mut native = Metadata::new_from_path(&path).unwrap_or_else(|_| Metadata::new());
            native.set_tag(ExifTag::ImageDescription("camera caption".into()));
            if extension == "tif" {
                let resolution = little_exif::rational::uR64 {
                    nominator: 72,
                    denominator: 1,
                };
                native.set_tag(ExifTag::XResolution(vec![resolution.clone()]));
                native.set_tag(ExifTag::YResolution(vec![resolution]));
                native.set_tag(ExifTag::ResolutionUnit(vec![2]));
            }
            if scenario == "existing" {
                native.set_tag(ExifTag::Orientation(vec![3]));
            }
            native.write_to_file(&path).unwrap();
            let before = std::fs::read(&path).unwrap();
            let pixels = orientation_test_pixels(&before, extension);
            let resource = json!({"value":{"size":before.len(),"fileChecksum":"original-resource","downloadURL":"https://example.invalid/original"}});
            let mut master = json!({"recordName":"MASTER", "fields":{
                "itemType":{"value":file_type}, "resOriginalFileType":{"value":file_type},
                "resOriginalRes":resource, "originalOrientation":{"value":6}
            }});
            let mut asset = json!({"recordName":"ASSET", "fields":{
                "orientation":{"value":1}, "resJPEGFullRes":resource,
                "resJPEGFullFileType":{"value":file_type}
            }});
            if scenario == "asset-resource" {
                asset["fields"]["resOriginalRes"] = resource;
            }
            if scenario == "unobserved" {
                master["fields"]
                    .as_object_mut()
                    .unwrap()
                    .remove("originalOrientation");
            }
            if scenario == "invalid" {
                master["fields"]["originalOrientation"]["value"] = json!(9);
            }
            let photo = crate::icloud::photos::PhotoAsset::new(master, asset);
            let mut config = crate::download::DownloadConfig::test_default();
            config.metadata.set_exif_orientation = scenario != "disabled";
            let version = if scenario == "edited" {
                VersionSizeKey::Adjusted
            } else {
                VersionSizeKey::Original
            };
            let payload = crate::download::filter::build_selected_payload(&photo, &config, version);
            let raw = crate::download::file::compute_sha256(&path).await.unwrap();
            let source_checksum = if scenario == "unproven-bytes" {
                None
            } else {
                Some(raw.as_str())
            };
            for attempt in 0..3 {
                let prior = std::fs::read(&path).unwrap();
                let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
                let fingerprint = crate::download::file::fingerprint_regular_file(&path)
                    .await
                    .unwrap();
                let result = write_download_metadata(MetadataWriteRequest {
                    final_path: &path,
                    embed_path: Some(&path),
                    expected_embed_fingerprint: Some(fingerprint),
                    source_checksum,
                    sidecar_path: None,
                    payload: Arc::clone(&payload),
                    created_local: now_local(),
                    flags: MetadataFlags::from(&config),
                    capture_timestamp_repair: CaptureTimestampRepair::Preserve,
                    temp_suffix: ".orientation-test",
                })
                .await;
                assert!(!result.any_failed(), "{extension}/{scenario}/{attempt}");
                let after = std::fs::read(&path).unwrap();
                assert_eq!(orientation_test_pixels(&after, extension), pixels);
                let native = Metadata::new_from_path(&path).unwrap();
                assert!(
                    matches!(native.get_tag(&ExifTag::ImageDescription(String::new())).next(),
                    Some(ExifTag::ImageDescription(value)) if value == "camera caption")
                );
                let orientation = native.get_tag(&ExifTag::Orientation(vec![])).next();
                match scenario {
                    "original" => assert!(
                        matches!(orientation, Some(ExifTag::Orientation(value)) if value == &[6]),
                        "{extension}: {orientation:?}"
                    ),
                    "existing" => assert!(
                        matches!(orientation, Some(ExifTag::Orientation(value)) if value == &[3])
                    ),
                    _ => assert!(orientation.is_none(), "{extension}/{scenario}"),
                }
                if attempt > 0 || scenario != "original" {
                    assert_eq!(after, prior, "{extension}/{scenario}/{attempt}");
                    assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), mtime);
                }
            }
        }
    }
}

fn orientation_test_tiff() -> Vec<u8> {
    let mut bytes = b"II*\0\x08\0\0\0".to_vec();
    let entries = [
        (256_u16, 4_u16, 1_u32, 2_u32),
        (257, 4, 1, 1),
        (258, 3, 3, 122),
        (259, 3, 1, 1),
        (262, 3, 1, 2),
        (273, 4, 1, 128),
        (277, 3, 1, 3),
        (278, 4, 1, 1),
        (279, 4, 1, 6),
    ];
    bytes.extend_from_slice(&9_u16.to_le_bytes());
    for (tag, kind, count, value) in entries {
        bytes.extend_from_slice(&tag.to_le_bytes());
        bytes.extend_from_slice(&kind.to_le_bytes());
        bytes.extend_from_slice(&count.to_le_bytes());
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes.extend_from_slice(&[0; 4]);
    bytes.extend_from_slice(&[8, 0, 8, 0, 8, 0, 10, 20, 30, 40, 50, 60]);
    bytes
}

#[cfg(feature = "xmp")]
#[test]
fn clearing_owned_xmp_preserves_identical_unowned_native_description() {
    use crate::download::metadata::{MetadataWrite, prepare_metadata_with_expected_fingerprint};
    use little_exif::{exif_tag::ExifTag, metadata::Metadata};
    use xmp_toolkit::{OpenFileOptions, XmpFile, xmp_ns};
    for (extension, bytes) in [
        (
            "jpg",
            include_bytes!("../../../../tests/data/media/pattern.jpg").to_vec(),
        ),
        ("tif", orientation_test_tiff()),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(format!("peer.{extension}"));
        std::fs::write(&path, bytes).unwrap();
        let write = |text: &str| MetadataWrite {
            description: Some(text.into()),
            ..Default::default()
        };
        prepare_metadata_with_expected_fingerprint(&path, &write("A"), ".peer", None)
            .unwrap()
            .publish(&path)
            .unwrap();
        let mut native = Metadata::new_from_path(&path).unwrap();
        native.set_tag(ExifTag::ImageDescription("B".into()));
        native.set_tag(ExifTag::Artist("camera artist".into()));
        if extension == "tif" {
            let resolution = little_exif::rational::uR64 {
                nominator: 72,
                denominator: 1,
            };
            native.set_tag(ExifTag::XResolution(vec![resolution.clone()]));
            native.set_tag(ExifTag::YResolution(vec![resolution]));
            native.set_tag(ExifTag::ResolutionUnit(vec![2]));
        }
        native.write_to_file(&path).unwrap();
        let pixels = orientation_test_pixels(&std::fs::read(&path).unwrap(), extension);
        let prepared =
            prepare_metadata_with_expected_fingerprint(&path, &write("B"), ".peer", None).unwrap();
        let fields = (*prepared.embedded_fields).clone();
        assert_eq!(fields.description.as_deref(), Some("B"));
        assert!(
            fields.native_description.is_none(),
            "identical native text is not owned"
        );
        prepared.publish(&path).unwrap();
        let clear = MetadataWrite {
            prior_embedded: fields.clone(),
            clear_embedded: fields,
            ..Default::default()
        };
        prepare_metadata_with_expected_fingerprint(&path, &clear, ".peer", None)
            .unwrap()
            .publish(&path)
            .unwrap();
        let native = Metadata::new_from_path(&path).unwrap();
        assert!(
            matches!(native.get_tag(&ExifTag::ImageDescription(String::new())).next(),Some(ExifTag::ImageDescription(value)) if value=="B")
        );
        assert!(
            matches!(native.get_tag(&ExifTag::Artist(String::new())).next(),Some(ExifTag::Artist(value)) if value=="camera artist")
        );
        let mut packet = XmpFile::new().unwrap();
        packet
            .open_file(&path, OpenFileOptions::default().use_packet_scanning())
            .unwrap();
        let meta = packet.xmp().unwrap();
        assert!(!meta.contains_property(xmp_ns::DC, "description"));
        assert_eq!(
            orientation_test_pixels(&std::fs::read(&path).unwrap(), extension),
            pixels
        );
    }
}

#[test]
fn embedded_field_evidence_preserves_identical_native_companions_and_pixels() {
    use crate::download::metadata::{GpsCoords, MetadataWrite};
    use little_exif::{exif_tag::ExifTag, ifd::ExifTagGroup, metadata::Metadata};
    for (extension, bytes) in [
        (
            "jpg",
            include_bytes!("../../../../tests/data/media/pattern.jpg").to_vec(),
        ),
        ("tif", orientation_test_tiff()),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(format!("proof.{extension}"));
        std::fs::write(&path, bytes).unwrap();
        let mut camera = Metadata::new_from_path(&path).unwrap_or_else(|_| Metadata::new());
        camera.set_tag(ExifTag::Artist("camera artist".into()));
        camera.set_tag(ExifTag::GPSLatitudeRef("N".into()));
        camera.set_tag(ExifTag::UnknownINT16U(
            vec![5],
            0x4746,
            ExifTagGroup::GENERIC,
        ));
        if extension == "tif" {
            let resolution = little_exif::rational::uR64 {
                nominator: 72,
                denominator: 1,
            };
            camera.set_tag(ExifTag::XResolution(vec![resolution.clone()]));
            camera.set_tag(ExifTag::YResolution(vec![resolution]));
            camera.set_tag(ExifTag::ResolutionUnit(vec![2]));
        }
        camera.write_to_file(&path).unwrap();
        let pixels = orientation_test_pixels(&std::fs::read(&path).unwrap(), extension);
        let write = MetadataWrite {
            rating: Some(5),
            description: Some("Kei description".into()),
            title: Some("Kei title".into()),
            keywords: vec!["Kei keyword".into()],
            people: vec!["Kei person".into()],
            gps: Some(GpsCoords {
                latitude: 1.5,
                longitude: 2.5,
                altitude: Some(10.0),
            }),
            ..Default::default()
        };
        let prepared = crate::download::metadata::prepare_metadata_with_expected_fingerprint(
            &path, &write, ".fields", None,
        )
        .unwrap();
        let fields = (*prepared.embedded_fields).clone();
        prepared.publish(&path).unwrap();
        assert_eq!(fields.description.as_deref(), Some("Kei description"));
        #[cfg(not(feature = "xmp"))]
        {
            assert!(fields.rating.is_none());
            assert!(fields.rating_percent.is_some());
            assert!(fields.gps[3].is_none());
        }
        let clear = MetadataWrite {
            prior_embedded: fields.clone(),
            clear_embedded: fields,
            ..Default::default()
        };
        let prepared = crate::download::metadata::prepare_metadata_with_expected_fingerprint(
            &path, &clear, ".fields", None,
        )
        .unwrap();
        let remaining = (*prepared.embedded_fields).clone();
        prepared.publish(&path).unwrap();
        assert_eq!(remaining, Default::default());
        let native = Metadata::new_from_path(&path).unwrap();
        assert!(
            matches!(native.get_tag(&ExifTag::Artist(String::new())).next(),Some(ExifTag::Artist(value)) if value=="camera artist")
        );
        assert!(
            matches!(native.get_tag(&ExifTag::GPSLatitudeRef(String::new())).next(),Some(ExifTag::GPSLatitudeRef(value)) if value=="N"),
            "{extension}: camera reference must survive"
        );
        assert!(
            native
                .get_tag(&ExifTag::UnknownINT16U(
                    vec![],
                    0x4746,
                    ExifTagGroup::GENERIC
                ))
                .next()
                .is_some()
        );
        assert!(
            native
                .get_tag(&ExifTag::ImageDescription(String::new()))
                .next()
                .is_none()
        );
        assert_eq!(
            orientation_test_pixels(&std::fs::read(&path).unwrap(), extension),
            pixels
        );
    }
}

// Independent container checks retain compressed JPEG scans and TIFF strip bytes.
fn orientation_test_pixels(bytes: &[u8], extension: &str) -> Vec<u8> {
    if extension == "jpg" {
        let mut output = Vec::new();
        let mut offset = 2;
        while offset < bytes.len() {
            assert_eq!(bytes[offset], 0xff);
            let marker = bytes[offset + 1];
            if marker == 0xda || marker == 0xd9 {
                output.extend_from_slice(&bytes[offset..]);
                break;
            }
            let end = offset
                + 2
                + usize::from(u16::from_be_bytes([bytes[offset + 2], bytes[offset + 3]]));
            if !(0xe0..=0xef).contains(&marker) && marker != 0xfe {
                output.extend_from_slice(&bytes[offset..end]);
            }
            offset = end;
        }
        return output;
    }
    assert_eq!(&bytes[..4], b"II*\0");
    let u32_at = |n| u32::from_le_bytes(bytes[n..n + 4].try_into().unwrap()) as usize;
    let ifd = u32_at(4);
    let count = usize::from(u16::from_le_bytes(bytes[ifd..ifd + 2].try_into().unwrap()));
    let value = |tag| {
        (0..count)
            .find_map(|n| {
                let at = ifd + 2 + n * 12;
                (u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap()) == tag)
                    .then(|| u32_at(at + 8))
            })
            .unwrap()
    };
    let offset = value(273);
    let length = value(279);
    bytes[offset..offset + length].to_vec()
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn standard_fields_exclude_provider_data_and_require_grouping_opt_ins() {
    use crate::download::AssetGroupings;
    use xmp_toolkit::{OpenFileOptions, XmpFile};

    for include_albums in [false, true] {
        for include_people in [false, true] {
            let metadata = crate::config::MetadataConfig {
                set_exif_orientation: false,
                set_exif_datetime: true,
                set_exif_rating: true,
                set_exif_gps: true,
                set_exif_description: true,
                embed_xmp: true,
                xmp_sidecar: true,
                include_albums,
                include_people,
            };
            let flags = MetadataFlags::from(&metadata);
            assert_eq!(MetadataFlags::from_bits(flags.bits()), Some(flags));
            let mut groupings = AssetGroupings::default();
            groupings
                .albums
                .insert("asset".into(), vec!["Album".into()]);
            groupings
                .people
                .insert("asset".into(), vec!["Alice".into()]);
            let source = crate::state::AssetMetadata {
                title: Some("Title".into()),
                description: Some("Caption".into()),
                keywords: Some(r#"["source-tag"]"#.into()),
                rating: Some(5),
                latitude: Some(1.0),
                longitude: Some(2.0),
                timezone_offset: Some(39_600),
                is_hidden: true,
                is_archived: true,
                media_subtype: Some("portrait".into()),
                burst_id: Some("internal-burst".into()),
                ..crate::state::AssetMetadata::default()
            };
            let payload = groupings.metadata_payload("asset", &source, flags);
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("photo.jpg");
            std::fs::write(&path, minimal_jpeg_bytes()).unwrap();
            let outcome = write_download_metadata(MetadataWriteRequest {
                final_path: &path,
                embed_path: Some(&path),
                expected_embed_fingerprint: None,
                source_checksum: None,
                sidecar_path: Some(&path),
                payload: Arc::new(payload),
                created_local: now_local(),
                flags,
                capture_timestamp_repair: CaptureTimestampRepair::Preserve,
                temp_suffix: ".metadata-test",
            })
            .await;
            assert!(!outcome.any_failed());
            let mut file = XmpFile::new().unwrap();
            file.open_file(&path, OpenFileOptions::default().for_read())
                .unwrap();
            let embedded = file.xmp().unwrap();
            let sidecar: XmpMeta = std::fs::read_to_string(path.with_extension("jpg.xmp"))
                .unwrap()
                .parse()
                .unwrap();
            for packet in [&embedded, &sidecar] {
                let keywords: Vec<_> = packet
                    .property_array(xmp_ns::DC, "subject")
                    .map(|v| v.value)
                    .collect();
                assert!(keywords.iter().any(|v| v == "source-tag"));
                assert_eq!(keywords.iter().any(|v| v == "Album"), include_albums);
                let people: Vec<_> = packet
                    .property_array(xmp_ns::IPTC_EXT, "PersonInImage")
                    .map(|v| v.value)
                    .collect();
                assert_eq!(
                    people,
                    if include_people {
                        vec!["Alice".to_owned()]
                    } else {
                        vec![]
                    }
                );
                assert!(packet.contains_property(xmp_ns::DC, "title"));
                assert!(packet.contains_property(xmp_ns::DC, "description"));
                assert!(packet.contains_property(xmp_ns::XMP, "Rating"));
                assert!(packet.contains_property(xmp_ns::EXIF, "GPSLatitude"));
                for field in ["hidden", "archived", "mediaSubtype", "burstId"] {
                    assert!(
                        !packet.contains_property("https://github.com/rhoopr/kei/ns/1.0/", field)
                    );
                }
            }
        }
    }
}

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
