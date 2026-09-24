//! XMP Toolkit preparation for native format handlers.

use super::prepared::{PreparedMetadataFile, TmpGuard, create_unique_embed_temp};
use super::values::MetadataWrite;
use super::xmp_fields::{apply_to_xmp, ensure_initialized};
use anyhow::{Context, Result};
use std::path::Path;
use xmp_toolkit::{OpenFileOptions, XmpFile, XmpMeta};

pub(super) fn prepare_metadata_xmp_toolkit(
    path: &Path,
    write: &MetadataWrite,
    temp_suffix: &str,
    expected_fingerprint: Option<crate::download::file::ExistingFileFingerprint>,
) -> Result<PreparedMetadataFile> {
    ensure_initialized();

    let expected = crate::download::file::fingerprint_regular_file_snapshot_blocking(path)
        .with_context(|| {
            format!(
                "Could not fingerprint {} for metadata update",
                path.display()
            )
        })?
        .fingerprint;
    if expected_fingerprint.is_some_and(|approved| approved != expected) {
        return Err(
            crate::download::file::ConditionalPublishTargetChanged::AfterPlanning {
                path: path.to_path_buf(),
            }
            .into(),
        );
    }
    let (file, tmp_path) = create_unique_embed_temp(path, temp_suffix)?;
    let cleanup_permissions = file
        .metadata()
        .with_context(|| format!("Could not inspect permissions of {}", tmp_path.display()))?
        .permissions();
    let guard = TmpGuard::with_cleanup_permissions(&tmp_path, cleanup_permissions);
    drop(file);
    std::fs::copy(path, &tmp_path).with_context(|| {
        format!(
            "Could not copy {} to {}",
            path.display(),
            tmp_path.display()
        )
    })?;
    let copied = crate::download::file::fingerprint_regular_file_snapshot_blocking(&tmp_path)
        .with_context(|| format!("Could not fingerprint {}", tmp_path.display()))?
        .fingerprint;
    if copied != expected {
        return Err(
            crate::download::file::ConditionalPublishTargetChanged::AfterPlanning {
                path: path.to_path_buf(),
            }
            .into(),
        );
    }

    let result: Result<()> = (|| {
        let mut file = XmpFile::new().context("Could not create XMP handle")?;
        file.open_file(
            &tmp_path,
            OpenFileOptions::default().for_update().use_smart_handler(),
        )
        .with_context(|| format!("Could not open {} for XMP update", tmp_path.display()))?;

        let mut meta = file
            .xmp()
            .unwrap_or_else(|| XmpMeta::new().unwrap_or_default());
        apply_to_xmp(&mut meta, write)?;

        if !file.can_put_xmp(&meta) {
            anyhow::bail!(
                "The XMP format handler cannot write metadata to {}",
                tmp_path.display()
            );
        }
        file.put_xmp(&meta)
            .with_context(|| format!("Could not write XMP metadata into {}", tmp_path.display()))?;
        file.try_close()
            .with_context(|| format!("Could not close {} after XMP update", tmp_path.display()))?;
        Ok(())
    })();

    result?;
    let temp = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&tmp_path)
        .with_context(|| {
            format!(
                "Could not reopen {} after metadata update",
                tmp_path.display()
            )
        })?;
    temp.sync_all()
        .with_context(|| format!("Could not fsync {}", tmp_path.display()))?;
    drop(temp);
    let output = crate::download::file::fingerprint_regular_file_snapshot_blocking(&tmp_path)
        .with_context(|| format!("Could not fingerprint {}", tmp_path.display()))?
        .fingerprint;
    Ok(PreparedMetadataFile {
        guard,
        expected_input: expected,
        expected_output: output,
    })
}

#[cfg(test)]
#[allow(clippy::unused_result_ok, reason = "test cleanup is best-effort")]
mod tests {
    #[cfg(test)]
    use super::super::embedded::apply_metadata;
    use super::super::prepared::publish_prepared_embed;
    use super::super::probe::probe_exif;
    use super::super::test_support::{
        apply_metadata_with_default_suffix, embed_temp_entries, fresh_jpeg, minimal_jpeg,
        read_meta, test_tmp_dir,
    };
    use super::super::values::{GpsCoords, MetadataWrite};
    use super::super::xmp_fields::KEI_XMP_NS;
    use super::prepare_metadata_xmp_toolkit;
    use crate::test_helpers::ur64;
    use little_exif::exif_tag::ExifTag;
    use little_exif::metadata::Metadata;
    use std::fs;
    use std::sync::Arc;
    use xmp_toolkit::xmp_ns;

    #[test]
    fn apply_metadata_uses_configured_temp_suffix() {
        let dir = test_tmp_dir("meta_tests");
        let path = fresh_jpeg(&dir, "custom_suffix.jpg");
        let default_tmp = dir.join("custom_suffix.jpg.meta-tmp");
        let configured_tmp = dir.join("custom_suffix.jpg.kei-tmp");
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
            "metadata rewrite must not use the old .meta-tmp suffix"
        );
        assert!(
            !configured_tmp.exists(),
            "configured metadata temp path must be installed or cleaned up"
        );
        fs::remove_file(&default_tmp).ok();
        fs::remove_file(&path).ok();
    }

    #[test]
    fn apply_metadata_datetime_roundtrips() {
        let dir = test_tmp_dir("meta_tests");
        let path = fresh_jpeg(&dir, "dt.jpg");
        apply_metadata_with_default_suffix(
            &path,
            &MetadataWrite {
                datetime: Some("2024:06:15 10:00:00".to_string()),
                ..MetadataWrite::default()
            },
        )
        .unwrap();
        let probe = probe_exif(&path).unwrap();
        assert!(probe.datetime_original.is_some());
        fs::remove_file(&path).ok();
    }

    #[test]
    fn apply_metadata_rating_roundtrips() {
        let dir = test_tmp_dir("meta_tests");
        let path = fresh_jpeg(&dir, "rating.jpg");
        apply_metadata_with_default_suffix(
            &path,
            &MetadataWrite {
                rating: Some(4),
                ..MetadataWrite::default()
            },
        )
        .unwrap();
        let meta = read_meta(&path);
        let rating = meta.property_i32(xmp_ns::XMP, "Rating").unwrap();
        assert_eq!(rating.value, 4);
        fs::remove_file(&path).ok();
    }

    #[test]
    fn apply_metadata_rating_clamps_above_5() {
        let dir = test_tmp_dir("meta_tests");
        let path = fresh_jpeg(&dir, "rating_clamp.jpg");
        apply_metadata_with_default_suffix(
            &path,
            &MetadataWrite {
                rating: Some(99),
                ..MetadataWrite::default()
            },
        )
        .unwrap();
        let meta = read_meta(&path);
        let rating = meta.property_i32(xmp_ns::XMP, "Rating").unwrap();
        assert_eq!(rating.value, 5);
        fs::remove_file(&path).ok();
    }

    #[test]
    fn apply_metadata_gps_roundtrips() {
        const ALTITUDE: f64 = 9.250_123_456_789;
        const ALTITUDE_RATIONAL: &str = "2032686204/219746927";

        let dir = test_tmp_dir("meta_tests");
        let path = fresh_jpeg(&dir, "gps.jpg");
        apply_metadata_with_default_suffix(
            &path,
            &MetadataWrite {
                gps: Some(GpsCoords {
                    latitude: 37.7749,
                    longitude: -122.4194,
                    altitude: Some(ALTITUDE),
                }),
                ..MetadataWrite::default()
            },
        )
        .unwrap();
        let probe = probe_exif(&path).unwrap();
        assert!(probe.has_gps);
        let meta = read_meta(&path);
        let lat = meta.property(xmp_ns::EXIF, "GPSLatitude").unwrap().value;
        assert!(lat.contains('N'), "lat should end with N: {lat}");
        let lng = meta.property(xmp_ns::EXIF, "GPSLongitude").unwrap().value;
        assert!(lng.contains('W'), "lng should end with W: {lng}");
        assert_eq!(
            meta.property(xmp_ns::EXIF, "GPSAltitude").unwrap().value,
            ALTITUDE_RATIONAL
        );

        let native = Metadata::new_from_path(&path).unwrap();
        let native_altitude = native
            .get_tag(&ExifTag::GPSAltitude(Vec::new()))
            .next()
            .and_then(|tag| match tag {
                ExifTag::GPSAltitude(values) => values.first(),
                _ => None,
            });
        assert_eq!(native_altitude, Some(&ur64(2_032_686_204, 219_746_927)));
        fs::remove_file(&path).ok();
    }

    #[test]
    fn apply_metadata_description_roundtrips() {
        let dir = test_tmp_dir("meta_tests");
        let path = fresh_jpeg(&dir, "desc.jpg");
        apply_metadata_with_default_suffix(
            &path,
            &MetadataWrite {
                description: Some("Beach day".to_string()),
                ..MetadataWrite::default()
            },
        )
        .unwrap();
        let meta = read_meta(&path);
        let (desc, _lang) = meta
            .localized_text(xmp_ns::DC, "description", None, "x-default")
            .unwrap();
        assert_eq!(desc.value, "Beach day");
        fs::remove_file(&path).ok();
    }

    #[test]
    fn apply_metadata_title_and_keywords_roundtrip() {
        let dir = test_tmp_dir("meta_tests");
        let path = fresh_jpeg(&dir, "tags.jpg");
        apply_metadata_with_default_suffix(
            &path,
            &MetadataWrite {
                title: Some("Vacation shot".to_string()),
                keywords: vec!["vacation".into(), "beach".into(), "Favorites".into()],
                ..MetadataWrite::default()
            },
        )
        .unwrap();
        let meta = read_meta(&path);
        let (title, _lang) = meta
            .localized_text(xmp_ns::DC, "title", None, "x-default")
            .unwrap();
        assert_eq!(title.value, "Vacation shot");
        let subjects: Vec<String> = meta
            .property_array(xmp_ns::DC, "subject")
            .map(|v| v.value)
            .collect();
        assert_eq!(subjects.len(), 3);
        assert!(subjects.contains(&"vacation".to_string()));
        assert!(subjects.contains(&"beach".to_string()));
        assert!(subjects.contains(&"Favorites".to_string()));
        fs::remove_file(&path).ok();
    }

    #[test]
    fn apply_metadata_people_roundtrips() {
        let dir = test_tmp_dir("meta_tests");
        let path = fresh_jpeg(&dir, "people.jpg");
        apply_metadata_with_default_suffix(
            &path,
            &MetadataWrite {
                people: vec!["Alice".into(), "Bob".into()],
                ..MetadataWrite::default()
            },
        )
        .unwrap();
        let meta = read_meta(&path);
        let names: Vec<String> = meta
            .property_array(xmp_ns::IPTC_EXT, "PersonInImage")
            .map(|v| v.value)
            .collect();
        assert_eq!(names, vec!["Alice".to_string(), "Bob".to_string()]);
        fs::remove_file(&path).ok();
    }

    #[test]
    fn apply_metadata_kei_namespace_fields() {
        let dir = test_tmp_dir("meta_tests");
        let path = fresh_jpeg(&dir, "kei_ns.jpg");
        apply_metadata_with_default_suffix(
            &path,
            &MetadataWrite {
                is_hidden: true,
                is_archived: true,
                media_subtype: Some("portrait".into()),
                burst_id: Some("burst_abc".into()),
                ..MetadataWrite::default()
            },
        )
        .unwrap();
        let meta = read_meta(&path);
        assert!(meta.property_bool(KEI_XMP_NS, "hidden").unwrap().value);
        assert!(meta.property_bool(KEI_XMP_NS, "archived").unwrap().value);
        assert_eq!(
            meta.property(KEI_XMP_NS, "mediaSubtype").unwrap().value,
            "portrait"
        );
        assert_eq!(
            meta.property(KEI_XMP_NS, "burstId").unwrap().value,
            "burst_abc"
        );
        fs::remove_file(&path).ok();
    }

    #[test]
    fn apply_metadata_all_fields_single_pass() {
        let dir = test_tmp_dir("meta_tests");
        let path = fresh_jpeg(&dir, "all.jpg");
        apply_metadata_with_default_suffix(
            &path,
            &MetadataWrite {
                datetime: Some("2024:06:15 10:00:00".to_string()),
                offset_time_original: None,
                clear_datetime_offsets: false,
                rating: Some(5),
                gps: Some(GpsCoords {
                    latitude: 1.0,
                    longitude: 2.0,
                    altitude: None,
                }),
                title: Some("T".into()),
                description: Some("D".into()),
                keywords: vec!["k".into()],
                people: vec!["Alice".into()],
                is_hidden: false,
                is_archived: true,
                media_subtype: Some("live_photo".into()),
                burst_id: None,
                ..MetadataWrite::default()
            },
        )
        .unwrap();
        let probe = probe_exif(&path).unwrap();
        assert!(probe.datetime_original.is_some());
        assert!(probe.has_gps);
        fs::remove_file(&path).ok();
    }

    #[test]
    fn apply_metadata_cleans_up_tmp_on_failure() {
        let dir = test_tmp_dir("meta_tests");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("corrupt.jpg");
        fs::write(&path, b"not a jpeg").unwrap();
        let result = apply_metadata_with_default_suffix(
            &path,
            &MetadataWrite {
                rating: Some(3),
                ..MetadataWrite::default()
            },
        );
        assert!(result.is_err(), "corrupt file should fail metadata write");
        let mut tmp_name = path.file_name().unwrap().to_os_string();
        tmp_name.push(".meta-tmp");
        let tmp_path = path.with_file_name(&tmp_name);
        assert!(
            !tmp_path.exists(),
            ".meta-tmp must be cleaned up after a failed write"
        );
        fs::remove_file(&path).ok();
    }

    #[test]
    fn xmp_toolkit_rewrite_fsyncs_temp_before_part_replace() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_path_buf();
        let path = fresh_jpeg(&dir, "durable_install.jpg");
        let original = fs::read(&path).unwrap();
        let install_called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let install_called_in_closure = Arc::clone(&install_called);
        let expected_path = path.clone();

        let prepared = prepare_metadata_xmp_toolkit(
            &path,
            &MetadataWrite {
                rating: Some(3),
                ..MetadataWrite::default()
            },
            ".meta-tmp",
            None,
        )
        .unwrap();
        let result = prepared.publish_with(&path, move |tmp, dst, _expected, _replacement| {
            install_called_in_closure.store(true, std::sync::atomic::Ordering::SeqCst);
            assert!(
                tmp.exists(),
                "metadata temp file must exist before durable install"
            );
            assert_eq!(dst, expected_path.as_path());
            Err(std::io::Error::other("simulated durable install failure").into())
        });

        assert!(
            result.is_err(),
            "durable install failure must surface to leave the asset retryable"
        );
        assert!(
            install_called.load(std::sync::atomic::Ordering::SeqCst),
            "XMP rewrite must route final replacement through the durable install primitive"
        );
        assert_eq!(
            fs::read(&path).unwrap(),
            original,
            "failed metadata publish must leave original bytes intact"
        );
        assert!(
            embed_temp_entries(&dir, ".meta-tmp").is_empty(),
            "metadata temp file must be cleaned up on durable install failure"
        );
        fs::remove_file(&path).ok();
    }

    #[test]
    fn contract_metadata_embed_rewrite_requires_stable_input() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_path_buf();
        let path = fresh_jpeg(&dir, "concurrent_edit.jpg");
        let external = b"external edit".to_vec();

        let prepared = prepare_metadata_xmp_toolkit(
            &path,
            &MetadataWrite {
                rating: Some(3),
                ..MetadataWrite::default()
            },
            ".meta-tmp",
            None,
        )
        .unwrap();
        let result = prepared.publish_with(&path, |src, dst, expected, expected_replacement| {
            fs::write(dst, &external)?;
            publish_prepared_embed(src, dst, expected, expected_replacement)
        });

        let error = result.expect_err("a concurrent edit must block metadata publication");
        assert!(crate::download::file::classify_conditional_publish_error(&error).target_changed);
        assert_eq!(fs::read(&path).unwrap(), external);
        assert!(embed_temp_entries(&dir, ".meta-tmp").is_empty());
    }

    #[test]
    fn apply_metadata_dispatches_xmp_toolkit_on_extension_less_jpeg_part_file() {
        // Negative case: extension-less ≠ HEIF. A JPEG-bearing part file
        // must still route to XMP Toolkit and succeed.
        let dir = test_tmp_dir("meta_tests");
        let path = dir.join("BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB.kei-tmp");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, minimal_jpeg()).unwrap();
        apply_metadata_with_default_suffix(
            &path,
            &MetadataWrite {
                rating: Some(2),
                ..MetadataWrite::default()
            },
        )
        .expect("JPEG metadata write must succeed on .kei-tmp part file");
        let meta = read_meta(&path);
        assert_eq!(
            meta.property(xmp_ns::XMP, "Rating").map(|v| v.value),
            Some("2".to_string()),
        );
        fs::remove_file(&path).ok();
    }
}
