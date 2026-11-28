//! XMP Toolkit preparation for native format handlers.

use super::movie_layout::{
    is_movie_file, validate_movie_metadata_layout, validate_movie_rewrite_preserves_media,
};
use super::prepared::{
    PreparedMetadataFile, TmpGuard, copy_fingerprinted, create_unique_embed_temp,
    keep_source_mtime, lasting_unless_io,
};
use super::values::MetadataWrite;
use super::xmp_fields::{apply_to_xmp, ensure_initialized};
use anyhow::{Context, Result};
use std::path::Path;
use xmp_toolkit::{OpenFileOptions, XmpFile, XmpMeta, xmp_ns};

/// XMP properties the toolkit maps onto `mvhd` creation and modification times.
const MOVIE_HEADER_DATES: [&str; 2] = ["CreateDate", "ModifyDate"];

#[cfg(test)]
pub(crate) type ToolkitWriteHook = Box<dyn FnOnce(&Path, &Path) + Send>;
/// Test hooks keyed by destination path. Each runs once, after the toolkit
/// writes the temporary copy and before the preservation check. Keyed by path
/// rather than thread so sync-level tests can reach the blocking write task.
#[cfg(test)]
static AFTER_TOOLKIT_WRITE: std::sync::Mutex<Vec<(std::path::PathBuf, ToolkitWriteHook)>> =
    std::sync::Mutex::new(Vec::new());

/// Run `hook(destination, temp)` after the next toolkit write for `destination`.
#[cfg(test)]
pub(crate) fn after_toolkit_write(destination: &Path, hook: ToolkitWriteHook) {
    AFTER_TOOLKIT_WRITE
        .lock()
        .unwrap()
        .push((destination.to_path_buf(), hook));
}

/// Remove an unused hook for `destination`; true when one was still waiting,
/// which proves no toolkit write reached that destination.
#[cfg(test)]
pub(crate) fn take_after_toolkit_write(destination: &Path) -> bool {
    let mut hooks = AFTER_TOOLKIT_WRITE.lock().unwrap();
    let before = hooks.len();
    hooks.retain(|(path, _)| path != destination);
    hooks.len() != before
}

#[cfg(test)]
fn run_after_toolkit_write(destination: &Path, temp: &Path) {
    let hook = {
        let mut hooks = AFTER_TOOLKIT_WRITE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        hooks
            .iter()
            .position(|(path, _)| path == destination)
            .map(|index| hooks.remove(index).1)
    };
    if let Some(hook) = hook {
        hook(destination, temp);
    }
}

pub(super) fn prepare_metadata_xmp_toolkit(
    path: &Path,
    write: &MetadataWrite,
    temp_suffix: &str,
    expected_fingerprint: Option<crate::download::file::ExistingFileFingerprint>,
) -> Result<PreparedMetadataFile> {
    ensure_initialized();

    let (mut file, tmp_path) = create_unique_embed_temp(path, temp_suffix)?;
    let cleanup_permissions = file
        .metadata()
        .with_context(|| format!("Could not inspect permissions of {}", tmp_path.display()))?
        .permissions();
    let guard = TmpGuard::with_cleanup_permissions(&tmp_path, cleanup_permissions);
    // One read of the source both copies and fingerprints it, so the copy is
    // exactly the bytes that are checked against the approved fingerprint.
    let expected = copy_fingerprinted(path, &mut file, &tmp_path)?;
    drop(file);
    if expected_fingerprint.is_some_and(|approved| approved != expected) {
        return Err(
            crate::download::file::ConditionalPublishTargetChanged::AfterPlanning {
                path: path.to_path_buf(),
            }
            .into(),
        );
    }

    let movie = is_movie_file(&tmp_path)?;
    if movie {
        validate_movie_metadata_layout(&tmp_path).map_err(lasting_unless_io)?;
    }
    if write.orientation.is_some() {
        anyhow::ensure!(
            super::probe::original_orientation_missing(&tmp_path)?,
            "Orientation omission is no longer proven"
        );
    }
    let clearing = write.clear_embedded != Default::default();
    let native_before = if clearing
        || write.description.is_some()
        || write.prior_embedded.native_description.is_some()
        || write.gps.is_some()
        || write.clear_embedded.gps.iter().any(Option::is_some)
        || write.prior_embedded.native_gps.iter().any(Option::is_some)
    {
        native_gps(&tmp_path)?
    } else {
        None
    };
    let result: Result<(
        crate::state::types::EmbeddedMetadataFields,
        crate::state::types::EmbeddedMetadataFields,
    )> = (|| {
        let mut file = XmpFile::new().context("Could not create XMP handle")?;
        file.open_file(
            &tmp_path,
            // Smart handlers reconcile XMP deletions into native TIFF, IPTC,
            // and container fields. Those namespaces have independent owners.
            // Packet-only updates leave all companions untouched; below we
            // clear only native properties with their own matching write proof.
            if clearing {
                OpenFileOptions::default()
                    .for_update()
                    .use_packet_scanning()
            } else {
                OpenFileOptions::default().for_update().use_smart_handler()
            },
        )
        .with_context(|| format!("Could not open {} for XMP update", tmp_path.display()))?;

        let mut meta = file
            .xmp()
            .unwrap_or_else(|| XmpMeta::new().unwrap_or_default());
        let before = super::xmp_fields::embedded_fields(&meta);
        // The toolkit maps these two properties onto the movie header's
        // native creation and modification times, importing them on open and
        // exporting them on save. Keeping the imported values leaves `mvhd`
        // untouched; kei's capture date still lands in the other date fields.
        let movie_dates =
            movie.then(|| MOVIE_HEADER_DATES.map(|name| meta.property(xmp_ns::XMP, name)));
        apply_to_xmp(&mut meta, write)?;
        if let Some(dates) = movie_dates {
            for (name, value) in MOVIE_HEADER_DATES.into_iter().zip(dates) {
                match value {
                    Some(value) => meta.set_property(xmp_ns::XMP, name, &value)?,
                    None => meta.delete_property(xmp_ns::XMP, name)?,
                }
            }
        }
        let after = super::xmp_fields::embedded_fields(&meta);

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
        Ok((before, after))
    })();

    let (mut before, mut after) = result?;
    if let Some((file_type, original)) = native_before {
        native_companion_values(&original, &mut before);
        if clearing {
            let bytes = std::fs::read(&tmp_path)?;
            let mut native = little_exif::metadata::Metadata::new_from_vec(&bytes, file_type)?;
            let mut removed = false;
            for ((tag, prior), actual) in original
                .iter()
                .zip(
                    write
                        .clear_embedded
                        .native_gps
                        .iter()
                        .chain(std::iter::once(&write.clear_embedded.native_description)),
                )
                .zip(
                    before
                        .native_gps
                        .iter()
                        .chain(std::iter::once(&before.native_description)),
                )
            {
                if let Some(tag) = tag
                    && prior.is_some()
                    && prior == actual
                {
                    native.remove_tag(tag.clone());
                    removed = true;
                }
            }
            if removed {
                let mut bytes = bytes;
                native.write_to_vec(&mut bytes, file_type)?;
                std::fs::write(&tmp_path, bytes)?;
            }
        }
        if let Some((_, tags)) = native_gps(&tmp_path)? {
            native_companion_values(&tags, &mut after);
        }
    }
    #[cfg(test)]
    run_after_toolkit_write(path, &tmp_path);
    let checked_output = if movie {
        match validate_movie_rewrite_preserves_media(path, &tmp_path) {
            Ok(checked) => checked,
            Err(error) => {
                // The comparison reads the live destination. If it changed
                // since the copy, report that rather than blaming the toolkit.
                let current =
                    crate::download::file::fingerprint_regular_file_snapshot_blocking(path)
                        .map(|snapshot| snapshot.fingerprint)
                        .ok();
                if current != Some(expected) {
                    return Err(
                        crate::download::file::ConditionalPublishTargetChanged::AfterPlanning {
                            path: path.to_path_buf(),
                        }
                        .into(),
                    );
                }
                return Err(lasting_unless_io(error));
            }
        }
    } else {
        None
    };
    let embedded_fields = write.ownership_after(&before, &after);
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
    // Nothing writes the temp file's bytes after the movie check hashed them.
    let output = match checked_output {
        Some(output) => output,
        None => {
            crate::download::file::fingerprint_regular_file_snapshot_blocking(&tmp_path)
                .with_context(|| format!("Could not fingerprint {}", tmp_path.display()))?
                .fingerprint
        }
    };
    Ok(PreparedMetadataFile {
        required_mtime: None,
        embedded_fields: Box::new(embedded_fields),
        guard,
        expected_input: expected,
        expected_output: output,
    })
}

fn native_gps(
    path: &Path,
) -> Result<
    Option<(
        little_exif::filetype::FileExtension,
        [Option<little_exif::exif_tag::ExifTag>; 7],
    )>,
> {
    use little_exif::{exif_tag::ExifTag, filetype::FileExtension, metadata::Metadata};
    use std::io::Read;
    let mut source = std::fs::File::open(path)?;
    let mut head = [0; 12];
    let count = source.read(&mut head)?;
    let head = head.get(..count).unwrap_or_default();
    let kind = if head.starts_with(&[0xff, 0xd8, 0xff]) {
        FileExtension::JPEG
    } else if head.starts_with(b"II*\0") || head.starts_with(b"MM\0*") {
        FileExtension::TIFF
    } else if head.starts_with(b"\x89PNG\r\n\x1a\n") {
        FileExtension::PNG {
            as_zTXt_chunk: false,
        }
    } else {
        return Ok(None);
    };
    let mut bytes = head.to_vec();
    source.read_to_end(&mut bytes)?;
    let native = match Metadata::new_from_vec(&bytes, kind) {
        Ok(native) => native,
        Err(error) if error.to_string().contains("No EXIF data found") => Metadata::new(),
        Err(error) => return Err(error.into()),
    };
    let tags = [
        ExifTag::GPSLatitude(vec![]),
        ExifTag::GPSLongitude(vec![]),
        ExifTag::GPSAltitude(vec![]),
        ExifTag::GPSLatitudeRef(String::new()),
        ExifTag::GPSLongitudeRef(String::new()),
        ExifTag::GPSAltitudeRef(vec![]),
        ExifTag::ImageDescription(String::new()),
    ];
    Ok(Some((
        kind,
        tags.map(|tag| native.get_tag(&tag).next().cloned()),
    )))
}

fn native_companion_values(
    tags: &[Option<little_exif::exif_tag::ExifTag>; 7],
    fields: &mut crate::state::types::EmbeddedMetadataFields,
) {
    for (value, tag) in fields
        .native_gps
        .iter_mut()
        .chain(std::iter::once(&mut fields.native_description))
        .zip(tags)
    {
        *value = tag.as_ref().map(|tag| {
            data_encoding::HEXLOWER
                .encode(&tag.value_as_u8_vec(&little_exif::endian::Endian::Little))
        });
    }
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
    fn quicktime_rewrite_embeds_metadata_and_preserves_bare_track_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("live.mov.kei-tmp");
        let original = crate::test_helpers::bare_track_meta_movie();
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
        for kind in [b"trak", b"mdat"] {
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

    // ── Movie preservation (MOVIE_EMBED_REWRITE_PRESERVES_MEDIA) ───────

    /// QuickTime user data with native text items next to kei's XMP.
    fn quicktime_text_items_movie() -> Vec<u8> {
        let item = |kind: &[u8; 4], text: &str| {
            let mut body = u16::try_from(text.len()).unwrap().to_be_bytes().to_vec();
            body.extend_from_slice(&[0x15, 0xc7]);
            body.extend_from_slice(text.as_bytes());
            let mut bytes = u32::try_from(body.len() + 8)
                .unwrap()
                .to_be_bytes()
                .to_vec();
            bytes.extend_from_slice(kind);
            bytes.extend(body);
            bytes
        };
        let mut tail = item(b"\xa9nam", "Native title");
        tail.extend(item(b"\xa9des", "Native description"));
        tail.extend(item(b"\xa9day", "2001-01-01"));
        tail.extend(item(b"\xa9xyz", "+10.0000+020.0000/"));
        crate::test_helpers::quicktime_user_data_movie(&tail)
    }

    fn movie_fixtures() -> Vec<(&'static str, Vec<u8>)> {
        vec![
            (
                "apple-live.mov",
                include_bytes!("../../../tests/data/media/apple-live.mov").to_vec(),
            ),
            (
                "pattern.mov",
                include_bytes!("../../../tests/data/media/pattern.mov").to_vec(),
            ),
            (
                "pattern.mp4",
                include_bytes!("../../../tests/data/media/pattern.mp4").to_vec(),
            ),
            (
                "bare-track-meta.mov",
                crate::test_helpers::bare_track_meta_movie(),
            ),
            ("text-items.mov", quicktime_text_items_movie()),
            ("dated.mp4", dated_mp4()),
            ("preview-first.mov", preview_first_movie()),
            ("creator-tool.mov", creator_tool_movie()),
            ("copyright.mov", copyright_movie()),
        ]
    }

    fn atom(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut bytes = u32::try_from(body.len() + 8)
            .unwrap()
            .to_be_bytes()
            .to_vec();
        bytes.extend_from_slice(kind);
        bytes.extend_from_slice(body);
        bytes
    }

    /// pattern.mp4 with real `mvhd` creation and modification times.
    fn dated_mp4() -> Vec<u8> {
        let mut bytes = include_bytes!("../../../tests/data/media/pattern.mp4").to_vec();
        let mvhd = bytes.windows(4).position(|w| w == b"mvhd").unwrap() + 4;
        // 2023-01-02 and 2023-01-03, in seconds since 1904.
        bytes[mvhd + 4..mvhd + 8].copy_from_slice(&3_755_548_800_u32.to_be_bytes());
        bytes[mvhd + 8..mvhd + 12].copy_from_slice(&3_755_635_200_u32.to_be_bytes());
        bytes
    }

    /// Classic QuickTime: no `ftyp`, a `pnot` preview first.
    fn preview_first_movie() -> Vec<u8> {
        let movie = crate::test_helpers::bare_track_meta_movie();
        let ftyp = usize::try_from(u32::from_be_bytes(movie[..4].try_into().unwrap())).unwrap();
        [atom(b"pnot", &[0; 12]), movie[ftyp..].to_vec()].concat()
    }

    /// QuickTime movie whose XMP names a creator tool, which the toolkit
    /// mirrors into a new `Cr8r` atom.
    fn creator_tool_movie() -> Vec<u8> {
        let packet = br#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmp:CreatorTool="Editor 1.0"/></rdf:RDF></x:xmpmeta>"#;
        crate::test_helpers::quicktime_user_data_movie(&atom(b"XMP_", packet))
    }

    /// QuickTime movie with an ISO `cprt` atom, which the toolkit reconciles
    /// with `dc:rights`.
    fn copyright_movie() -> Vec<u8> {
        // version/flags, packed language "eng", then the text.
        crate::test_helpers::quicktime_user_data_movie(&atom(
            b"cprt",
            b"\0\0\0\0\x15\xc7(c) Someone\0",
        ))
    }

    fn every_movie_field() -> MetadataWrite {
        MetadataWrite {
            rating: Some(4),
            datetime: Some("2024:05:06 07:08:09".into()),
            offset_time_original: Some("+02:00".into()),
            gps: Some(GpsCoords {
                latitude: 1.5,
                longitude: 2.5,
                altitude: Some(3.0),
            }),
            title: Some("Title".into()),
            description: Some("Description".into()),
            keywords: vec!["one".into(), "two".into()],
            ..MetadataWrite::default()
        }
    }

    /// Independent top-level walk: (kind, offset, bytes) for every box.
    fn top_level_boxes(bytes: &[u8]) -> Vec<([u8; 4], usize, Vec<u8>)> {
        let mut boxes = Vec::new();
        let mut offset = 0;
        while offset + 8 <= bytes.len() {
            let size = u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap());
            let size = match size {
                0 => bytes.len() - offset,
                1 => usize::try_from(u64::from_be_bytes(
                    bytes[offset + 8..offset + 16].try_into().unwrap(),
                ))
                .unwrap(),
                size => usize::try_from(size).unwrap(),
            };
            let kind = bytes[offset + 4..offset + 8].try_into().unwrap();
            boxes.push((kind, offset, bytes[offset..offset + size].to_vec()));
            offset += size;
        }
        boxes
    }

    fn atom_bytes(bytes: &[u8], kind: &[u8; 4]) -> Vec<Vec<u8>> {
        let mut found = Vec::new();
        let mut from = 0;
        while let Some(at) = bytes[from..].windows(4).position(|window| window == kind) {
            let start = from + at - 4;
            let size = u32::from_be_bytes(bytes[start..start + 4].try_into().unwrap());
            found.push(bytes[start..start + usize::try_from(size).unwrap()].to_vec());
            from = from + at + 4;
        }
        found
    }

    #[test]
    fn contract_movie_embed_rewrite_preserves_media_across_real_toolkit_rewrites() {
        for (name, original) in movie_fixtures() {
            for write in [
                MetadataWrite {
                    rating: Some(3),
                    ..MetadataWrite::default()
                },
                every_movie_field(),
            ] {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join(format!("{name}.kei-tmp"));
                fs::write(&path, &original).unwrap();
                apply_metadata_with_default_suffix(&path, &write).unwrap();
                // A second, larger packet forces the toolkit to grow `moov` again.
                let mut again = write.clone();
                again.rating = Some(5);
                again.description = Some("A longer description ".repeat(200));
                apply_metadata_with_default_suffix(&path, &again).unwrap();
                let rewritten = fs::read(&path).unwrap();

                assert_eq!(
                    read_meta(&path)
                        .property_i32(xmp_ns::XMP, "Rating")
                        .unwrap()
                        .value,
                    5,
                    "{name}"
                );
                let media = |bytes: &[u8]| {
                    top_level_boxes(bytes)
                        .into_iter()
                        .filter(|(kind, _, _)| kind == b"mdat" || kind == b"ftyp")
                        .collect::<Vec<_>>()
                };
                assert_eq!(media(&rewritten), media(&original), "{name}: media moved");
                // Old `moov` bytes may survive inside `free`; inspect the live one.
                let live_moov = |bytes: &[u8]| {
                    let moov = top_level_boxes(bytes)
                        .into_iter()
                        .filter(|(kind, _, _)| kind == b"moov")
                        .collect::<Vec<_>>();
                    assert_eq!(moov.len(), 1, "{name}: one moov");
                    moov.into_iter().next().unwrap().2
                };
                let (before_moov, after_moov) = (live_moov(&original), live_moov(&rewritten));
                for kind in [
                    b"mvhd", b"trak", b"\xa9nam", b"\xa9des", b"\xa9day", b"\xa9xyz", b"name",
                ] {
                    assert_eq!(
                        atom_bytes(&after_moov, kind),
                        atom_bytes(&before_moov, kind),
                        "{name}: {kind:?}"
                    );
                }
                assert!(embed_temp_entries(dir.path(), ".meta-tmp").is_empty());
            }
        }
    }

    #[test]
    fn contract_movie_embed_rewrite_preserves_media_refuses_damaged_rewrites() {
        type Damage = fn(&mut Vec<u8>);
        let damages: [(&str, Damage); 5] = [
            ("track handler (#947)", |bytes| {
                // The old `moov` may survive as `free` padding; damage the live one.
                let at = bytes
                    .windows(4)
                    .rposition(|window| window == b"mdta")
                    .unwrap();
                bytes[at..at + 4].copy_from_slice(&[0, 0, 1, 0xe7]);
            }),
            ("media byte", |bytes| {
                let at = bytes
                    .windows(15)
                    .position(|window| window == b"unchanged media")
                    .unwrap();
                bytes[at] ^= 1;
            }),
            ("media offset", |bytes| {
                let at = bytes
                    .windows(4)
                    .position(|window| window == b"mdat")
                    .unwrap()
                    - 4;
                bytes.splice(at..at, *b"\0\0\0\x08free");
            }),
            ("movie timescale", |bytes| {
                let mvhd = bytes
                    .windows(4)
                    .rposition(|window| window == b"mvhd")
                    .unwrap();
                // version/flags (4) + creation/modification times (8).
                bytes[mvhd + 4 + 12] ^= 1;
            }),
            ("native user data", |bytes| {
                let at = bytes
                    .windows(4)
                    .rposition(|window| window == b"kept")
                    .unwrap();
                bytes[at..at + 4].copy_from_slice(b"lost");
            }),
        ];
        for (label, damage) in damages {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("live.mov");
            let original = quicktime_text_items_movie();
            fs::write(&path, &original).unwrap();
            super::after_toolkit_write(
                &path,
                Box::new(move |_destination, temp| {
                    let mut bytes = fs::read(temp).unwrap();
                    damage(&mut bytes);
                    fs::write(temp, bytes).unwrap();
                }),
            );
            let error = match prepare_metadata_xmp_toolkit(
                &path,
                &every_movie_field(),
                ".meta-tmp",
                None,
            ) {
                Ok(_) => panic!("{label}: damaged rewrite was accepted"),
                Err(error) => error,
            };
            assert!(
                error
                    .to_string()
                    .contains("Refusing embedded metadata rewrite"),
                "{label}: {error:#}"
            );
            assert!(
                super::super::is_lasting_refusal(&error),
                "{label}: the same bytes would be refused again"
            );
            assert_eq!(fs::read(&path).unwrap(), original, "{label}");
            assert!(
                embed_temp_entries(dir.path(), ".meta-tmp").is_empty(),
                "{label}"
            );
        }
    }

    #[test]
    fn contract_movie_embed_rewrite_preserves_media_creator_tool_gains_cr8r() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creator.mov");
        fs::write(&path, creator_tool_movie()).unwrap();
        apply_metadata_with_default_suffix(&path, &every_movie_field()).unwrap();
        let rewritten = fs::read(&path).unwrap();
        assert!(
            rewritten.windows(4).any(|w| w == b"Cr8r"),
            "the toolkit owns and wrote the creator atom"
        );
    }

    #[test]
    fn movie_check_reports_a_concurrent_edit_as_a_changed_target() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("live.mov");
        fs::write(&path, quicktime_text_items_movie()).unwrap();
        super::after_toolkit_write(
            &path,
            Box::new(|destination, _temp| {
                // An edit to the destination during the rewrite.
                let mut bytes = fs::read(destination).unwrap();
                let at = bytes
                    .windows(15)
                    .position(|w| w == b"unchanged media")
                    .unwrap();
                bytes[at] ^= 1;
                fs::write(destination, bytes).unwrap();
            }),
        );
        let error =
            match prepare_metadata_xmp_toolkit(&path, &every_movie_field(), ".meta-tmp", None) {
                Ok(_) => panic!("an edited destination was accepted"),
                Err(error) => error,
            };
        assert!(
            crate::download::file::classify_conditional_publish_error(&error).target_changed,
            "{error:#}"
        );
        assert!(!super::super::is_lasting_refusal(&error));
        assert!(embed_temp_entries(dir.path(), ".meta-tmp").is_empty());
    }
}
