use little_exif::exif_tag::ExifTag;
use little_exif::metadata::Metadata;
use serde_json::{Value, json};

use crate::config::MetadataConfig;
use crate::download::DownloadOutcome;

use super::support::{Harness, files};
use super::{cycle, fixture, sha256};

fn provider_metadata(records: &mut [Value]) {
    let fields = &mut records[1]["fields"];
    fields["assetDate"] = json!({"value": 1_700_000_000_000_i64});
    fields["timeZoneOffset"] = json!({"value": 0});
    fields["locationLatitude"] = json!({"value": 1.0});
    fields["locationLongitude"] = json!({"value": -2.0});
    fields["extendedDescEnc"] = json!({"value": "Fixture description", "type": "STRING"});
    fields["captionEnc"] = json!({"value": "Café fixture", "type": "STRING"});
}

// Retain codec tables, frame headers and compressed scan, excluding only metadata.
fn jpeg_payload(bytes: &[u8]) -> Vec<u8> {
    assert_eq!(&bytes[..2], b"\xff\xd8");
    let mut output = Vec::new();
    let mut offset = 2;
    while offset < bytes.len() {
        assert_eq!(bytes[offset], 0xff);
        let marker = bytes[offset + 1];
        if marker == 0xda || marker == 0xd9 {
            output.extend_from_slice(&bytes[offset..]);
            break;
        }
        let length = usize::from(u16::from_be_bytes([bytes[offset + 2], bytes[offset + 3]]));
        let end = offset + 2 + length;
        if !(0xe0..=0xef).contains(&marker) && marker != 0xfe {
            output.extend_from_slice(&bytes[offset..end]);
        }
        offset = end;
    }
    output
}

#[tokio::test]
async fn bundled_metadata_roundtrips() {
    for populated in [true, false] {
        let mut h = Harness::new().await;
        h.config.metadata = MetadataConfig {
            set_exif_datetime: true,
            set_exif_rating: true,
            set_exif_gps: true,
            set_exif_description: true,
            #[cfg(feature = "xmp")]
            embed_xmp: false,
            #[cfg(feature = "xmp")]
            xmp_sidecar: false,
        };
        let mut records = h
            .asset(
                "metadata",
                "Metadata.jpg",
                "public.jpeg",
                "media/pattern.jpg",
                1,
            )
            .await;
        if populated {
            provider_metadata(&mut records);
        } else {
            records[1]["fields"]["isFavorite"] = json!({"value": 0});
            records[1]["fields"]["timeZoneOffset"] = json!({"value": 0});
        }
        let result = cycle(&h.config, records.clone()).await;
        assert!(
            matches!(result.outcome, DownloadOutcome::Success),
            "{result:?}"
        );
        assert_eq!(result.stats.downloaded, 1);
        let path = h.config.directory.join("Metadata.JPG");
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(
            jpeg_payload(&bytes),
            jpeg_payload(&fixture("media/pattern.jpg"))
        );
        let meta = Metadata::new_from_path(&path).unwrap();
        assert_eq!(
            meta.get_tag(&ExifTag::DateTimeOriginal(String::new()))
                .next(),
            Some(&ExifTag::DateTimeOriginal("2023:11:14 22:13:20".into()))
        );
        if populated {
            assert_eq!(
                meta.get_tag(&ExifTag::ImageDescription(String::new()))
                    .next(),
                Some(&ExifTag::ImageDescription("Fixture description".into()))
            );
            assert_eq!(
                meta.get_tag(&ExifTag::GPSLatitudeRef(String::new())).next(),
                Some(&ExifTag::GPSLatitudeRef("N".into()))
            );
            assert_eq!(
                meta.get_tag(&ExifTag::GPSLongitudeRef(String::new()))
                    .next(),
                Some(&ExifTag::GPSLongitudeRef("W".into()))
            );
            for (tag, degrees) in [
                (ExifTag::GPSLatitude(Vec::new()), 1.0),
                (ExifTag::GPSLongitude(Vec::new()), 2.0),
            ] {
                let values = match meta.get_tag(&tag).next().unwrap() {
                    ExifTag::GPSLatitude(values) | ExifTag::GPSLongitude(values) => values,
                    other => panic!("unexpected GPS tag: {other:?}"),
                };
                assert_eq!(
                    values.iter().cloned().map(f64::from).collect::<Vec<_>>(),
                    vec![degrees, 0.0, 0.0]
                );
            }
            #[cfg(feature = "xmp")]
            {
                let mut file = xmp_toolkit::XmpFile::new().unwrap();
                file.open_file(&path, xmp_toolkit::OpenFileOptions::default().for_read())
                    .unwrap();
                assert_eq!(
                    file.xmp()
                        .unwrap()
                        .property_i32(xmp_toolkit::xmp_ns::XMP, "Rating")
                        .unwrap()
                        .value,
                    5
                );
            }
            #[cfg(not(feature = "xmp"))]
            assert!(meta.get_ifd(little_exif::ifd::ExifTagGroup::GENERIC, 0).unwrap().get_tags().iter().any(|tag| matches!(tag, ExifTag::UnknownINT16U(values, 0x4746, _) if values == &[5])));
        } else {
            assert!(
                meta.get_tag(&ExifTag::GPSLatitude(Vec::new()))
                    .next()
                    .is_none()
            );
            assert!(
                meta.get_tag(&ExifTag::ImageDescription(String::new()))
                    .next()
                    .is_none()
            );
        }
        h.reopen().await;
        let row = h.db().get_downloaded_page(0, 10).await.unwrap().remove(0);
        assert_eq!(
            row.download_checksum.as_deref(),
            Some(sha256(&fixture("media/pattern.jpg")).as_str())
        );
        assert_eq!(row.local_checksum.as_deref(), Some(sha256(&bytes).as_str()));
        assert_ne!(row.local_checksum, row.download_checksum);
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        let result = cycle(&h.config, records).await;
        assert!(
            matches!(result.outcome, DownloadOutcome::Success),
            "{result:?}"
        );
        assert_eq!(result.stats.downloaded, 0);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            modified
        );
        assert_eq!(files(&h.config.directory), vec![path]);
        assert!(h.db().get_failed().await.unwrap().is_empty());
        assert!(h.db().get_pending().await.unwrap().is_empty());
        h.server.verify().await;
    }
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn bundled_sidecar_roundtrip() {
    use xmp_toolkit::{OpenFileOptions, XmpFile, XmpMeta, xmp_ns};
    for sidecar in [false, true] {
        let mut h = Harness::new().await;
        h.config.metadata.set_exif_datetime = !sidecar;
        h.config.metadata.set_exif_rating = !sidecar;
        h.config.metadata.set_exif_gps = !sidecar;
        h.config.metadata.set_exif_description = !sidecar;
        h.config.metadata.xmp_sidecar = sidecar;
        h.config.metadata.embed_xmp = !sidecar;
        let mut records = h
            .asset("xmp", "Packet.jpg", "public.jpeg", "media/pattern.jpg", 1)
            .await;
        provider_metadata(&mut records);
        let result = cycle(&h.config, records.clone()).await;
        assert!(
            matches!(result.outcome, DownloadOutcome::Success),
            "{result:?}"
        );
        assert_eq!(result.stats.downloaded, 1);
        let path = h.config.directory.join("Packet.JPG");
        let sidecar_path = h.config.directory.join("Packet.JPG.xmp");
        let meta = if sidecar {
            std::fs::read_to_string(&sidecar_path)
                .unwrap()
                .parse::<XmpMeta>()
                .unwrap()
        } else {
            let mut file = XmpFile::new().unwrap();
            file.open_file(&path, OpenFileOptions::default().for_read())
                .unwrap();
            file.xmp().unwrap()
        };
        assert_eq!(meta.property_i32(xmp_ns::XMP, "Rating").unwrap().value, 5);
        assert_eq!(
            meta.localized_text(xmp_ns::DC, "description", None, "x-default")
                .unwrap()
                .0
                .value,
            "Fixture description"
        );
        assert_eq!(
            meta.localized_text(xmp_ns::DC, "title", None, "x-default")
                .unwrap()
                .0
                .value,
            "Café fixture"
        );
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(
            jpeg_payload(&bytes),
            jpeg_payload(&fixture("media/pattern.jpg"))
        );
        if sidecar {
            assert_eq!(bytes, fixture("media/pattern.jpg"));
        }
        let snapshots: Vec<_> = files(&h.config.directory)
            .into_iter()
            .map(|p| {
                let bytes = std::fs::read(&p).unwrap();
                let modified = std::fs::metadata(&p).unwrap().modified().unwrap();
                (p, bytes, modified)
            })
            .collect();
        assert_eq!(snapshots.len(), if sidecar { 2 } else { 1 });
        h.reopen().await;
        let result = cycle(&h.config, records).await;
        assert!(
            matches!(result.outcome, DownloadOutcome::Success),
            "{result:?}"
        );
        assert_eq!(result.stats.downloaded, 0);
        for (path, bytes, modified) in &snapshots {
            assert_eq!(std::fs::read(path).unwrap(), *bytes);
            assert_eq!(
                std::fs::metadata(path).unwrap().modified().unwrap(),
                *modified
            );
        }
        assert_eq!(files(&h.config.directory).len(), snapshots.len());
        h.server.verify().await;
    }
}

#[tokio::test]
async fn bundled_existing_metadata_survives_rating_update() {
    let mut h = Harness::new().await;
    h.config.metadata.set_exif_rating = true;
    let records = h
        .asset(
            "existing-metadata",
            "Existing.jpg",
            "public.jpeg",
            "media/metadata.jpg",
            1,
        )
        .await;
    let original = fixture("media/metadata.jpg");
    let source_path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/media/metadata.jpg");
    let before = Metadata::new_from_path(&source_path).unwrap();
    let result = cycle(&h.config, records.clone()).await;
    assert!(
        matches!(result.outcome, DownloadOutcome::Success),
        "{result:?}"
    );
    let path = h.config.directory.join("Existing.JPG");
    let after = Metadata::new_from_path(&path).unwrap();
    for tag in [
        ExifTag::Orientation(Vec::new()),
        ExifTag::DateTimeOriginal(String::new()),
        ExifTag::GPSLatitude(Vec::new()),
        ExifTag::GPSLongitude(Vec::new()),
    ] {
        let original_tag = before.get_tag(&tag).next().expect("controlled source tag");
        assert_eq!(after.get_tag(&tag).next(), Some(original_tag));
    }
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(jpeg_payload(&original), jpeg_payload(&bytes));
    h.reopen().await;
    let row = h.db().get_downloaded_page(0, 10).await.unwrap().remove(0);
    assert_eq!(
        row.download_checksum.as_deref(),
        Some(sha256(&original).as_str())
    );
    assert_eq!(row.local_checksum.as_deref(), Some(sha256(&bytes).as_str()));
    let result = cycle(&h.config, records).await;
    assert!(
        matches!(result.outcome, DownloadOutcome::Success),
        "{result:?}"
    );
    assert_eq!(result.stats.downloaded, 0);
    assert!(std::fs::read(&path).unwrap() == bytes);
    assert_eq!(files(&h.config.directory), vec![path]);
    h.server.verify().await;
}
