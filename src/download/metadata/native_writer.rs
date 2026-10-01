//! Native JPEG and TIFF EXIF preparation without XMP Toolkit.

use super::formats::native_file_type;
use super::prepared::{
    PreparedMetadataFile, TmpGuard, create_unique_embed_temp, fingerprint_bytes,
};
use super::values::MetadataWrite;
use anyhow::{Context, Result};
use little_exif::exif_tag::ExifTag;
use little_exif::ifd::ExifTagGroup;
use little_exif::metadata::Metadata;
use little_exif::rational::uR64;
use std::path::Path;

pub(super) fn prepare_metadata_native(
    path: &Path,
    write: &MetadataWrite,
    temp_suffix: &str,
    expected_fingerprint: Option<crate::download::file::ExistingFileFingerprint>,
) -> Result<PreparedMetadataFile> {
    let input = std::fs::read(path)
        .with_context(|| format!("Could not read {} for native EXIF update", path.display()))?;
    let expected = fingerprint_bytes(&input)?;
    if expected_fingerprint.is_some_and(|approved| approved != expected) {
        return Err(
            crate::download::file::ConditionalPublishTargetChanged::AfterPlanning {
                path: path.to_path_buf(),
            }
            .into(),
        );
    }
    let source_permissions = std::fs::metadata(path)
        .with_context(|| format!("Could not inspect permissions of {}", path.display()))?
        .permissions();
    let Some(file_type) = native_file_type(&input, path) else {
        tracing::debug!(target: "kei::download::metadata",
            path = %path.display(),
            "Native EXIF writer supports JPEG/TIFF only; skipping metadata write"
        );
        anyhow::bail!(
            "Native EXIF writer does not support embedded metadata in {}",
            path.display()
        );
    };

    let mut metadata = match Metadata::new_from_vec(&input, file_type) {
        Ok(metadata) => metadata,
        Err(e) if e.to_string().contains("No EXIF data found") => Metadata::new(),
        Err(e) => {
            return Err(e).with_context(|| format!("Could not read EXIF from {}", path.display()));
        }
    };
    if let Some(dt) = &write.datetime {
        metadata.set_tag(ExifTag::DateTimeOriginal(dt.clone()));
        metadata.set_tag(ExifTag::CreateDate(dt.clone()));
        metadata.set_tag(ExifTag::ModifyDate(dt.clone()));
    }
    if write.clear_datetime_offsets {
        metadata.remove_tag(ExifTag::OffsetTimeOriginal(String::new()));
        metadata.remove_tag(ExifTag::OffsetTimeDigitized(String::new()));
        metadata.remove_tag(ExifTag::OffsetTime(String::new()));
    }
    if let Some(offset) = &write.offset_time_original {
        metadata.set_tag(ExifTag::OffsetTimeOriginal(offset.clone()));
        if write.datetime.is_some() {
            // CreateDate and ModifyDate carry their own offset tags. They
            // share this offset only when this pass wrote all three
            // timestamps from the same capture-local instant.
            metadata.set_tag(ExifTag::OffsetTimeDigitized(offset.clone()));
            metadata.set_tag(ExifTag::OffsetTime(offset.clone()));
        }
    }
    if let Some(desc) = &write.description {
        metadata.set_tag(ExifTag::ImageDescription(desc.clone()));
    }
    if let Some(gps) = write.gps {
        metadata.set_tag(ExifTag::GPSLatitudeRef(if gps.latitude >= 0.0 {
            "N".to_string()
        } else {
            "S".to_string()
        }));
        metadata.set_tag(ExifTag::GPSLatitude(dms_rational(gps.latitude)));
        metadata.set_tag(ExifTag::GPSLongitudeRef(if gps.longitude >= 0.0 {
            "E".to_string()
        } else {
            "W".to_string()
        }));
        metadata.set_tag(ExifTag::GPSLongitude(dms_rational(gps.longitude)));
        if let Some(alt) = gps.altitude {
            metadata.set_tag(ExifTag::GPSAltitudeRef(vec![u8::from(alt < 0.0)]));
            metadata.set_tag(ExifTag::GPSAltitude(vec![uR64::from(alt.abs())]));
        }
    }
    if let Some(rating) = write.rating {
        let rating = u16::from(rating.min(5));
        metadata.set_tag(ExifTag::UnknownINT16U(
            vec![rating],
            WINDOWS_RATING_TAG,
            ExifTagGroup::GENERIC,
        ));
        metadata.set_tag(ExifTag::UnknownINT16U(
            vec![windows_rating_percent(rating)],
            WINDOWS_RATING_PERCENT_TAG,
            ExifTagGroup::GENERIC,
        ));
    }

    let mut output = input;
    metadata
        .write_to_vec(&mut output, file_type)
        .with_context(|| format!("Could not write native EXIF into {}", path.display()))?;
    let output_fingerprint = fingerprint_bytes(&output)?;
    let (file, tmp_path) = create_unique_embed_temp(path, temp_suffix)?;
    let cleanup_permissions = file
        .metadata()
        .with_context(|| format!("Could not inspect permissions of {}", tmp_path.display()))?
        .permissions();
    let guard = TmpGuard::with_cleanup_permissions(&tmp_path, cleanup_permissions);
    let mut writer = std::io::BufWriter::new(file);
    std::io::Write::write_all(&mut writer, &output).with_context(|| {
        format!(
            "Could not write native EXIF temp file {}",
            tmp_path.display()
        )
    })?;
    let file = writer
        .into_inner()
        .with_context(|| format!("Could not flush {}", tmp_path.display()))?;
    file.set_permissions(source_permissions)
        .with_context(|| format!("Could not preserve permissions on {}", tmp_path.display()))?;
    file.sync_all()
        .with_context(|| format!("Could not fsync {}", tmp_path.display()))?;
    drop(file);
    Ok(PreparedMetadataFile {
        guard,
        expected_input: expected,
        expected_output: output_fingerprint,
    })
}

fn dms_rational(decimal: f64) -> Vec<uR64> {
    let abs = decimal.abs();
    let degrees = abs.floor();
    let minutes_full = (abs - degrees) * 60.0;
    let minutes = minutes_full.floor();
    let seconds = (minutes_full - minutes) * 60.0;
    vec![
        uR64::from(degrees),
        uR64::from(minutes),
        uR64::from(seconds),
    ]
}

const WINDOWS_RATING_TAG: u16 = 0x4746;

const WINDOWS_RATING_PERCENT_TAG: u16 = 0x4749;

const fn windows_rating_percent(rating: u16) -> u16 {
    match rating {
        0 => 0,
        1 => 1,
        2 => 25,
        3 => 50,
        4 => 75,
        _ => 99,
    }
}

#[cfg(test)]
mod native_tests {
    #[cfg(test)]
    use super::super::embedded::apply_metadata;
    use super::super::prepared::publish_prepared_embed;
    use super::super::probe::probe_exif;
    use super::super::test_support::fresh_jpeg;
    use super::super::values::{GpsCoords, MetadataWrite};
    use super::{WINDOWS_RATING_PERCENT_TAG, WINDOWS_RATING_TAG, prepare_metadata_native};
    use little_exif::exif_tag::ExifTag;
    use little_exif::ifd::ExifTagGroup;
    use little_exif::metadata::Metadata;
    use std::fs;

    fn first_unknown_int16(metadata: &Metadata, tag_id: u16) -> Option<u16> {
        metadata
            .get_tag(&ExifTag::UnknownINT16U(
                Vec::new(),
                tag_id,
                ExifTagGroup::GENERIC,
            ))
            .next()
            .and_then(|tag| match tag {
                ExifTag::UnknownINT16U(values, tag, _) if *tag == tag_id => values.first().copied(),
                _ => None,
            })
    }

    #[test]
    fn native_apply_metadata_writes_jpeg_exif_without_xmp_feature() {
        let dir = tempfile::tempdir().unwrap();
        let path = fresh_jpeg(dir.path(), "native.jpg");

        apply_metadata(
            &path,
            &MetadataWrite {
                datetime: Some("2024:06:15 10:00:00".to_string()),
                description: Some("Beach day".to_string()),
                gps: Some(GpsCoords {
                    latitude: 37.7749,
                    longitude: -122.4194,
                    altitude: Some(17.0),
                }),
                rating: Some(5),
                ..MetadataWrite::default()
            },
            ".kei-tmp",
        )
        .unwrap();

        let probe = probe_exif(&path).unwrap();
        assert_eq!(
            probe.datetime_original.as_deref(),
            Some("2024:06:15 10:00:00")
        );
        assert_eq!(probe.offset_time_original, None);
        assert!(probe.has_gps);

        let metadata = Metadata::new_from_path(&path).unwrap();
        let description = metadata
            .get_tag(&ExifTag::ImageDescription(String::new()))
            .next()
            .and_then(|tag| match tag {
                ExifTag::ImageDescription(s) => Some(s.as_str()),
                _ => None,
            });
        assert_eq!(description, Some("Beach day"));

        assert_eq!(first_unknown_int16(&metadata, WINDOWS_RATING_TAG), Some(5));
        assert_eq!(
            first_unknown_int16(&metadata, WINDOWS_RATING_PERCENT_TAG),
            Some(99)
        );
    }

    #[test]
    fn native_metadata_rewrite_refuses_concurrent_target_edit() {
        let dir = tempfile::tempdir().unwrap();
        let path = fresh_jpeg(dir.path(), "native-concurrent.jpg");
        let external = b"external native edit";

        let prepared = prepare_metadata_native(
            &path,
            &MetadataWrite {
                datetime: Some("2024:06:15 10:00:00".to_string()),
                ..MetadataWrite::default()
            },
            ".kei-tmp",
            None,
        )
        .unwrap();
        let result = prepared.publish_with(&path, |src, dst, expected, expected_replacement| {
            fs::write(dst, external)?;
            publish_prepared_embed(src, dst, expected, expected_replacement)
        });

        let error = result.expect_err("a concurrent edit must block native EXIF publication");
        assert!(crate::download::file::classify_conditional_publish_error(&error).target_changed);
        assert_eq!(fs::read(&path).unwrap(), external);
    }

    #[test]
    fn native_write_pairs_every_datetime_with_its_offset() {
        let dir = tempfile::tempdir().unwrap();
        let path = fresh_jpeg(dir.path(), "native-offset.jpg");

        apply_metadata(
            &path,
            &MetadataWrite {
                datetime: Some("2026:02:01 09:31:59".to_string()),
                offset_time_original: Some("+11:00".to_string()),
                ..MetadataWrite::default()
            },
            ".kei-tmp",
        )
        .unwrap();

        let probe = probe_exif(&path).unwrap();
        assert_eq!(probe.offset_time_original.as_deref(), Some("+11:00"));
        assert!(probe.has_other_datetime_offset);

        let metadata = Metadata::new_from_path(&path).unwrap();
        for template in [
            ExifTag::OffsetTimeOriginal(String::new()),
            ExifTag::OffsetTimeDigitized(String::new()),
            ExifTag::OffsetTime(String::new()),
        ] {
            let written = metadata
                .get_tag(&template)
                .next()
                .and_then(|tag| match tag {
                    ExifTag::OffsetTimeOriginal(s)
                    | ExifTag::OffsetTimeDigitized(s)
                    | ExifTag::OffsetTime(s) => Some(s.as_str()),
                    _ => None,
                });
            assert_eq!(
                written,
                Some("+11:00"),
                "EXIF tag 0x{:04x} must carry the capture offset",
                template.as_u16()
            );
        }

        // Without a timestamp write, CreateDate and ModifyDate keep whatever
        // the file already held, so only the verified DateTimeOriginal gets an
        // offset.
        let lone = fresh_jpeg(dir.path(), "native-lone-offset.jpg");
        apply_metadata(
            &lone,
            &MetadataWrite {
                offset_time_original: Some("+11:00".to_string()),
                ..MetadataWrite::default()
            },
            ".kei-tmp",
        )
        .unwrap();

        let metadata = Metadata::new_from_path(&lone).unwrap();
        assert!(
            metadata
                .get_tag(&ExifTag::OffsetTimeOriginal(String::new()))
                .next()
                .is_some()
        );
        for template in [
            ExifTag::OffsetTimeDigitized(String::new()),
            ExifTag::OffsetTime(String::new()),
        ] {
            assert!(
                metadata.get_tag(&template).next().is_none(),
                "EXIF tag 0x{:04x} must not claim an offset for a timestamp this pass did not write",
                template.as_u16()
            );
        }
    }

    /// A camera can leave offset tags behind without the capture timestamp
    /// they qualified. Writing a resolved timestamp over that state has to
    /// drop them first, or the file ends up claiming a zone that describes
    /// nothing in it.
    #[test]
    fn native_write_clears_offsets_orphaned_from_a_replaced_timestamp() {
        let dir = tempfile::tempdir().unwrap();
        let path = fresh_jpeg(dir.path(), "native-orphaned-offset.jpg");

        let mut seed = Metadata::new();
        seed.set_tag(ExifTag::OffsetTimeOriginal("+02:00".to_string()));
        seed.set_tag(ExifTag::OffsetTimeDigitized("+02:00".to_string()));
        seed.set_tag(ExifTag::OffsetTime("+02:00".to_string()));
        seed.write_to_file(&path).unwrap();

        let seeded = probe_exif(&path).unwrap();
        assert_eq!(seeded.datetime_original, None);
        assert!(seeded.has_any_datetime_offset());

        apply_metadata(
            &path,
            &MetadataWrite {
                datetime: Some("2026:02:01 09:31:59".to_string()),
                clear_datetime_offsets: true,
                ..MetadataWrite::default()
            },
            ".kei-tmp",
        )
        .unwrap();

        let probe = probe_exif(&path).unwrap();
        assert_eq!(
            probe.datetime_original.as_deref(),
            Some("2026:02:01 09:31:59")
        );
        assert!(
            !probe.has_any_datetime_offset(),
            "a stale offset must not survive to qualify the replacement timestamp"
        );

        let metadata = Metadata::new_from_path(&path).unwrap();
        for template in [
            ExifTag::OffsetTimeOriginal(String::new()),
            ExifTag::OffsetTimeDigitized(String::new()),
            ExifTag::OffsetTime(String::new()),
        ] {
            assert!(
                metadata.get_tag(&template).next().is_none(),
                "EXIF tag 0x{:04x} must be cleared",
                template.as_u16()
            );
        }
    }

    #[test]
    fn native_apply_metadata_uses_configured_temp_suffix() {
        let dir = tempfile::tempdir().unwrap();
        let path = fresh_jpeg(dir.path(), "native_suffix.jpg");
        let default_tmp = dir.path().join("native_suffix.jpg.meta-tmp");
        let configured_tmp = dir.path().join("native_suffix.jpg.kei-tmp");
        fs::write(&default_tmp, b"sentinel").unwrap();

        apply_metadata(
            &path,
            &MetadataWrite {
                datetime: Some("2024:06:15 10:00:00".to_string()),
                ..MetadataWrite::default()
            },
            ".kei-tmp",
        )
        .unwrap();

        assert_eq!(
            fs::read(&default_tmp).unwrap(),
            b"sentinel",
            "native metadata rewrite must not use the old .meta-tmp suffix"
        );
        assert!(
            !configured_tmp.exists(),
            "configured native metadata temp path must be installed or cleaned up"
        );
    }

    #[test]
    fn native_apply_metadata_skips_heic_without_xmp_feature() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image.heic");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0x18_u32.to_be_bytes());
        bytes.extend_from_slice(b"ftyp");
        bytes.extend_from_slice(b"heic");
        bytes.extend_from_slice(&0_u32.to_be_bytes());
        bytes.extend_from_slice(b"heic");
        bytes.extend_from_slice(b"mif1");
        fs::write(&path, &bytes).unwrap();

        apply_metadata(
            &path,
            &MetadataWrite {
                datetime: Some("2024:06:15 10:00:00".to_string()),
                ..MetadataWrite::default()
            },
            ".kei-tmp",
        )
        .unwrap();

        assert_eq!(
            fs::read(&path).unwrap(),
            bytes,
            "HEIC should be skipped unchanged when XMP support is disabled"
        );
        assert!(!dir.path().join("image.heic.kei-tmp").exists());
    }
}
