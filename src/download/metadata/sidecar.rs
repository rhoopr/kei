//! Sidecar ownership, preparation, and conditional publication.

use super::prepared::{TmpGuard, fingerprint_bytes};
use super::values::MetadataWrite;
use super::xmp_fields::{
    KEI_MANAGED_FIELDS, KEI_XMP_NS, apply_to_owned_sidecar, ensure_initialized,
};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use xmp_toolkit::{FromStrOptions, XmpErrorType, XmpMeta};

static SIDECAR_TEMP_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

struct ReconciledSidecarSnapshot {
    path: crate::fs_util::ConfinedPath,
    file: std::fs::File,
    bytes: Vec<u8>,
}

impl ReconciledSidecarSnapshot {
    fn read(path: crate::fs_util::ConfinedPath) -> Result<Option<Self>> {
        let Some(mut file) = path.open_optional_regular()? else {
            return Ok(None);
        };
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut file, &mut bytes)?;
        path.validate_identity(crate::fs_util::file_identity(&file)?)?;
        Ok(Some(Self { path, file, bytes }))
    }

    fn validate(&self) -> Result<()> {
        let identity = crate::fs_util::file_identity(&self.file)?;
        let mut current = self.path.validate_identity(identity)?;
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut current, &mut bytes)?;
        anyhow::ensure!(
            bytes == self.bytes,
            "Reconciled sidecar changed before state finalization"
        );
        self.path.validate_identity(identity)?;
        Ok(())
    }
}

/// Keep source and destination sidecar evidence live through catalogue finalization.
pub(in crate::download) struct ReconciledSidecar {
    source_path: crate::fs_util::ConfinedPath,
    source: Option<ReconciledSidecarSnapshot>,
    destination: ReconciledSidecarSnapshot,
}

impl ReconciledSidecar {
    pub(in crate::download) async fn validate(self: &Arc<Self>) -> Result<()> {
        let sidecar = Arc::clone(self);
        tokio::task::spawn_blocking(move || sidecar.validate_blocking()).await?
    }

    fn validate_blocking(&self) -> Result<()> {
        if let Some(source) = &self.source {
            source.validate()?;
        } else {
            anyhow::ensure!(
                self.source_path.open_optional_regular()?.is_none(),
                "Source sidecar appeared during reconciliation"
            );
        }
        self.destination.validate()
    }
}

/// Preserve an existing packet byte-for-byte. Generate a configured packet only
/// when no source sidecar exists. Never overwrite a conflicting destination.
pub(in crate::download) fn write_reconciled_sidecar(
    copy: &crate::download::file::ReconciledFile,
    write_missing: impl FnOnce() -> Result<MetadataWrite>,
    temp_suffix: &str,
) -> Result<Arc<ReconciledSidecar>> {
    fn sidecar_name(path: &Path) -> Result<PathBuf> {
        let mut name = path
            .file_name()
            .context("Reconciled media has no filename")?
            .to_os_string();
        name.push(".xmp");
        Ok(path.with_file_name(name))
    }
    let source_path = copy.source.sibling(&sidecar_name(copy.source.path())?)?;
    // Sibling creates a second retained capability without resolving ancestors.
    let source = ReconciledSidecarSnapshot::read(copy.source.sibling(source_path.path())?)?;
    let bytes = if let Some(source) = &source {
        ensure_initialized();
        parse_existing_sidecar(
            std::str::from_utf8(&source.bytes).context("Source sidecar is not UTF-8")?,
        )?;
        source.bytes.clone()
    } else {
        let write = write_missing()?;
        ensure_initialized();
        let mut meta = XmpMeta::new().context("creating reconciled XMP packet")?;
        apply_to_owned_sidecar(&mut meta, &write)?;
        meta.to_string().into_bytes()
    };
    if let Some(source) = &source {
        source.validate()?;
    }
    copy.validate_blocking()?;
    let destination = copy
        .destination
        .sibling(&sidecar_name(copy.destination.path())?)?;
    // CONTRACT: FILE_PUBLISH_NO_OVERWRITE
    let existing = ReconciledSidecarSnapshot::read(copy.destination.sibling(destination.path())?)?;
    let destination = if let Some(existing) = existing {
        anyhow::ensure!(
            existing.bytes == bytes,
            "Reconciled sidecar destination conflicts with source metadata"
        );
        existing
    } else {
        let temp_path = destination.path().with_file_name(format!(
            ".kei-xmp-reconcile-{}{temp_suffix}",
            uuid::Uuid::new_v4()
        ));
        let temp = destination.sibling(&temp_path)?;
        let mut file = temp.create_new_regular()?;
        std::io::Write::write_all(&mut file, &bytes)?;
        file.sync_all()?;
        temp.validate_identity(crate::fs_util::file_identity(&file)?)?;
        crate::download::file::publish_reconciliation_part_blocking(&temp, &destination)?;
        let installed = ReconciledSidecarSnapshot::read(destination)?
            .context("Reconciled sidecar disappeared")?;
        anyhow::ensure!(
            installed.bytes == bytes,
            "Reconciled sidecar changed during publication"
        );
        installed
    };
    destination.path.sync_parent()?;
    let receipt = Arc::new(ReconciledSidecar {
        source_path,
        source,
        destination,
    });
    receipt.validate_blocking()?;
    Ok(receipt)
}

/// Write `write` as a `.xmp` sidecar next to the media file, atomically.
///
/// If a sidecar already exists (e.g., from Darktable / Lightroom / digiKam),
/// its existing XMP properties are read and kei's fields are layered on top
/// rather than overwriting the whole packet. Existing bytes must remain
/// readable, parseable, and unchanged through publication. Otherwise the
/// write fails without replacing them.
pub(crate) async fn write_sidecar(
    media_path: &Path,
    write: &MetadataWrite,
    temp_suffix: &str,
) -> Result<()> {
    let media_path = media_path.to_path_buf();
    let write = write.clone();
    let temp_suffix = temp_suffix.to_owned();
    let prepared = tokio::task::spawn_blocking(move || {
        prepare_sidecar_write(&media_path, &write, &temp_suffix)
    })
    .await
    .context("XMP sidecar preparation task panicked")??;
    let Some(prepared) = prepared else {
        return Ok(());
    };

    // CONTRACT: XMP_SIDECAR_REWRITE_REQUIRES_STABLE_INPUT
    crate::download::file::publish_file_if_unchanged(
        &prepared.tmp_path,
        &prepared.sidecar_path,
        prepared.expected,
    )
    .await
    .with_context(|| {
        format!(
            "Could not install XMP sidecar {} -> {}",
            prepared.tmp_path.display(),
            prepared.sidecar_path.display()
        )
    })?;
    tracing::debug!(target: "kei::download::metadata", path = %prepared.sidecar_path.display(), "Wrote XMP sidecar");
    Ok(())
}

struct PreparedSidecar {
    tmp_path: PathBuf,
    sidecar_path: PathBuf,
    expected: Option<crate::download::file::ExistingFileFingerprint>,
}

fn prepare_sidecar_write(
    media_path: &Path,
    write: &MetadataWrite,
    temp_suffix: &str,
) -> Result<Option<PreparedSidecar>> {
    ensure_initialized();

    let Some(name) = media_path.file_name() else {
        if write.is_empty() {
            return Ok(None);
        }
        anyhow::bail!(
            "Cannot write an XMP sidecar because the media path has no filename: {}",
            media_path.display()
        );
    };
    let mut sidecar_name = name.to_os_string();
    sidecar_name.push(".xmp");
    let sidecar_path = media_path.with_file_name(&sidecar_name);
    // Seed the packet with any existing sidecar content so user-authored
    // ratings / keywords / develop settings from another tool survive.
    let (mut meta, expected) = match std::fs::read(&sidecar_path) {
        Ok(existing_bytes) => {
            let existing = std::str::from_utf8(&existing_bytes).with_context(|| {
                format!(
                    "Existing XMP sidecar is not valid UTF-8: {}",
                    sidecar_path.display()
                )
            })?;
            let parsed = parse_existing_sidecar(existing).with_context(|| {
                format!(
                    "Could not parse existing XMP sidecar {}",
                    sidecar_path.display()
                )
            })?;
            let fingerprint = fingerprint_bytes(&existing_bytes)?;
            (parsed, Some(fingerprint))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            (XmpMeta::new().context("creating XmpMeta")?, None)
        }
        Err(e) => {
            return Err(e).with_context(|| {
                format!(
                    "Could not read existing XMP sidecar {}",
                    sidecar_path.display()
                )
            });
        }
    };
    if write.is_empty() && !meta.contains_property(KEI_XMP_NS, KEI_MANAGED_FIELDS) {
        return Ok(None);
    }
    apply_to_owned_sidecar(&mut meta, write)?;
    let bytes = meta.to_string().into_bytes();

    let (mut temp, tmp_path) = create_unique_sidecar_temp(&sidecar_path, temp_suffix)?;
    let guard = TmpGuard::new(&tmp_path);
    std::io::Write::write_all(&mut temp, &bytes).with_context(|| {
        format!(
            "Could not write temporary XMP sidecar {}",
            tmp_path.display()
        )
    })?;
    temp.sync_all().with_context(|| {
        format!(
            "Could not sync temporary XMP sidecar {}",
            tmp_path.display()
        )
    })?;
    guard.disarm();
    Ok(Some(PreparedSidecar {
        tmp_path,
        sidecar_path,
        expected,
    }))
}

fn sidecar_temp_path(sidecar_path: &Path, temp_suffix: &str, sequence: u64) -> PathBuf {
    let parent = sidecar_path.parent().unwrap_or_else(|| Path::new("."));
    parent.join(format!(
        ".kei-xmp-{}-{sequence}{temp_suffix}",
        std::process::id()
    ))
}

fn create_unique_sidecar_temp(
    sidecar_path: &Path,
    temp_suffix: &str,
) -> Result<(std::fs::File, PathBuf)> {
    loop {
        let candidate = sidecar_temp_path(
            sidecar_path,
            temp_suffix,
            SIDECAR_TEMP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        );
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => return Ok((file, candidate)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "Could not create temporary XMP sidecar {}",
                        candidate.display()
                    )
                });
            }
        }
    }
}

fn parse_existing_sidecar(existing: &str) -> Result<XmpMeta> {
    match XmpMeta::from_str_with_options(existing, FromStrOptions::default().require_xmp_meta()) {
        Ok(parsed) => Ok(parsed),
        Err(error) if error.error_type == XmpErrorType::XmpMetaElementMissing => {
            let parsed = existing.parse::<XmpMeta>()?;
            anyhow::ensure!(
                parsed
                    .iter(xmp_toolkit::IterOptions::default())
                    .next()
                    .is_some(),
                "sidecar has no recognizable XMP properties"
            );
            Ok(parsed)
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
#[allow(clippy::unused_result_ok, reason = "test cleanup is best-effort")]
mod tests {
    use super::super::test_support::{
        test_tmp_dir, write_sidecar_with_default_suffix, xmp_rational,
    };
    use super::super::values::{GpsCoords, MetadataWrite};
    use super::super::xmp_fields::{
        EXIF_EX_XMP_NS, KEI_MANAGED_FIELDS, KEI_XMP_NS, ensure_initialized,
    };
    use super::{parse_existing_sidecar, prepare_sidecar_write, write_sidecar};
    use anyhow::Result;
    use std::fs;
    use std::path::{Path, PathBuf};
    use xmp_toolkit::{XmpMeta, XmpValue, xmp_ns};

    // ── Sidecar + format-dispatch tests ────────────────────────────────

    fn read_sidecar_meta(media_path: &Path) -> XmpMeta {
        let mut sidecar_path = media_path.as_os_str().to_os_string();
        sidecar_path.push(".xmp");
        fs::read_to_string(PathBuf::from(sidecar_path))
            .unwrap()
            .parse()
            .unwrap()
    }

    #[test]
    fn write_sidecar_is_noop_on_empty_write() {
        let dir = test_tmp_dir("sidecar_tests");
        std::fs::create_dir_all(&dir).unwrap();
        let media_path = dir.join("empty.jpg");
        std::fs::write(&media_path, b"placeholder").unwrap();
        write_sidecar_with_default_suffix(&media_path, &MetadataWrite::default()).unwrap();
        let sidecar = dir.join("empty.jpg.xmp");
        assert!(
            !sidecar.exists(),
            "empty metadata write must not create a sidecar"
        );
        fs::remove_file(&media_path).ok();
    }

    #[test]
    fn write_sidecar_creates_xmp_file_next_to_media() {
        let dir = test_tmp_dir("sidecar_tests");
        std::fs::create_dir_all(&dir).unwrap();
        let media_path = dir.join("photo.jpg");
        std::fs::write(&media_path, b"placeholder").unwrap();

        let write = MetadataWrite {
            rating: Some(5),
            title: Some("Vacation".to_string()),
            keywords: vec!["beach".into(), "sun".into()],
            people: vec!["Alice".into()],
            ..MetadataWrite::default()
        };
        write_sidecar_with_default_suffix(&media_path, &write).expect("sidecar write");
        let sidecar = dir.join("photo.jpg.xmp");
        assert!(sidecar.exists(), "sidecar should be written next to media");

        let bytes = fs::read(&sidecar).unwrap();
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(s.contains("rdf:RDF"));
        assert!(s.contains("xmp:Rating"));
        assert!(s.contains("Vacation"));
        assert!(s.contains("beach"));
        assert!(s.contains("Alice"));

        fs::remove_file(&sidecar).ok();
        fs::remove_file(&media_path).ok();
    }

    #[test]
    fn write_sidecar_is_atomic_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let media_path = dir.path().join("rewrite.jpg");
        std::fs::write(&media_path, b"placeholder").unwrap();

        let first = MetadataWrite {
            title: Some("Before".into()),
            ..MetadataWrite::default()
        };
        write_sidecar_with_default_suffix(&media_path, &first).unwrap();

        let second = MetadataWrite {
            title: Some("After".into()),
            ..MetadataWrite::default()
        };
        write_sidecar_with_default_suffix(&media_path, &second).unwrap();

        let sidecar = dir.path().join("rewrite.jpg.xmp");
        let s = fs::read_to_string(&sidecar).unwrap();
        assert!(s.contains("After"), "second write should replace first");
        assert!(
            !s.contains("Before"),
            "previous title must not leak through"
        );

        let retained_temps: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(".kei-xmp-"))
            .collect();
        assert!(
            retained_temps.is_empty(),
            "successful writes must remove their unique temp files: {retained_temps:?}"
        );
    }

    #[tokio::test]
    async fn write_sidecar_refuses_existing_file_changed_after_read() {
        let dir = tempfile::tempdir().unwrap();
        let media_path = dir.path().join("changed.jpg");
        let sidecar_path = dir.path().join("changed.jpg.xmp");
        std::fs::write(&media_path, b"placeholder").unwrap();

        ensure_initialized();
        let mut original = XmpMeta::new().unwrap();
        original
            .set_property(
                xmp_ns::DC,
                "creator",
                &XmpValue::new("Initial author".to_string()),
            )
            .unwrap();
        std::fs::write(&sidecar_path, original.to_string().into_bytes()).unwrap();

        let prepared = prepare_sidecar_write(
            &media_path,
            &MetadataWrite {
                rating: Some(4),
                ..MetadataWrite::default()
            },
            ".meta-tmp",
        )
        .unwrap()
        .expect("sidecar update should be prepared");
        let temp_path = prepared.tmp_path.clone();

        let external = b"external replacement after initial read";
        std::fs::write(&sidecar_path, external).unwrap();
        let error = crate::download::file::publish_file_if_unchanged(
            &prepared.tmp_path,
            &prepared.sidecar_path,
            prepared.expected,
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("bytes changed"), "{error:#}");
        assert_eq!(
            std::fs::read(&sidecar_path).unwrap(),
            external,
            "an external change after the initial read must not be replaced"
        );
        assert!(
            temp_path.exists(),
            "refused publication retains the prepared file because an exchange failure could have displaced user bytes there"
        );
    }

    #[tokio::test]
    async fn write_sidecar_refuses_file_created_after_missing_read() {
        let dir = tempfile::tempdir().unwrap();
        let media_path = dir.path().join("appeared.jpg");
        let sidecar_path = dir.path().join("appeared.jpg.xmp");
        std::fs::write(&media_path, b"placeholder").unwrap();

        let prepared = prepare_sidecar_write(
            &media_path,
            &MetadataWrite {
                rating: Some(4),
                ..MetadataWrite::default()
            },
            ".meta-tmp",
        )
        .unwrap()
        .expect("new sidecar should be prepared");
        let temp_path = prepared.tmp_path.clone();

        let external = b"sidecar created by another application";
        std::fs::write(&sidecar_path, external).unwrap();
        let error = crate::download::file::publish_file_if_unchanged(
            &prepared.tmp_path,
            &prepared.sidecar_path,
            prepared.expected,
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("appeared"), "{error:#}");
        assert_eq!(std::fs::read(&sidecar_path).unwrap(), external);
        assert!(
            temp_path.exists(),
            "refused publication retains the prepared file for operator inspection"
        );
    }

    #[tokio::test]
    async fn write_sidecar_retries_after_retained_conflict_temp() {
        let dir = tempfile::tempdir().unwrap();
        let media_path = dir.path().join("temp-conflict.jpg");
        let sidecar_path = dir.path().join("temp-conflict.jpg.xmp");
        std::fs::write(&media_path, b"placeholder").unwrap();

        let prepared = prepare_sidecar_write(
            &media_path,
            &MetadataWrite {
                rating: Some(4),
                ..MetadataWrite::default()
            },
            ".meta-tmp",
        )
        .unwrap()
        .expect("initial sidecar should be prepared");
        let retained_path = prepared.tmp_path.clone();
        let retained_bytes = std::fs::read(&retained_path).unwrap();

        ensure_initialized();
        let mut external = XmpMeta::new().unwrap();
        external
            .set_property(
                xmp_ns::DC,
                "creator",
                &XmpValue::new("External author".to_string()),
            )
            .unwrap();
        std::fs::write(&sidecar_path, external.to_string().into_bytes()).unwrap();
        crate::download::file::publish_file_if_unchanged(
            &prepared.tmp_path,
            &prepared.sidecar_path,
            prepared.expected,
        )
        .await
        .unwrap_err();

        write_sidecar(
            &media_path,
            &MetadataWrite {
                rating: Some(4),
                ..MetadataWrite::default()
            },
            ".meta-tmp",
        )
        .await
        .expect("a retained conflict file must not block a later attempt");

        assert_eq!(
            std::fs::read(&retained_path).unwrap(),
            retained_bytes,
            "the later attempt must not overwrite or delete ambiguous retained bytes"
        );
        let written = std::fs::read_to_string(&sidecar_path).unwrap();
        assert!(written.contains("External author"), "{written}");
        assert!(written.contains("Rating"), "{written}");
    }

    #[test]
    fn write_sidecar_clears_only_fields_previously_owned_by_kei() {
        let dir = test_tmp_dir("sidecar_tests");
        std::fs::create_dir_all(&dir).unwrap();
        let media_path = dir.join("owned.jpg");
        let sidecar_path = dir.join("owned.jpg.xmp");
        std::fs::write(&media_path, b"placeholder").unwrap();

        const THIRD_PARTY_NS: &str = "https://example.invalid/xmp/third-party/";
        ensure_initialized();
        XmpMeta::register_namespace(THIRD_PARTY_NS, "thirdParty").unwrap();
        let mut seed = XmpMeta::new().unwrap();
        seed.set_property(
            xmp_ns::DC,
            "creator",
            &XmpValue::new("User-Photographer".to_string()),
        )
        .unwrap();
        seed.set_property(
            THIRD_PARTY_NS,
            "developSettings",
            &XmpValue::new("opaque-user-data".to_string()),
        )
        .unwrap();
        std::fs::write(&sidecar_path, seed.to_string().into_bytes()).unwrap();

        let first = MetadataWrite {
            datetime: Some("2024:06:15 10:00:00".into()),
            offset_time_original: Some("+10:00".into()),
            clear_datetime_offsets: false,
            require_native_heif_capture_time: false,
            gps_datetime: Some("2024-06-15T10:00:01Z".into()),
            gps_speed: Some(xmp_rational(25, 2)),
            gps_speed_ref: Some("K".into()),
            gps_h_positioning_error: Some(xmp_rational(13, 4)),
            preserve_source_gps: false,
            rating: Some(5),
            gps: Some(GpsCoords {
                latitude: 37.7,
                longitude: -122.4,
                altitude: Some(10.0),
            }),
            title: Some("Owned title".into()),
            description: Some("Owned description".into()),
            keywords: vec!["vacation".into()],
            people: vec!["Alice".into()],
            is_hidden: true,
            is_archived: true,
            media_subtype: Some("portrait".into()),
            burst_id: Some("burst-1".into()),
        };
        write_sidecar_with_default_suffix(&media_path, &first).unwrap();

        let cleared = MetadataWrite {
            datetime: first.datetime.clone(),
            gps: Some(GpsCoords {
                latitude: 37.7,
                longitude: -122.4,
                altitude: None,
            }),
            ..MetadataWrite::default()
        };
        write_sidecar_with_default_suffix(&media_path, &cleared).unwrap();

        let meta = read_sidecar_meta(&media_path);
        assert!(meta.contains_property(xmp_ns::XMP, "CreateDate"));
        assert!(meta.contains_property(xmp_ns::EXIF, "GPSLatitude"));
        for (namespace, property) in [
            (xmp_ns::XMP, "Rating"),
            (EXIF_EX_XMP_NS, "OffsetTimeOriginal"),
            (xmp_ns::EXIF, "GPSTimeStamp"),
            (xmp_ns::EXIF, "GPSSpeed"),
            (xmp_ns::EXIF, "GPSSpeedRef"),
            (xmp_ns::EXIF, "GPSHPositioningError"),
            (xmp_ns::EXIF, "GPSAltitude"),
            (xmp_ns::EXIF, "GPSAltitudeRef"),
            (xmp_ns::DC, "subject"),
            (xmp_ns::IPTC_EXT, "PersonInImage"),
            (KEI_XMP_NS, "hidden"),
            (KEI_XMP_NS, "archived"),
            (KEI_XMP_NS, "mediaSubtype"),
            (KEI_XMP_NS, "burstId"),
        ] {
            assert!(
                !meta.contains_property(namespace, property),
                "cleared kei-owned property must be removed: {property}"
            );
        }
        for property in ["title", "description"] {
            let path = XmpMeta::compose_lang_selector(xmp_ns::DC, property, "x-default").unwrap();
            assert!(
                !meta.contains_property(xmp_ns::DC, &path),
                "cleared kei-owned localized property must be removed: {property}"
            );
        }
        let managed = meta.property(KEI_XMP_NS, KEI_MANAGED_FIELDS).unwrap().value;
        assert!(managed.contains("exif:GPSLatitude"));
        assert!(!managed.contains("xmp:Rating"));
        assert!(!managed.contains("exifEX:OffsetTimeOriginal"));
        assert!(!managed.contains("exif:GPSTimeStamp"));
        assert!(!managed.contains("exif:GPSSpeed"));
        assert!(!managed.contains("exif:GPSSpeedRef"));
        assert!(!managed.contains("exif:GPSHPositioningError"));
        assert!(!managed.contains("exif:GPSAltitude"));
        assert_eq!(
            meta.property(THIRD_PARTY_NS, "developSettings")
                .unwrap()
                .value,
            "opaque-user-data"
        );
        assert!(
            std::fs::read_to_string(&sidecar_path)
                .unwrap()
                .contains("User-Photographer"),
            "clearing kei-owned fields must preserve dc:creator"
        );

        fs::remove_file(&sidecar_path).ok();
        fs::remove_file(&media_path).ok();
    }

    #[test]
    fn write_sidecar_preserves_unmarked_standard_fields_when_source_is_empty() {
        let dir = test_tmp_dir("sidecar_tests");
        std::fs::create_dir_all(&dir).unwrap();
        let media_path = dir.join("legacy.jpg");
        let sidecar_path = dir.join("legacy.jpg.xmp");
        std::fs::write(&media_path, b"placeholder").unwrap();

        ensure_initialized();
        let mut seed = XmpMeta::new().unwrap();
        seed.set_property_i32(xmp_ns::XMP, "Rating", &XmpValue::new(5))
            .unwrap();
        seed.set_property(
            xmp_ns::EXIF,
            "GPSTimeStamp",
            &XmpValue::new("2024-06-15T10:00:01Z".to_string()),
        )
        .unwrap();
        seed.set_property(xmp_ns::EXIF, "GPSSpeed", &XmpValue::new("25/2".to_string()))
            .unwrap();
        seed.set_property(xmp_ns::EXIF, "GPSSpeedRef", &XmpValue::new("K".to_string()))
            .unwrap();
        seed.set_property(
            xmp_ns::EXIF,
            "GPSHPositioningError",
            &XmpValue::new("13/4".to_string()),
        )
        .unwrap();
        seed.set_localized_text(
            xmp_ns::DC,
            "description",
            None,
            "x-default",
            "User description",
        )
        .unwrap();
        std::fs::write(&sidecar_path, seed.to_string().into_bytes()).unwrap();

        write_sidecar_with_default_suffix(
            &media_path,
            &MetadataWrite {
                datetime: Some("2024:06:15 10:00:00".into()),
                ..MetadataWrite::default()
            },
        )
        .unwrap();

        let meta = read_sidecar_meta(&media_path);
        assert_eq!(
            meta.property_i32(xmp_ns::XMP, "Rating").unwrap().value,
            5,
            "an unmarked rating may belong to another application"
        );
        for property in ["GPSTimeStamp", "GPSSpeed", "GPSSpeedRef"] {
            assert!(
                meta.contains_property(xmp_ns::EXIF, property),
                "an unmarked GPS property may belong to another application: {property}"
            );
        }
        assert!(
            meta.property(xmp_ns::EXIF, "GPSHPositioningError")
                .is_some_and(|property| property.value == "13/4"),
            "an unmarked GPS property may belong to another application: GPSHPositioningError"
        );
        assert_eq!(
            meta.localized_text(xmp_ns::DC, "description", None, "x-default")
                .unwrap()
                .0
                .value,
            "User description",
            "an unmarked description may belong to the user"
        );
        let managed = meta.property(KEI_XMP_NS, KEI_MANAGED_FIELDS).unwrap().value;
        assert!(!managed.contains("xmp:Rating"));
        assert!(!managed.contains("dc:description"));
        assert!(!managed.contains("exif:GPSTimeStamp"));
        assert!(!managed.contains("exif:GPSSpeed"));
        assert!(!managed.contains("exif:GPSSpeedRef"));
        assert!(!managed.contains("exif:GPSHPositioningError"));

        fs::remove_file(&sidecar_path).ok();
        fs::remove_file(&media_path).ok();
    }

    #[test]
    fn write_sidecar_preserves_owned_source_gps_when_source_state_is_unknown() {
        let dir = test_tmp_dir("sidecar_preserve_unknown_source_gps");
        fs::create_dir_all(&dir).unwrap();
        let media_path = dir.join("unknown-source.jpg");
        let sidecar_path = dir.join("unknown-source.jpg.xmp");
        fs::write(&media_path, b"placeholder").unwrap();

        write_sidecar_with_default_suffix(
            &media_path,
            &MetadataWrite {
                gps_datetime: Some("2024-06-15T10:00:01Z".into()),
                gps_speed: Some(xmp_rational(25, 2)),
                gps_speed_ref: Some("K".into()),
                gps_h_positioning_error: Some(xmp_rational(13, 4)),
                ..MetadataWrite::default()
            },
        )
        .unwrap();
        write_sidecar_with_default_suffix(
            &media_path,
            &MetadataWrite {
                rating: Some(5),
                preserve_source_gps: true,
                ..MetadataWrite::default()
            },
        )
        .unwrap();

        let meta = read_sidecar_meta(&media_path);
        for property in ["GPSTimeStamp", "GPSSpeed", "GPSSpeedRef"] {
            assert!(
                meta.contains_property(xmp_ns::EXIF, property),
                "unknown source state must preserve {property}"
            );
        }
        assert!(
            meta.contains_property(xmp_ns::EXIF, "GPSHPositioningError"),
            "unknown source state must preserve GPSHPositioningError"
        );
        let managed = meta.property(KEI_XMP_NS, KEI_MANAGED_FIELDS).unwrap().value;
        for token in [
            "exif:GPSTimeStamp",
            "exif:GPSSpeed",
            "exif:GPSSpeedRef",
            "exif:GPSHPositioningError",
        ] {
            assert!(
                managed.split(',').any(|value| value == token),
                "unknown source state must preserve {token}"
            );
        }
        assert_eq!(meta.property_i32(xmp_ns::XMP, "Rating").unwrap().value, 5);

        fs::remove_file(&sidecar_path).ok();
        fs::remove_file(&media_path).ok();
    }

    #[test]
    fn write_sidecar_preserves_existing_user_fields() {
        // A third-party tool (Darktable, digiKam) wrote a sidecar with
        // dc:creator before kei ever ran. On our write, the creator must
        // survive; kei's rating / keywords layer on top.
        let dir = test_tmp_dir("sidecar_tests");
        std::fs::create_dir_all(&dir).unwrap();
        let media_path = dir.join("merge.jpg");
        let sidecar_path = dir.join("merge.jpg.xmp");
        std::fs::write(&media_path, b"placeholder").unwrap();

        // Seed an existing sidecar that carries a dc:creator we must keep.
        ensure_initialized();
        let mut seed = XmpMeta::new().unwrap();
        seed.set_property(
            xmp_toolkit::xmp_ns::DC,
            "creator",
            &xmp_toolkit::XmpValue::new("User-Photographer".to_string()),
        )
        .unwrap();
        std::fs::write(&sidecar_path, seed.to_string().into_bytes()).unwrap();

        // kei writes its own rating on top.
        let write = MetadataWrite {
            rating: Some(4),
            ..MetadataWrite::default()
        };
        write_sidecar_with_default_suffix(&media_path, &write).expect("sidecar merge");

        let merged = fs::read_to_string(&sidecar_path).unwrap();
        assert!(
            merged.contains("User-Photographer"),
            "existing user-authored dc:creator must survive kei's write: {merged}"
        );
        assert!(
            merged.contains("Rating") || merged.contains("rating"),
            "kei's rating must be applied on top: {merged}"
        );

        fs::remove_file(&sidecar_path).ok();
        fs::remove_file(&media_path).ok();
    }

    /// Third-party tools (Darktable, digiKam, Lightroom) attach custom
    /// namespaces to their sidecars. Kei's merge must preserve those too,
    /// not just well-known dc: properties.
    #[test]
    fn write_sidecar_preserves_non_dc_namespaces() {
        let dir = test_tmp_dir("sidecar_tests");
        std::fs::create_dir_all(&dir).unwrap();
        let media_path = dir.join("darktable.jpg");
        let sidecar_path = dir.join("darktable.jpg.xmp");
        std::fs::write(&media_path, b"placeholder").unwrap();

        ensure_initialized();
        const DARKTABLE_NS: &str = "http://darktable.sf.net/";
        XmpMeta::register_namespace(DARKTABLE_NS, "darktable").unwrap();

        let mut seed = XmpMeta::new().unwrap();
        seed.set_property(DARKTABLE_NS, "history_end", &XmpValue::new("7".to_string()))
            .unwrap();
        seed.set_property(DARKTABLE_NS, "xmp_version", &XmpValue::new("5".to_string()))
            .unwrap();
        // Third-party develop-settings-style blob under a non-dc namespace.
        seed.set_property(
            DARKTABLE_NS,
            "raw_params",
            &XmpValue::new("gc5ghbmY2k8...opaque...".to_string()),
        )
        .unwrap();
        std::fs::write(&sidecar_path, seed.to_string().into_bytes()).unwrap();

        let write = MetadataWrite {
            rating: Some(3),
            keywords: vec!["vacation".to_string()],
            ..MetadataWrite::default()
        };
        write_sidecar_with_default_suffix(&media_path, &write).expect("sidecar merge");

        let merged = fs::read_to_string(&sidecar_path).unwrap();
        for expected in ["history_end", "xmp_version", "raw_params", "gc5ghbmY2k8"] {
            assert!(
                merged.contains(expected),
                "darktable field `{expected}` must survive kei's merge: {merged}"
            );
        }
        assert!(
            merged.contains("Rating") || merged.contains("rating"),
            "kei's rating must be applied on top: {merged}"
        );
        assert!(
            merged.contains("vacation"),
            "kei's keyword must be applied on top: {merged}"
        );

        fs::remove_file(&sidecar_path).ok();
        fs::remove_file(&media_path).ok();
    }

    #[test]
    fn write_sidecar_preserves_unparsable_existing() {
        let dir = test_tmp_dir("sidecar_tests");
        std::fs::create_dir_all(&dir).unwrap();
        let media_path = dir.join("garbage.jpg");
        let sidecar_path = dir.join("garbage.jpg.xmp");
        std::fs::write(&media_path, b"placeholder").unwrap();
        let original = b"<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF";
        std::fs::write(&sidecar_path, original).unwrap();

        let write = MetadataWrite {
            title: Some("Clean".into()),
            ..MetadataWrite::default()
        };
        let error = write_sidecar_with_default_suffix(&media_path, &write).unwrap_err();

        assert!(
            format!("{error:#}").contains("Could not parse existing XMP sidecar"),
            "{error:#}"
        );
        assert_eq!(
            fs::read(&sidecar_path).unwrap(),
            original,
            "unparsable third-party bytes must remain unchanged"
        );

        fs::remove_file(&sidecar_path).ok();
        fs::remove_file(&media_path).ok();
    }

    #[test]
    fn parse_existing_sidecar_accepts_bare_rdf_with_properties() {
        let bare_rdf = r#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
<rdf:Description rdf:about="" xmlns:dc="http://purl.org/dc/elements/1.1/">
<dc:format>image/jpeg</dc:format>
</rdf:Description>
</rdf:RDF>"#;

        let parsed = parse_existing_sidecar(bare_rdf).unwrap();
        assert_eq!(
            parsed.property(xmp_ns::DC, "format").unwrap().value,
            "image/jpeg"
        );
    }

    #[test]
    fn write_sidecar_does_not_touch_media_file() {
        let dir = test_tmp_dir("sidecar_tests");
        std::fs::create_dir_all(&dir).unwrap();
        let media_path = dir.join("untouched.jpg");
        let original_bytes = b"opaque-bytes-dont-care-about-format";
        std::fs::write(&media_path, original_bytes).unwrap();

        let write = MetadataWrite {
            rating: Some(3),
            ..MetadataWrite::default()
        };
        write_sidecar_with_default_suffix(&media_path, &write).unwrap();

        let after = fs::read(&media_path).unwrap();
        assert_eq!(
            after,
            original_bytes.to_vec(),
            "sidecar write must never alter the media file"
        );

        let sidecar = dir.join("untouched.jpg.xmp");
        fs::remove_file(&sidecar).ok();
        fs::remove_file(&media_path).ok();
    }
}
