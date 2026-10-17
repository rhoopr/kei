//! XMP Toolkit preparation for native format handlers.

use super::prepared::{
    PreparedMetadataFile, TmpGuard, create_unique_embed_temp, keep_source_mtime,
};
use super::values::MetadataWrite;
use super::xmp_fields::{apply_to_xmp, ensure_initialized};
use anyhow::{Context, Result};
use std::io::{Read, Seek, SeekFrom};
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

    validate_movie_metadata_layout(&tmp_path)?;
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
    keep_source_mtime(&temp, path);
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

/// The toolkit assumes every nested `meta` has a four-byte FullBox prefix.
/// QuickTime also permits bare child boxes there, which its rewrite corrupts.
fn validate_movie_metadata_layout(path: &Path) -> Result<()> {
    let mut file = std::fs::File::open(path)?;
    let end = file.metadata()?.len();
    let mut header = [0; 8];
    if end < 8 {
        return Ok(());
    }
    file.read_exact(&mut header)?;
    if !matches!(
        &header[4..8],
        b"ftyp" | b"moov" | b"mdat" | b"wide" | b"free" | b"skip"
    ) {
        return Ok(());
    }
    let mut budget = 100_000;
    validate_movie_boxes(&mut file, 0, end, None, 0, &mut budget)
}

fn validate_movie_boxes(
    file: &mut std::fs::File,
    mut offset: u64,
    end: u64,
    parent: Option<&[u8; 4]>,
    depth: usize,
    budget: &mut usize,
) -> Result<()> {
    anyhow::ensure!(
        depth <= 32,
        "Movie metadata nesting exceeds preservation check limit"
    );
    while offset < end {
        // QuickTime user-data lists may end with a four-byte zero terminator.
        if parent == Some(b"udta") && end - offset == 4 {
            file.seek(SeekFrom::Start(offset))?;
            let mut terminator = [0; 4];
            file.read_exact(&mut terminator)?;
            anyhow::ensure!(terminator == [0; 4], "Invalid movie user-data terminator");
            return Ok(());
        }
        anyhow::ensure!(
            *budget > 0,
            "Movie box count exceeds preservation check limit"
        );
        *budget -= 1;
        anyhow::ensure!(end - offset >= 8, "Incomplete movie box header");
        file.seek(SeekFrom::Start(offset))?;
        let mut header = [0; 8];
        file.read_exact(&mut header)?;
        let kind: &[u8; 4] = header[4..8].try_into()?;
        let size = u32::from_be_bytes(header[..4].try_into()?);
        let (size, header_size) = match size {
            0 => (end - offset, 8),
            1 => {
                anyhow::ensure!(end - offset >= 16, "Incomplete extended movie box header");
                let mut extended = [0; 8];
                file.read_exact(&mut extended)?;
                (u64::from_be_bytes(extended), 16)
            }
            size => (u64::from(size), 8),
        };
        anyhow::ensure!(
            size >= header_size && size <= end - offset,
            "Invalid movie box bounds"
        );
        let next = offset + size;
        let mut children = offset + header_size;
        if kind == b"moov"
            || (parent.is_some()
                && matches!(
                    kind,
                    b"udta"
                        | b"meta"
                        | b"ilst"
                        | b"trak"
                        | b"edts"
                        | b"mdia"
                        | b"minf"
                        | b"dinf"
                        | b"stbl"
                ))
        {
            if kind == b"meta" && parent != Some(b"moov") {
                anyhow::ensure!(next - children >= 4, "Incomplete movie metadata prefix");
                file.seek(SeekFrom::Start(children))?;
                let mut prefix = [0; 4];
                file.read_exact(&mut prefix)?;
                anyhow::ensure!(
                    prefix == [0; 4],
                    "Refusing embedded metadata rewrite: nested QuickTime metadata lacks a supported FullBox prefix; the XMP toolkit could damage existing metadata"
                );
                children += 4;
            }
            validate_movie_boxes(file, children, next, Some(kind), depth + 1, budget)?;
        }
        offset = next;
    }
    Ok(())
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
    fn quicktime_rewrite_rejects_bare_track_metadata_before_damage() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("live.mov");
        let original = crate::test_helpers::bare_track_meta_movie();
        fs::write(&path, &original).unwrap();
        let error = apply_metadata_with_default_suffix(
            &path,
            &MetadataWrite {
                rating: Some(3),
                ..MetadataWrite::default()
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("nested QuickTime metadata"));
        assert_eq!(fs::read(&path).unwrap(), original);
        assert!(embed_temp_entries(dir.path(), ".meta-tmp").is_empty());
    }

    #[test]
    fn quicktime_supported_movie_still_embeds_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("live.mov.kei-tmp");
        fs::write(
            &path,
            include_bytes!("../../../tests/data/media/apple-live.mov"),
        )
        .unwrap();
        apply_metadata_with_default_suffix(
            &path,
            &MetadataWrite {
                rating: Some(3),
                ..MetadataWrite::default()
            },
        )
        .unwrap();
        assert_eq!(
            read_meta(&path)
                .property_i32(xmp_ns::XMP, "Rating")
                .unwrap()
                .value,
            3
        );
    }

    #[test]
    fn quicktime_user_data_terminator_preserves_metadata_and_media() {
        for tail in [&[][..], &[0; 4][..]] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("live.mov.kei-tmp");
            let original = crate::test_helpers::quicktime_user_data_movie(tail);
            fs::write(&path, &original).unwrap();
            apply_metadata_with_default_suffix(
                &path,
                &MetadataWrite {
                    rating: Some(3),
                    ..MetadataWrite::default()
                },
            )
            .unwrap();
            let rewritten = fs::read(&path).unwrap();
            for kind in [b"hdlr", b"name", b"mdat"] {
                let atom = |bytes: &[u8]| {
                    let start = bytes.windows(4).position(|value| value == kind).unwrap() - 4;
                    let size = u32::from_be_bytes(bytes[start..start + 4].try_into().unwrap());
                    bytes[start..start + usize::try_from(size).unwrap()].to_vec()
                };
                assert_eq!(atom(&rewritten), atom(&original), "{kind:?}");
            }
            assert_eq!(
                read_meta(&path)
                    .property_i32(xmp_ns::XMP, "Rating")
                    .unwrap()
                    .value,
                3
            );
            assert!(embed_temp_entries(dir.path(), ".meta-tmp").is_empty());
        }
    }

    #[test]
    fn quicktime_user_data_terminator_does_not_allow_truncated_boxes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("live.mov");
        for tail in [&[0; 3][..], &[0, 0, 0, 1][..], &[0; 5][..]] {
            let original = crate::test_helpers::quicktime_user_data_movie(tail);
            fs::write(&path, &original).unwrap();
            assert!(
                apply_metadata_with_default_suffix(
                    &path,
                    &MetadataWrite {
                        rating: Some(3),
                        ..MetadataWrite::default()
                    }
                )
                .is_err()
            );
            assert_eq!(fs::read(&path).unwrap(), original);
            assert!(embed_temp_entries(dir.path(), ".meta-tmp").is_empty());
        }
        for parent in [b"trak", b"meta", b"moov"] {
            let mut original = crate::test_helpers::quicktime_user_data_movie(&[0; 4]);
            let user_data = original
                .windows(4)
                .position(|value| value == b"udta")
                .unwrap();
            original[user_data..user_data + 4].copy_from_slice(parent);
            fs::write(&path, &original).unwrap();
            assert!(
                super::validate_movie_metadata_layout(&path).is_err(),
                "{parent:?}"
            );
        }
    }

    #[test]
    fn quicktime_layout_check_accepts_fullbox_and_rejects_invalid_bounds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("movie.part");
        let mut bytes = crate::test_helpers::bare_track_meta_movie();
        let meta_type = bytes.windows(4).position(|value| value == b"meta").unwrap();
        bytes.splice(meta_type + 4..meta_type + 4, [0; 4]);
        for kind in [b"moov", b"trak", b"meta"] {
            let start = bytes.windows(4).position(|value| value == kind).unwrap() - 4;
            let size = u32::from_be_bytes(bytes[start..start + 4].try_into().unwrap());
            bytes[start..start + 4].copy_from_slice(&(size + 4).to_be_bytes());
        }
        fs::write(&path, &bytes).unwrap();
        super::validate_movie_metadata_layout(&path).unwrap();
        // A terminal size-zero media box is bounded by the file, not loaded.
        let mdat = bytes.windows(4).position(|value| value == b"mdat").unwrap() - 4;
        bytes[mdat..mdat + 4].copy_from_slice(&0u32.to_be_bytes());
        fs::write(&path, &bytes).unwrap();
        super::validate_movie_metadata_layout(&path).unwrap();
        bytes[mdat..mdat + 4].copy_from_slice(&u32::MAX.to_be_bytes());
        fs::write(&path, &bytes).unwrap();
        assert!(super::validate_movie_metadata_layout(&path).is_err());
        fs::write(&path, b"\0\0\0\x01moov").unwrap();
        assert!(super::validate_movie_metadata_layout(&path).is_err());
    }

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
