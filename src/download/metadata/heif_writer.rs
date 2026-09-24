//! HEIF-family metadata preparation with media-preservation checks.

use super::prepared::{
    PreparedMetadataFile, TmpGuard, create_unique_embed_temp, fingerprint_bytes,
};
use super::probe::probe_from_native_metadata;
use super::values::MetadataWrite;
use super::xmp_fields::{apply_to_xmp, ensure_initialized};
use crate::download::heif;
use anyhow::{Context, Result};
use little_exif::filetype::FileExtension;
use little_exif::metadata::Metadata;
use std::path::Path;
use xmp_toolkit::XmpMeta;

pub(super) fn prepare_metadata_heif(
    path: &Path,
    write: &MetadataWrite,
    temp_suffix: &str,
    expected_fingerprint: Option<crate::download::file::ExistingFileFingerprint>,
) -> Result<PreparedMetadataFile> {
    use std::io::Read;

    // CONTRACT: HEIF_EMBED_REWRITE_REQUIRES_STABLE_INPUT
    ensure_initialized();
    let mut source = std::fs::File::open(path)
        .with_context(|| format!("Could not open {} for HEIC XMP update", path.display()))?;
    let source_permissions = source
        .metadata()
        .with_context(|| format!("Could not inspect permissions of {}", path.display()))?
        .permissions();
    let mut input = Vec::new();
    source
        .read_to_end(&mut input)
        .with_context(|| format!("Could not read {} for HEIC XMP update", path.display()))?;
    let expected = fingerprint_bytes(&input)?;
    if expected_fingerprint.is_some_and(|approved| approved != expected) {
        return Err(
            crate::download::file::ConditionalPublishTargetChanged::AfterPlanning {
                path: path.to_path_buf(),
            }
            .into(),
        );
    }
    let existing = heif::extract_xmp_strict(&input)
        .with_context(|| format!("Could not inspect existing XMP in {}", path.display()))?;
    let mut meta = match existing.as_deref() {
        Some(bytes) => {
            let text = std::str::from_utf8(bytes)
                .with_context(|| format!("Existing HEIC XMP in {} is not UTF-8", path.display()))?;
            text.parse::<XmpMeta>().with_context(|| {
                format!("Existing HEIC XMP in {} is not valid XMP", path.display())
            })?
        }
        None => XmpMeta::new().context("Could not create XMP metadata")?,
    };
    apply_to_xmp(&mut meta, write)?;
    let xmp = meta.to_string().into_bytes();
    let native_input = if write.require_native_heif_capture_time {
        heif::validate_capture_repair_item_ownership(&input).with_context(|| {
            format!(
                "Could not prove HEIC capture metadata belongs only to the primary image in {}",
                path.display()
            )
        })?;
        let (Some(datetime), Some(offset)) = (
            write.datetime.as_deref(),
            write.offset_time_original.as_deref(),
        ) else {
            anyhow::bail!(
                "HEIC capture timestamp repair has no complete timestamp pair for {}",
                path.display()
            );
        };
        heif::rewrite_exif_capture_time(&input, datetime, offset).with_context(|| {
            format!(
                "Could not update native HEIC Exif capture time in {}",
                path.display()
            )
        })?
    } else {
        None
    };
    if write.require_native_heif_capture_time && native_input.is_none() {
        anyhow::bail!(
            "HEIC capture timestamp repair could not update a native Exif timestamp pair in {}",
            path.display()
        );
    }
    let rewrite_input = native_input.as_deref().unwrap_or(&input);

    let (file, tmp_path) = create_unique_embed_temp(path, temp_suffix)?;
    let cleanup_permissions = file
        .metadata()
        .with_context(|| format!("Could not inspect permissions of {}", tmp_path.display()))?
        .permissions();
    let guard = TmpGuard::with_cleanup_permissions(&tmp_path, cleanup_permissions);
    let mut writer = std::io::BufWriter::new(file);
    heif::rewrite_xmp(rewrite_input, &xmp, &mut writer)
        .with_context(|| format!("Could not update XMP in {}", path.display()))?;
    let file = writer
        .into_inner()
        .with_context(|| format!("Could not flush {}", tmp_path.display()))?;
    file.sync_all()
        .with_context(|| format!("Could not fsync {}", tmp_path.display()))?;
    let mut file = file;
    validate_heif_post_rewrite(&mut file, &tmp_path)?;
    let rewritten = std::fs::read(&tmp_path)
        .with_context(|| format!("Could not validate {}", tmp_path.display()))?;
    let rewritten_fingerprint = fingerprint_bytes(&rewritten)?;
    heif::validate_rewrite_preserves_non_xmp_items(rewrite_input, &rewritten).with_context(
        || {
            format!(
                "HEIC metadata rewrite changed non-XMP media data in {}",
                tmp_path.display()
            )
        },
    )?;
    if native_input.is_some() {
        let native_tiff = heif::extract_exif_tiff_bytes(&rewritten)
            .with_context(|| {
                format!(
                    "Could not inspect rewritten native HEIC Exif in {}",
                    tmp_path.display()
                )
            })?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Rewritten HEIC lost its native Exif item: {}",
                    tmp_path.display()
                )
            })?;
        let native_metadata = Metadata::new_from_vec(&native_tiff, FileExtension::TIFF)
            .with_context(|| {
                format!(
                    "Could not parse rewritten native HEIC Exif in {}",
                    tmp_path.display()
                )
            })?;
        let native_probe = probe_from_native_metadata(&native_metadata);
        if native_probe.datetime_original.as_deref() != write.datetime.as_deref()
            || native_probe.offset_time_original.as_deref() != write.offset_time_original.as_deref()
        {
            anyhow::bail!(
                "Rewritten HEIC native Exif does not contain the requested capture timestamp and offset: {}",
                tmp_path.display()
            );
        }
    }
    let Some(rewritten_xmp) = heif::extract_xmp_strict(&rewritten)
        .with_context(|| format!("Could not inspect rewritten XMP in {}", tmp_path.display()))?
    else {
        anyhow::bail!(
            "HEIC XMP rewrite produced no XMP item: {}",
            tmp_path.display()
        );
    };
    std::str::from_utf8(&rewritten_xmp)
        .with_context(|| format!("Rewritten HEIC XMP is not UTF-8: {}", tmp_path.display()))?
        .parse::<XmpMeta>()
        .with_context(|| format!("Rewritten HEIC XMP is invalid: {}", tmp_path.display()))?;
    // A multi-image HEIC holds one XMP packet per image, so reader and writer
    // must resolve the same item or a merge moves an auxiliary image's metadata
    // onto the photograph. Replacing in place leaves the extent length alone and
    // pads the tail with spaces, so the written packet is a prefix rather than
    // the whole extent.
    let resolves_to_written_packet = rewritten_xmp
        .split_at_checked(xmp.len())
        .is_some_and(|(written, padding)| written == xmp && padding.iter().all(|b| *b == b' '));
    if !resolves_to_written_packet {
        anyhow::bail!(
            "HEIC XMP rewrite resolved a different item than it wrote in {}: wrote {} bytes, read back {} bytes. Refusing to replace the user's file.",
            tmp_path.display(),
            xmp.len(),
            rewritten_xmp.len()
        );
    }
    file.set_permissions(source_permissions)
        .with_context(|| format!("Could not preserve permissions on {}", tmp_path.display()))?;
    file.sync_all()
        .with_context(|| format!("Could not fsync permissions on {}", tmp_path.display()))?;
    drop(file);
    Ok(PreparedMetadataFile {
        guard,
        expected_input: expected,
        expected_output: rewritten_fingerprint,
    })
}

/// Read the first 12 bytes of `file` and verify it starts with an
/// ISO-BMFF `ftyp` box whose major brand is in the HEIF family. Used as
/// a sanity check between `rewrite_xmp` and the atomic rename so a
/// malformed rewrite never lands on disk. Reads from the still-open
/// rewrite handle (seeks back to 0) to avoid reopening `tmp_path`
/// immediately after `sync_all`; the path is only used for diagnostics.
fn validate_heif_post_rewrite(file: &mut std::fs::File, tmp_path: &Path) -> Result<()> {
    use std::io::{Read, Seek, SeekFrom};
    file.seek(SeekFrom::Start(0)).with_context(|| {
        format!(
            "Could not rewind {} for media validation",
            tmp_path.display()
        )
    })?;
    let mut head = [0u8; 12];
    file.read_exact(&mut head)
        .with_context(|| format!("Could not read magic bytes of {}", tmp_path.display()))?;
    if !heif::is_heif_content(&head) {
        anyhow::bail!(
            "HEIC metadata rewrite produced invalid media at {}: the first 12 bytes did not include an ISO-BMFF ftyp/HEIF brand (got {:02x?}). Refusing to replace the user's file.",
            tmp_path.display(),
            head
        );
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unused_result_ok, reason = "test cleanup is best-effort")]
mod tests {
    #[cfg(test)]
    use super::super::apply_metadata_with_expected_fingerprint;
    use super::super::prepared::fingerprint_bytes;
    #[cfg(test)]
    use super::super::test_support::temp_path_for;
    use super::super::test_support::{
        SAMPLE_AVIF, SAMPLE_HEIC, apply_metadata_with_default_suffix, embed_temp_entries,
        fresh_heic, heic_with_xmp_packet, minimal_jpeg, test_tmp_dir, write_seeded_heic,
    };
    use super::super::values::{GpsCoords, MetadataWrite};
    use super::super::xmp_fields::KEI_XMP_NS;
    #[cfg(test)]
    use super::super::xmp_fields::build_xmp_packet;
    use super::{prepare_metadata_heif, validate_heif_post_rewrite};
    use crate::download::heif;
    use crate::test_helpers::heif_ftyp_without_meta_bytes;
    use std::fs;
    use xmp_toolkit::{XmpMeta, xmp_ns};

    /// MS-6: validate_heif_post_rewrite must accept a real HEIC head and
    /// reject anything that doesn't begin with an ISO-BMFF ftyp/HEIF
    /// brand. The probe reads only the first 12 bytes, so a minimal
    /// fixture is sufficient.
    #[test]
    fn validate_heif_post_rewrite_accepts_known_heif_brand() {
        let dir = test_tmp_dir("ms6_validate");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("good.heic");
        // ftyp box: size=0x18, kind=ftyp, major_brand=heic, minor_version=0,
        // compatible_brands=[heic, mif1].
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0x18_u32.to_be_bytes());
        bytes.extend_from_slice(b"ftyp");
        bytes.extend_from_slice(b"heic");
        bytes.extend_from_slice(&0_u32.to_be_bytes());
        bytes.extend_from_slice(b"heic");
        bytes.extend_from_slice(b"mif1");
        fs::write(&path, &bytes).unwrap();
        let mut f = fs::File::open(&path).unwrap();
        validate_heif_post_rewrite(&mut f, &path).expect("known-good heic head must validate");
        fs::remove_file(&path).ok();
    }

    #[test]
    fn validate_heif_post_rewrite_rejects_jpeg_magic() {
        let dir = test_tmp_dir("ms6_validate");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bad-jpeg.heic");
        // 12 bytes of JPEG SOI + FFD8FFE0 + JFIF header — definitely not HEIF.
        fs::write(
            &path,
            [
                0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0x00, 0x01,
            ],
        )
        .unwrap();
        let mut f = fs::File::open(&path).unwrap();
        let err = validate_heif_post_rewrite(&mut f, &path).unwrap_err();
        assert!(err.to_string().contains("ftyp/HEIF brand"), "msg: {err}");
        fs::remove_file(&path).ok();
    }

    #[test]
    fn validate_heif_post_rewrite_rejects_non_heif_iso_bmff() {
        let dir = test_tmp_dir("ms6_validate");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mp4.heic");
        // ftyp present but with mp42 brand — valid ISO-BMFF, not HEIF.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0x18_u32.to_be_bytes());
        bytes.extend_from_slice(b"ftyp");
        bytes.extend_from_slice(b"mp42");
        bytes.extend_from_slice(&0_u32.to_be_bytes());
        bytes.extend_from_slice(b"mp42");
        bytes.extend_from_slice(b"isom");
        fs::write(&path, &bytes).unwrap();
        let mut f = fs::File::open(&path).unwrap();
        let err = validate_heif_post_rewrite(&mut f, &path).unwrap_err();
        assert!(err.to_string().contains("ftyp/HEIF brand"), "msg: {err}");
        fs::remove_file(&path).ok();
    }

    #[test]
    fn apply_metadata_heic_rating_and_title() {
        let dir = test_tmp_dir("meta_heic_tests");
        let path = fresh_heic(&dir, "rating.heic");
        apply_metadata_with_default_suffix(
            &path,
            &MetadataWrite {
                rating: Some(5),
                title: Some("Vacation".into()),
                keywords: vec!["beach".into()],
                ..MetadataWrite::default()
            },
        )
        .expect("HEIC metadata write");
        let meta = read_heif_meta(&fs::read(&path).unwrap());
        assert_eq!(meta.property_i32(xmp_ns::XMP, "Rating").unwrap().value, 5);
        assert_eq!(
            meta.localized_text(xmp_ns::DC, "title", None, "x-default")
                .unwrap()
                .0
                .value,
            "Vacation"
        );
        assert_eq!(
            meta.property_array(xmp_ns::DC, "subject")
                .map(|value| value.value)
                .collect::<Vec<_>>(),
            vec!["beach"]
        );
        fs::remove_file(&path).ok();
    }

    #[test]
    fn apply_metadata_heic_gps_roundtrips() {
        let dir = test_tmp_dir("meta_heic_tests");
        let path = fresh_heic(&dir, "gps.heic");
        apply_metadata_with_default_suffix(
            &path,
            &MetadataWrite {
                gps: Some(GpsCoords {
                    latitude: 37.7749,
                    longitude: -122.4194,
                    altitude: Some(17.0),
                }),
                ..MetadataWrite::default()
            },
        )
        .expect("HEIC GPS metadata write");
        let meta = read_heif_meta(&fs::read(&path).unwrap());
        assert!(meta.property(xmp_ns::EXIF, "GPSLatitude").is_some());
        assert!(meta.property(xmp_ns::EXIF, "GPSLongitude").is_some());
        fs::remove_file(&path).ok();
    }

    #[test]
    fn apply_metadata_heic_preserves_image_data() {
        let dir = test_tmp_dir("meta_heic_tests");
        let path = fresh_heic(&dir, "preserve.heic");
        let source = fs::read(&path).unwrap();
        apply_metadata_with_default_suffix(
            &path,
            &MetadataWrite {
                rating: Some(3),
                ..MetadataWrite::default()
            },
        )
        .expect("HEIC rating metadata write");
        let rewritten = fs::read(&path).unwrap();
        assert_eq!(
            find_mdat_bytes(&source),
            find_mdat_bytes(&rewritten),
            "HEIC image payload must remain byte-for-byte stable"
        );

        fs::remove_file(&path).ok();
    }

    #[test]
    fn apply_metadata_avif_inserts_xmp_and_preserves_image_data() {
        let dir = test_tmp_dir("meta_heic_tests");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("white_1x1.avif");
        fs::write(&path, SAMPLE_AVIF).unwrap();
        let source = fs::read(&path).unwrap();

        apply_metadata_with_default_suffix(
            &path,
            &MetadataWrite {
                rating: Some(4),
                title: Some("White pixel".into()),
                ..MetadataWrite::default()
            },
        )
        .expect("AVIF metadata write");

        let rewritten = fs::read(&path).unwrap();
        let meta = read_heif_meta(&rewritten);
        assert_eq!(meta.property_i32(xmp_ns::XMP, "Rating").unwrap().value, 4);
        assert_eq!(
            meta.localized_text(xmp_ns::DC, "title", None, "x-default")
                .unwrap()
                .0
                .value,
            "White pixel"
        );
        heif::validate_rewrite_preserves_non_xmp_items(&source, &rewritten)
            .expect("AVIF image payload and item metadata must remain byte-for-byte stable");
        fs::remove_file(&path).ok();
    }

    /// Existing XMP is layered with the requested fields without changing the
    /// HEIC item count or image payload.
    #[test]
    fn apply_metadata_heic_preserves_existing_xmp() {
        let dir = test_tmp_dir("meta_heic_tests");
        let path = write_seeded_heic(
            &dir,
            "preserve_xmp.heic",
            &MetadataWrite {
                title: Some("First".into()),
                ..MetadataWrite::default()
            },
        );
        apply_metadata_with_default_suffix(
            &path,
            &MetadataWrite {
                rating: Some(4),
                ..MetadataWrite::default()
            },
        )
        .expect("HEIC metadata update");

        let rewritten = fs::read(&path).unwrap();
        let meta = read_heif_meta(&rewritten);
        assert_eq!(
            meta.localized_text(xmp_ns::DC, "title", None, "x-default")
                .unwrap()
                .0
                .value,
            "First"
        );
        assert_eq!(meta.property_i32(xmp_ns::XMP, "Rating").unwrap().value, 4);
        assert_eq!(count_xmp_items_in_heic(&rewritten), 1);
        fs::remove_file(&path).ok();
    }

    /// An iOS HDR capture carries an XMP packet per image. Writing the
    /// photograph's metadata into an auxiliary image's packet would attach it
    /// to a gain map, and no later read could tell.
    #[test]
    fn apply_metadata_heic_writes_past_auxiliary_image_xmp() {
        const AUX_XMP: &[u8] = b"<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF><rdf:Description xmlns:HDRGainMap='http://ns.apple.com/HDRGainMap/1.0/' HDRGainMap:HDRGainMapHeadroom='2.67'/></rdf:RDF></x:xmpmeta>";
        let dir = test_tmp_dir("meta_heic_tests");
        fs::create_dir_all(&dir).unwrap();
        let seed = build_xmp_packet(&MetadataWrite {
            title: Some("First".into()),
            ..MetadataWrite::default()
        })
        .expect("seed XMP packet");

        for (name, primary) in [
            ("aux_and_primary_xmp.heic", Some(seed.as_slice())),
            ("aux_xmp_only.heic", None),
        ] {
            let path = dir.join(name);
            fs::write(&path, heif::apple_multi_xmp_heic(primary, AUX_XMP)).unwrap();

            apply_metadata_with_default_suffix(
                &path,
                &MetadataWrite {
                    rating: Some(4),
                    ..MetadataWrite::default()
                },
            )
            .expect("HEIC metadata update");

            let rewritten = fs::read(&path).unwrap();
            let packets = xmp_packets_in_heic(&rewritten);
            assert_eq!(packets.len(), 2, "{name} must keep both XMP items");
            assert!(
                packets.iter().any(|(_, packet)| packet == AUX_XMP),
                "{name} must leave the auxiliary image's packet byte-for-byte"
            );
            let photo = packets
                .iter()
                .find(|(_, packet)| packet != AUX_XMP)
                .map(|(_, packet)| packet.clone())
                .expect("photograph's packet");
            let meta: XmpMeta = std::str::from_utf8(&photo).unwrap().parse().unwrap();
            assert_eq!(meta.property_i32(xmp_ns::XMP, "Rating").unwrap().value, 4);
            if primary.is_some() {
                assert_eq!(
                    meta.localized_text(xmp_ns::DC, "title", None, "x-default")
                        .unwrap()
                        .0
                        .value,
                    "First",
                    "{name} must merge into the photograph's own packet"
                );
            }
            fs::remove_file(&path).ok();
        }
    }

    #[test]
    fn apply_metadata_heic_preserves_fixture_with_seeded_xmp_item() {
        let dir = test_tmp_dir("meta_heic_tests");
        let path = write_seeded_heic(
            &dir,
            "seeded_xmp.heic",
            &MetadataWrite {
                description: Some("Original iOS caption".into()),
                people: vec!["Casey".into()],
                media_subtype: Some("portrait".into()),
                ..MetadataWrite::default()
            },
        );
        let source = fs::read(&path).unwrap();
        apply_metadata_with_default_suffix(
            &path,
            &MetadataWrite {
                rating: Some(5),
                title: Some("Kei rewrite".into()),
                keywords: vec!["Favorites".into()],
                ..MetadataWrite::default()
            },
        )
        .expect("HEIC metadata update");

        let rewritten = fs::read(&path).unwrap();
        let meta = read_heif_meta(&rewritten);
        assert_eq!(meta.property_i32(xmp_ns::XMP, "Rating").unwrap().value, 5);
        assert_eq!(
            meta.localized_text(xmp_ns::DC, "description", None, "x-default")
                .unwrap()
                .0
                .value,
            "Original iOS caption"
        );
        assert_eq!(
            meta.localized_text(xmp_ns::DC, "title", None, "x-default")
                .unwrap()
                .0
                .value,
            "Kei rewrite"
        );
        assert_eq!(
            meta.property_array(xmp_ns::DC, "subject")
                .map(|value| value.value)
                .collect::<Vec<_>>(),
            vec!["Favorites"]
        );
        assert_eq!(
            meta.property_array(xmp_ns::IPTC_EXT, "PersonInImage")
                .map(|value| value.value)
                .collect::<Vec<_>>(),
            vec!["Casey"]
        );
        assert!(meta.property(KEI_XMP_NS, "mediaSubtype").is_some());
        assert_eq!(
            count_xmp_items_in_heic(&rewritten),
            1,
            "repeat HEIC metadata updates must retain one XMP item"
        );
        assert_eq!(
            find_mdat_bytes(&source),
            find_mdat_bytes(&rewritten),
            "HEIC image data must remain byte-for-byte stable"
        );
        fs::remove_file(&path).ok();
    }

    #[test]
    fn apply_metadata_heic_failure_leaves_media_bytes_intact() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing_meta.heic");
        let original = heif_ftyp_without_meta_bytes();
        fs::write(&path, &original).unwrap();

        let result = apply_metadata_with_default_suffix(
            &path,
            &MetadataWrite {
                rating: Some(3),
                ..MetadataWrite::default()
            },
        );
        assert!(
            result.is_err(),
            "HEIC files without a meta box must be rejected"
        );
        assert_eq!(
            fs::read(&path).unwrap(),
            original,
            "rejected HEIC rewrites must leave original media bytes untouched"
        );
        assert!(
            embed_temp_entries(dir.path(), ".meta-tmp").is_empty(),
            "rejected HEIC rewrites must clean the metadata temp file"
        );
    }

    #[test]
    fn apply_metadata_heic_rejects_conflicting_tone_map_xmp_ownership() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("conflicting_tone_map_xmp.heic");
        let original = heif::apple_tmap_conflicting_xmp_heic(
            &crate::test_helpers::minimal_tiff_with_source_gps(),
            b"<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF><rdf:Description xmlns:HDRGainMap='http://ns.apple.com/HDRGainMap/1.0/' HDRGainMap:HDRGainMapHeadroom='2.67'/></rdf:RDF></x:xmpmeta>",
        );
        fs::write(&path, &original).unwrap();

        let result = apply_metadata_with_default_suffix(
            &path,
            &MetadataWrite {
                rating: Some(3),
                ..MetadataWrite::default()
            },
        );

        assert!(result.is_err());
        assert_eq!(
            fs::read(&path).unwrap(),
            original,
            "conflicting XMP ownership must leave original media bytes untouched"
        );
        assert!(
            embed_temp_entries(dir.path(), ".meta-tmp").is_empty(),
            "a refused rewrite must clean its uniquely owned temp file"
        );
    }

    #[test]
    fn apply_metadata_heic_install_failure_leaves_original_intact_and_cleans_temp() {
        let dir = tempfile::tempdir().unwrap();
        let path = fresh_heic(dir.path(), "install_fault.heic");
        let original = fs::read(&path).unwrap();

        // Inject a failure at the exact boundary where the temp file has been
        // written, fsynced and validated, but the atomic install has not yet
        // renamed over the original.
        let install_calls = std::cell::Cell::new(0u32);
        let prepared = prepare_metadata_heif(
            &path,
            &MetadataWrite {
                rating: Some(5),
                ..MetadataWrite::default()
            },
            ".meta-tmp",
            None,
        )
        .unwrap();
        let result = prepared.publish_with(&path, |src, _dst, _expected, _expected_replacement| {
            install_calls.set(install_calls.get() + 1);
            assert!(src.exists(), "temp file must exist at the install boundary");
            Err(std::io::Error::other("simulated crash before publication").into())
        });

        assert!(
            result.is_err(),
            "an install failure must surface as an error"
        );
        assert_eq!(
            install_calls.get(),
            1,
            "the install boundary must be reached"
        );
        assert_eq!(
            fs::read(&path).unwrap(),
            original,
            "a crash before publication must leave the original byte-identical"
        );
        assert!(
            embed_temp_entries(dir.path(), ".meta-tmp").is_empty(),
            "a failure before exchange must clean its uniquely owned temp file"
        );
    }

    #[test]
    fn contract_heif_embed_rewrite_requires_stable_input() {
        let dir = tempfile::tempdir().unwrap();
        let path = fresh_heic(dir.path(), "concurrent_edit.heic");
        let external = b"external edit at the install boundary";

        let prepared = prepare_metadata_heif(
            &path,
            &MetadataWrite {
                rating: Some(5),
                ..MetadataWrite::default()
            },
            ".meta-tmp",
            None,
        )
        .unwrap();
        let result = prepared.publish_with(&path, |src, dst, expected, expected_replacement| {
            fs::write(dst, external)?;
            crate::download::file::publish_file_if_unchanged_blocking(
                src,
                dst,
                expected,
                expected_replacement,
            )
        });

        let error = result.expect_err("a concurrent edit must block HEIC publication");
        assert!(
            crate::download::file::classify_conditional_publish_error(&error).target_changed,
            "the caller must preserve checksum evidence for a concurrent edit"
        );
        let error_chain = format!("{error:#}");
        assert!(error_chain.contains("bytes changed"), "{error_chain}");
        assert_eq!(
            fs::read(&path).unwrap(),
            external,
            "a concurrent edit must not be overwritten by the stale HEIC rewrite"
        );
        assert!(
            embed_temp_entries(dir.path(), ".meta-tmp").is_empty(),
            "a refused pre-exchange publication must clean its uniquely owned temp file"
        );
    }

    #[test]
    fn apply_metadata_heic_refuses_temp_changed_after_validation() {
        let dir = tempfile::tempdir().unwrap();
        let path = fresh_heic(dir.path(), "changed_temp.heic");
        let original = fs::read(&path).unwrap();

        let prepared = prepare_metadata_heif(
            &path,
            &MetadataWrite {
                rating: Some(5),
                ..MetadataWrite::default()
            },
            ".meta-tmp",
            None,
        )
        .unwrap();
        let error = prepared
            .publish_with(&path, |src, dst, expected, expected_replacement| {
                fs::write(src, b"unapproved temporary bytes")?;
                crate::download::file::publish_file_if_unchanged_blocking(
                    src,
                    dst,
                    expected,
                    expected_replacement,
                )
            })
            .expect_err("temporary bytes changed after validation must not be published");

        assert_eq!(fs::read(&path).unwrap(), original);
        let retained = embed_temp_entries(dir.path(), ".meta-tmp");
        assert_eq!(retained.len(), 1);
        let disposition = crate::download::file::classify_conditional_publish_error(&error);
        assert!(disposition.retained_paths.contains(&retained[0]));
        fs::remove_file(&retained[0]).unwrap();
    }

    #[test]
    fn apply_metadata_heic_refuses_change_after_approved_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let path = fresh_heic(dir.path(), "changed_before_read.heic");
        let original = fs::read(&path).unwrap();
        let approved = fingerprint_bytes(&original).unwrap();
        let changed = heic_with_xmp_packet(
            &build_xmp_packet(&MetadataWrite {
                title: Some("External edit".into()),
                ..MetadataWrite::default()
            })
            .unwrap(),
        );
        fs::write(&path, &changed).unwrap();

        let error = apply_metadata_with_expected_fingerprint(
            &path,
            &MetadataWrite {
                rating: Some(5),
                ..MetadataWrite::default()
            },
            ".meta-tmp",
            Some(approved),
        )
        .expect_err("bytes changed after retry approval must not be rewritten");

        assert!(crate::download::file::classify_conditional_publish_error(&error).target_changed);
        assert_eq!(fs::read(&path).unwrap(), changed);
        assert!(embed_temp_entries(dir.path(), ".meta-tmp").is_empty());
    }

    #[test]
    fn apply_metadata_heic_refuses_format_change_after_approved_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let path = fresh_heic(dir.path(), "changed_format.heic");
        let approved = fingerprint_bytes(&fs::read(&path).unwrap()).unwrap();
        let changed = minimal_jpeg();
        fs::write(&path, &changed).unwrap();

        let error = apply_metadata_with_expected_fingerprint(
            &path,
            &MetadataWrite {
                rating: Some(5),
                ..MetadataWrite::default()
            },
            ".meta-tmp",
            Some(approved),
        )
        .expect_err("a format change after retry approval must not be rewritten");

        assert!(crate::download::file::classify_conditional_publish_error(&error).target_changed);
        assert_eq!(fs::read(&path).unwrap(), changed);
        assert!(!temp_path_for(&path, ".meta-tmp").exists());
        assert!(embed_temp_entries(dir.path(), ".meta-tmp").is_empty());
    }

    #[test]
    fn apply_metadata_heic_returns_installed_output_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let path = fresh_heic(dir.path(), "output_fingerprint.heic");
        let approved = fingerprint_bytes(&fs::read(&path).unwrap()).unwrap();

        let output = apply_metadata_with_expected_fingerprint(
            &path,
            &MetadataWrite {
                rating: Some(5),
                ..MetadataWrite::default()
            },
            ".meta-tmp",
            Some(approved),
        )
        .expect("HEIC metadata write")
        .expect("HEIC writes return their installed fingerprint");

        assert_eq!(
            fingerprint_bytes(&fs::read(&path).unwrap()).unwrap(),
            output
        );
    }

    #[test]
    fn apply_metadata_heic_temp_preserves_readonly_permission() {
        let dir = tempfile::tempdir().unwrap();
        let path = fresh_heic(dir.path(), "readonly.heic");
        let original_permissions = fs::metadata(&path).unwrap().permissions();
        let mut readonly_permissions = original_permissions.clone();
        readonly_permissions.set_readonly(true);
        fs::set_permissions(&path, readonly_permissions).unwrap();

        let prepared = prepare_metadata_heif(
            &path,
            &MetadataWrite {
                rating: Some(5),
                ..MetadataWrite::default()
            },
            ".meta-tmp",
            None,
        )
        .unwrap();
        let result = prepared.publish_with(&path, |src, _dst, _expected, _expected_replacement| {
            assert!(
                fs::metadata(src).unwrap().permissions().readonly(),
                "the validated temp file must carry the source read-only permission"
            );
            Err(std::io::Error::other("stop after permission check").into())
        });

        assert!(result.is_err(), "the injected install failure must surface");
        fs::set_permissions(&path, original_permissions).unwrap();
        assert!(embed_temp_entries(dir.path(), ".meta-tmp").is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn apply_metadata_heic_preserves_unix_mode() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = fresh_heic(dir.path(), "mode.heic");
        let original_permissions = fs::metadata(&path).unwrap().permissions();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();

        apply_metadata_with_default_suffix(
            &path,
            &MetadataWrite {
                rating: Some(5),
                ..MetadataWrite::default()
            },
        )
        .expect("HEIC metadata write");

        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640,
            "the installed file must preserve the source mode"
        );
        fs::set_permissions(&path, original_permissions).unwrap();
        assert!(
            embed_temp_entries(dir.path(), ".meta-tmp").is_empty(),
            "the mode check must not leave a temp file"
        );
    }

    #[test]
    fn apply_metadata_heic_is_idempotent_on_rewrite() {
        let dir = test_tmp_dir("meta_heic_tests");
        let path = fresh_heic(&dir, "idempotent.heic");
        let write = MetadataWrite {
            rating: Some(4),
            title: Some("Repeat".into()),
            ..MetadataWrite::default()
        };
        let original = fs::read(&path).unwrap();

        apply_metadata_with_default_suffix(&path, &write).unwrap();
        let first = fs::read(&path).unwrap();
        apply_metadata_with_default_suffix(&path, &write).unwrap();
        let second = fs::read(&path).unwrap();

        assert_ne!(
            first, original,
            "HEIC metadata write must update the XMP item"
        );
        assert_eq!(
            second, first,
            "repeated HEIC metadata writes must stay idempotent"
        );
        fs::remove_file(&path).ok();
    }

    /// Each write merges into the packet the reader resolved, so unrelated
    /// fields must survive. A shrinking packet also reuses the existing extent
    /// and pads the tail, which is the layout the post-write identity check has
    /// to tolerate.
    #[test]
    fn apply_metadata_heic_changes_one_field_without_rewriting_other_xmp() {
        let dir = test_tmp_dir("meta_heic_tests");
        let path = write_seeded_heic(
            &dir,
            "change_rating.heic",
            &MetadataWrite {
                rating: Some(5),
                title: Some("Keep this".into()),
                keywords: vec![
                    "alpha".into(),
                    "beta".into(),
                    "gamma".into(),
                    "delta".into(),
                ],
                ..MetadataWrite::default()
            },
        );
        apply_metadata_with_default_suffix(
            &path,
            &MetadataWrite {
                rating: Some(2),
                ..MetadataWrite::default()
            },
        )
        .expect("HEIC rating change");
        let meta = read_heif_meta(&fs::read(&path).unwrap());
        assert_eq!(meta.property_i32(xmp_ns::XMP, "Rating").unwrap().value, 2);
        assert_eq!(
            meta.localized_text(xmp_ns::DC, "title", None, "x-default")
                .unwrap()
                .0
                .value,
            "Keep this"
        );
        apply_metadata_with_default_suffix(
            &path,
            &MetadataWrite {
                rating: Some(0),
                ..MetadataWrite::default()
            },
        )
        .expect("HEIC rating clear");
        let cleared = read_heif_meta(&fs::read(&path).unwrap());
        assert_eq!(
            cleared.property_i32(xmp_ns::XMP, "Rating").unwrap().value,
            0
        );

        let before = extract_xmp_from_heic(&fs::read(&path).unwrap()).expect("packet before");
        apply_metadata_with_default_suffix(
            &path,
            &MetadataWrite {
                keywords: vec!["alpha".into()],
                ..MetadataWrite::default()
            },
        )
        .expect("HEIC keyword removal");
        let after = extract_xmp_from_heic(&fs::read(&path).unwrap()).expect("packet after");
        assert_eq!(
            after.len(),
            before.len(),
            "a shrinking packet must reuse the existing extent"
        );
        assert!(
            after.ends_with(b" "),
            "the reused extent must be padded to its original length"
        );
        let shrunk = read_heif_meta(&fs::read(&path).unwrap());
        assert_eq!(
            shrunk
                .localized_text(xmp_ns::DC, "title", None, "x-default")
                .unwrap()
                .0
                .value,
            "Keep this",
            "removing a keyword must not disturb the rest of the packet"
        );
        assert_eq!(
            shrunk.property_array(xmp_ns::DC, "subject").count(),
            1,
            "stale keywords must not accumulate"
        );
        fs::remove_file(&path).ok();
    }

    // ── Regression: HEIC writes must work on part-file paths (issue #552) ──
    //
    // The download pipeline writes embedded metadata onto the `<base32>.kei-tmp`
    // part file before the atomic rename to the final `.HEIC` name. Content
    // sniffing must route that extension-shadowed part file to the HEIF writer.

    #[test]
    fn apply_metadata_skips_heif_on_extension_less_part_file() {
        let dir = test_tmp_dir("meta_heic_tests");
        fs::create_dir_all(&dir).unwrap();
        // Mimic the download part-file: base32-ish stem with `.kei-tmp`
        // suffix shadowing the real `.heic` extension.
        let path = dir.join("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA.kei-tmp");
        fs::write(&path, SAMPLE_HEIC).unwrap();
        apply_metadata_with_default_suffix(
            &path,
            &MetadataWrite {
                rating: Some(4),
                title: Some("PartFile".into()),
                ..MetadataWrite::default()
            },
        )
        .expect("HEIC metadata write must succeed on .kei-tmp part file");
        let meta = read_heif_meta(&fs::read(&path).unwrap());
        assert_eq!(meta.property_i32(xmp_ns::XMP, "Rating").unwrap().value, 4);
        assert!(
            !temp_path_for(&path, ".meta-tmp").exists(),
            "extension-shadowed HEIC part files must not leave a temp file"
        );
        fs::remove_file(&path).ok();
    }

    // ── Regression: iOS 17+ HEICs with `uri ` infe items (issue #274) ──
    //
    // Apple started embedding `uri ` item-info entries (item_type="uri ",
    // item_uri_type="tag:apple.com,2023:photos/<id>") in HEICs from iOS 17.
    // The mp4-atom 0.10.1 release didn't read the trailing `item_uri_type`
    // cstr, so its strict end-check rejected every such infe entry as
    // `UnderDecode("infe")` — which surfaced to users as the metadata-embed
    // pass failing on every iOS 17+ HEIC. The fix lives on mp4-atom's `main`
    // (PR #123, 2026-01-26); kei pins past that commit. This test pins the
    // round-trip so a future bump that drops the `uri ` decoder regresses
    // visibly rather than reverting silently.

    #[test]
    fn apply_metadata_succeeds_on_heic_with_uri_infe_item() {
        use mp4_atom::{Any, DecodeMaybe, Encode, FourCC, Iinf, Iloc, ItemInfoEntry};

        // Build a HEIC variant that carries a `uri ` infe entry by parsing
        // the sample, injecting a synthetic Apple-style entry into iinf,
        // and re-serializing. This produces bytes byte-shape-identical to
        // what an iOS 17+ camera writes for the failing case.
        let mut atoms: Vec<Any> = Vec::new();
        let mut cursor: &[u8] = SAMPLE_HEIC;
        while let Some(atom) = Any::decode_maybe(&mut cursor).expect("sample HEIC must parse") {
            atoms.push(atom);
        }
        let meta = atoms
            .iter_mut()
            .find_map(|a| if let Any::Meta(m) = a { Some(m) } else { None })
            .expect("sample HEIC has a meta box");
        let iinf = meta
            .get_mut::<Iinf>()
            .expect("sample HEIC has an iinf inside meta");
        iinf.item_infos.push(ItemInfoEntry {
            item_id: 9999,
            item_protection_index: 0,
            item_type: Some(FourCC::new(b"uri ")),
            item_name: "metadata".to_string(),
            content_type: None,
            content_encoding: None,
            item_uri_type: Some("tag:apple.com,2023:photos/UNIT-TEST".to_string()),
            item_not_in_presentation: false,
        });
        let mut bytes: Vec<u8> = Vec::new();
        for atom in &atoms {
            atom.encode(&mut bytes)
                .expect("re-encode of synthetic uri-bearing HEIC must succeed");
        }

        // mp4-atom writes iloc offsets verbatim, so adding the uri entry grows
        // meta and shifts the mdat without repointing the item offsets. Repoint
        // every item at the re-encoded mdat so the fixture stays a valid HEIC
        // whose image data lives after meta, not inside it.
        let mdat_start = {
            let mut pos = 0usize;
            loop {
                let size = u32::from_be_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
                if &bytes[pos + 4..pos + 8] == b"mdat" {
                    break pos as u64;
                }
                pos += size;
            }
        };
        let meta = atoms
            .iter_mut()
            .find_map(|a| if let Any::Meta(m) = a { Some(m) } else { None })
            .expect("sample HEIC has a meta box");
        let iloc = meta
            .get_mut::<Iloc>()
            .expect("sample HEIC has an iloc inside meta");
        let min_base = iloc
            .item_locations
            .iter()
            .map(|loc| loc.base_offset)
            .min()
            .expect("iloc has at least one item");
        let shift = (mdat_start + 8) - min_base;
        for loc in &mut iloc.item_locations {
            loc.base_offset += shift;
        }
        let mut bytes: Vec<u8> = Vec::new();
        for atom in &atoms {
            atom.encode(&mut bytes)
                .expect("re-encode of repointed uri-bearing HEIC must succeed");
        }

        let dir = test_tmp_dir("meta_heic_tests");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("uri_infe.heic");
        fs::write(&path, &bytes).unwrap();

        apply_metadata_with_default_suffix(
            &path,
            &MetadataWrite {
                rating: Some(3),
                title: Some("UriItem".into()),
                ..MetadataWrite::default()
            },
        )
        .expect("HEIC metadata write must preserve a `uri ` infe item");

        let rewritten = fs::read(&path).unwrap();
        let meta = read_heif_meta(&rewritten);
        assert_eq!(meta.property_i32(xmp_ns::XMP, "Rating").unwrap().value, 3);
        assert_eq!(
            find_mdat_bytes(&bytes),
            find_mdat_bytes(&rewritten),
            "HEIC image data must survive a `uri ` item update"
        );
        fs::remove_file(&path).ok();
    }
    fn read_heif_meta(bytes: &[u8]) -> XmpMeta {
        let xmp = extract_xmp_from_heic(bytes).expect("HEIF XMP missing");
        std::str::from_utf8(&xmp)
            .expect("HEIF XMP is not UTF-8")
            .parse()
            .expect("HEIF XMP is not valid")
    }
    /// The first XMP packet in a HEIC file. Single-packet fixtures only; use
    /// [`xmp_packets_in_heic`] where several images each carry one.
    fn extract_xmp_from_heic(bytes: &[u8]) -> Option<Vec<u8>> {
        xmp_packets_in_heic(bytes)
            .into_iter()
            .next()
            .map(|(_, packet)| packet)
    }
    /// Walk a HEIC file's top-level atoms and return every XMP packet with its
    /// item ID. The write path puts XMP in a trailing `mdat`; the iloc entry is
    /// construction_method=0 with a file-absolute offset, so we slice the
    /// file bytes directly. Parsed independently of kei's own reader so a test
    /// cannot confirm the reader against itself.
    fn xmp_packets_in_heic(bytes: &[u8]) -> Vec<(u32, Vec<u8>)> {
        use mp4_atom::{Any, DecodeMaybe, FourCC, Iinf, Iloc};
        let mut cursor: &[u8] = bytes;
        while let Ok(Some(atom)) = Any::decode_maybe(&mut cursor) {
            let Any::Meta(meta) = atom else {
                continue;
            };
            let (Some(iinf), Some(iloc)) = (meta.get::<Iinf>(), meta.get::<Iloc>()) else {
                return Vec::new();
            };
            return iinf
                .item_infos
                .iter()
                .filter(|e| {
                    e.item_type == Some(FourCC::new(b"mime"))
                        && e.content_type.as_deref() == Some("application/rdf+xml")
                })
                .filter_map(|entry| {
                    let loc = iloc
                        .item_locations
                        .iter()
                        .find(|l| l.item_id == entry.item_id)?;
                    if loc.construction_method != 0 {
                        return None;
                    }
                    let extent = loc.extents.first()?;
                    let start = loc.base_offset.saturating_add(extent.offset) as usize;
                    let end = start + extent.length as usize;
                    Some((entry.item_id, bytes.get(start..end)?.to_vec()))
                })
                .collect();
        }
        Vec::new()
    }
    fn count_xmp_items_in_heic(bytes: &[u8]) -> usize {
        use mp4_atom::{Any, DecodeMaybe, FourCC, Iinf};
        let mut cursor: &[u8] = bytes;
        while let Ok(Some(atom)) = Any::decode_maybe(&mut cursor) {
            if let Any::Meta(meta) = atom
                && let Some(iinf) = meta.get::<Iinf>()
            {
                return iinf
                    .item_infos
                    .iter()
                    .filter(|e| {
                        e.item_type == Some(FourCC::new(b"mime"))
                            && e.content_type.as_deref() == Some("application/rdf+xml")
                    })
                    .count();
            }
        }
        0
    }
    /// Locate the raw `mdat` box payload bytes in a HEIC file. Used to prove
    /// that the image data didn't change when we modified metadata.
    fn find_mdat_bytes(bytes: &[u8]) -> Option<Vec<u8>> {
        // `mdat` is one of the atoms the `mp4-atom::Any` decoder recognises.
        use mp4_atom::{Any, DecodeMaybe, Encode};
        let mut cursor: &[u8] = bytes;
        while let Ok(Some(atom)) = Any::decode_maybe(&mut cursor) {
            if let Any::Mdat(_) = &atom {
                // Re-encode so the test compares the full box bytes (header + body).
                let mut buf = Vec::new();
                atom.encode(&mut buf).ok()?;
                return Some(buf);
            }
        }
        None
    }
}
