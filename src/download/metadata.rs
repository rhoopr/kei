//! Embedded metadata (XMP + native EXIF/IPTC reconciliation).
//!
//! For JPEG / PNG / TIFF / MP4 / MOV, the writer runs through
//! [`xmp_toolkit::XmpFile`] — Adobe's vendored XMPFiles implementation, which
//! also reconciles XMP with native EXIF/IPTC blocks so consumers that read
//! only EXIF still see values like `Rating`, GPS, and `DateTimeOriginal`.
//!
//! HEIC / HEIF / AVIF have no XMP Toolkit handler, so those formats route
//! through [`super::heif`], which edits the ISO-BMFF container directly with
//! [`mp4_atom`]. Both paths build the XMP packet via the same
//! [`apply_to_xmp`] helper, so the embedded content is identical.

use std::path::Path;
use std::sync::Once;

use anyhow::{Context, Result};
use xmp_toolkit::{xmp_ns, OpenFileOptions, XmpFile, XmpMeta, XmpValue};

use super::heif;
use crate::fs_util::atomic_install;

/// Custom XMP namespace for kei-specific fields that don't fit standard
/// schemas (`hidden`, `archived`, `mediaSubtype`, `burstId`). Consumers that
/// care about these know to look for the `kei` prefix.
const KEI_XMP_NS: &str = "https://github.com/rhoopr/kei/ns/1.0/";
const KEI_XMP_PREFIX: &str = "kei";

static INIT: Once = Once::new();

fn ensure_initialized() {
    INIT.call_once(|| {
        // Registering the same namespace twice is fine; XMP Toolkit returns
        // the existing prefix. Ignore the Result — even a failure here only
        // disables the kei: fields, and standard XMP continues to work.
        let _ = XmpMeta::register_namespace(KEI_XMP_NS, KEI_XMP_PREFIX);
    });
}

/// Snapshot of existing metadata fields that gate write decisions. Populated
/// from whatever XMP Toolkit sees in the file (XMP + reconciled EXIF/IPTC).
#[derive(Debug, Clone, Default)]
pub(crate) struct ExifProbe {
    pub(crate) datetime_original: Option<String>,
    pub(crate) has_gps: bool,
}

pub(crate) fn probe_exif(path: &Path) -> Result<ExifProbe> {
    ensure_initialized();
    let mut file = XmpFile::new().context("creating XmpFile handle")?;
    if file
        .open_file(path, OpenFileOptions::default().for_read().only_xmp())
        .is_err()
    {
        return Ok(ExifProbe::default());
    }
    let meta = match file.xmp() {
        Some(m) => m,
        None => return Ok(ExifProbe::default()),
    };
    let datetime_original = meta
        .property(xmp_ns::EXIF, "DateTimeOriginal")
        .map(|v| v.value);
    let has_gps = meta.contains_property(xmp_ns::EXIF, "GPSLatitude")
        || meta.contains_property(xmp_ns::EXIF, "GPSLongitude");
    Ok(ExifProbe {
        datetime_original,
        has_gps,
    })
}

/// GPS triple passed to [`apply_metadata`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct GpsCoords {
    pub(crate) latitude: f64,
    pub(crate) longitude: f64,
    pub(crate) altitude: Option<f64>,
}

/// Bundle of every field the writer knows how to embed. Empty / default
/// fields are skipped.
#[derive(Debug, Default, Clone)]
pub(crate) struct MetadataWrite {
    /// `"YYYY:MM:DD HH:MM:SS"` EXIF-style datetime string.
    pub(crate) datetime: Option<String>,
    pub(crate) rating: Option<u8>,
    pub(crate) gps: Option<GpsCoords>,
    pub(crate) title: Option<String>,
    pub(crate) description: Option<String>,
    /// `dc:subject` bag — iCloud keyword tags and album names merge here.
    pub(crate) keywords: Vec<String>,
    /// MWG-RS person names for `iptcExt:PersonInImage`.
    pub(crate) people: Vec<String>,
    pub(crate) is_hidden: bool,
    pub(crate) is_archived: bool,
    pub(crate) media_subtype: Option<String>,
    pub(crate) burst_id: Option<String>,
}

impl MetadataWrite {
    pub(crate) fn is_empty(&self) -> bool {
        self.datetime.is_none()
            && self.rating.is_none()
            && self.gps.is_none()
            && self.title.is_none()
            && self.description.is_none()
            && self.keywords.is_empty()
            && self.people.is_empty()
            && !self.is_hidden
            && !self.is_archived
            && self.media_subtype.is_none()
            && self.burst_id.is_none()
    }
}

/// Write the requested metadata into the file's XMP packet, with EXIF/IPTC
/// reconciliation where the container supports it.
///
/// Atomic: we copy the input to a sibling `.meta-tmp`, patch it in place,
/// then rename over the target. A crash mid-write leaves the original
/// untouched.
pub(crate) fn apply_metadata(path: &Path, write: &MetadataWrite) -> Result<()> {
    if write.is_empty() {
        return Ok(());
    }
    if heif::is_heif_path(path) {
        apply_metadata_heif(path, write)
    } else {
        apply_metadata_xmp_toolkit(path, write)
    }
}

/// Whether this path's extension is one the in-place embedded-metadata writer
/// can patch. JPEG / PNG / TIFF / MP4 / MOV go through XMP Toolkit;
/// HEIC / HEIF / AVIF go through [`super::heif`].
pub(crate) fn is_embed_writable_path(path: &Path) -> bool {
    if heif::is_heif_path(path) {
        return true;
    }
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        matches!(
            e.to_ascii_lowercase().as_str(),
            "jpg" | "jpeg" | "png" | "tif" | "tiff" | "mp4" | "mov"
        )
    })
}

/// Write `write` as a `.xmp` sidecar next to the media file, atomically.
///
/// If a sidecar already exists (e.g., from Darktable / Lightroom / digiKam),
/// its existing XMP properties are read and kei's fields are layered on top
/// rather than overwriting the whole packet. A malformed existing sidecar
/// falls back to a fresh packet — kei's enriched view wins over a file we
/// can't parse.
pub(crate) fn write_sidecar(media_path: &Path, write: &MetadataWrite) -> Result<()> {
    if write.is_empty() {
        return Ok(());
    }
    ensure_initialized();

    let Some(name) = media_path.file_name() else {
        anyhow::bail!("sidecar target has no filename: {}", media_path.display());
    };
    let mut sidecar_name = name.to_os_string();
    sidecar_name.push(".xmp");
    let sidecar_path = media_path.with_file_name(&sidecar_name);
    let mut tmp_name = sidecar_name;
    tmp_name.push(".meta-tmp");
    let tmp_path = sidecar_path.with_file_name(&tmp_name);

    // Seed the packet with any existing sidecar content so user-authored
    // ratings / keywords / develop settings from another tool survive.
    let mut meta = match std::fs::read(&sidecar_path) {
        Ok(existing_bytes) => match std::str::from_utf8(&existing_bytes)
            .ok()
            .and_then(|s| s.parse::<XmpMeta>().ok())
        {
            Some(parsed) => parsed,
            None => {
                tracing::warn!(
                    path = %sidecar_path.display(),
                    "Existing XMP sidecar could not be parsed; overwriting with a fresh packet"
                );
                XmpMeta::new().context("creating XmpMeta")?
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            XmpMeta::new().context("creating XmpMeta")?
        }
        Err(e) => {
            return Err(e)
                .with_context(|| format!("Reading existing sidecar {}", sidecar_path.display()));
        }
    };
    apply_to_xmp(&mut meta, write)?;
    let bytes = meta.to_string().into_bytes();

    std::fs::write(&tmp_path, &bytes)
        .with_context(|| format!("Writing XMP sidecar {}", tmp_path.display()))?;
    atomic_install(&tmp_path, &sidecar_path).with_context(|| {
        format!(
            "Installing XMP sidecar {} -> {}",
            tmp_path.display(),
            sidecar_path.display()
        )
    })?;
    tracing::debug!(path = %sidecar_path.display(), "Wrote XMP sidecar");
    Ok(())
}

/// Remove the tmp file on drop unless disarmed. Protects `.meta-tmp` against
/// a panic across the xmp_toolkit FFI boundary; no orphan sweep matches this
/// suffix.
#[derive(Debug)]
struct TmpGuard<'a> {
    path: &'a Path,
    armed: bool,
}

impl<'a> TmpGuard<'a> {
    fn new(path: &'a Path) -> Self {
        Self { path, armed: true }
    }

    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for TmpGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_file(self.path);
        }
    }
}

fn apply_metadata_xmp_toolkit(path: &Path, write: &MetadataWrite) -> Result<()> {
    ensure_initialized();

    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(".meta-tmp");
    let tmp_path = path.with_file_name(&tmp_name);
    std::fs::copy(path, &tmp_path)
        .with_context(|| format!("Copying {} -> {}", path.display(), tmp_path.display()))?;

    let guard = TmpGuard::new(&tmp_path);

    let result: Result<()> = (|| {
        let mut file = XmpFile::new().context("creating XmpFile handle")?;
        file.open_file(
            &tmp_path,
            OpenFileOptions::default().for_update().use_smart_handler(),
        )
        .with_context(|| format!("Opening {} for XMP update", tmp_path.display()))?;

        let mut meta = file
            .xmp()
            .unwrap_or_else(|| XmpMeta::new().unwrap_or_default());
        apply_to_xmp(&mut meta, write)?;

        if !file.can_put_xmp(&meta) {
            anyhow::bail!(
                "format handler for {} does not support writing XMP",
                tmp_path.display()
            );
        }
        file.put_xmp(&meta)
            .with_context(|| format!("Writing XMP into {}", tmp_path.display()))?;
        file.try_close()
            .with_context(|| format!("Closing {} after XMP update", tmp_path.display()))?;
        Ok(())
    })();

    result?;
    std::fs::rename(&tmp_path, path)
        .with_context(|| format!("Renaming {} -> {}", tmp_path.display(), path.display()))?;
    guard.disarm();
    tracing::debug!(path = %path.display(), "Applied metadata");
    Ok(())
}

/// Apply the requested metadata fields to an `XmpMeta`. Single source of
/// truth — both the xmp_toolkit-backed and ISO-BMFF-backed writers route
/// through here so the two paths produce identical XMP content.
fn apply_to_xmp(meta: &mut XmpMeta, write: &MetadataWrite) -> xmp_toolkit::XmpResult<()> {
    if let Some(dt) = &write.datetime {
        // XMP uses ISO 8601; our stored form is EXIF-style "YYYY:MM:DD HH:MM:SS".
        // Convert for XMP, keep a local EXIF copy so XMP Toolkit's reconciler
        // writes the native block too on formats that have one.
        let iso = exif_datetime_to_iso(dt);
        meta.set_property(xmp_ns::XMP, "CreateDate", &XmpValue::new(iso.clone()))?;
        meta.set_property(xmp_ns::XMP, "ModifyDate", &XmpValue::new(iso.clone()))?;
        meta.set_property(
            xmp_ns::EXIF,
            "DateTimeOriginal",
            &XmpValue::new(iso.clone()),
        )?;
        meta.set_property(xmp_ns::PHOTOSHOP, "DateCreated", &XmpValue::new(iso))?;
    }

    if let Some(r) = write.rating {
        meta.set_property_i32(xmp_ns::XMP, "Rating", &XmpValue::new(i32::from(r.min(5))))?;
    }

    if let Some(gps) = write.gps {
        meta.set_property(
            xmp_ns::EXIF,
            "GPSLatitude",
            &XmpValue::new(encode_gps(gps.latitude, 'N', 'S')),
        )?;
        meta.set_property(
            xmp_ns::EXIF,
            "GPSLongitude",
            &XmpValue::new(encode_gps(gps.longitude, 'E', 'W')),
        )?;
        if let Some(alt) = gps.altitude {
            meta.set_property(
                xmp_ns::EXIF,
                "GPSAltitude",
                &XmpValue::new(encode_altitude(alt)),
            )?;
            meta.set_property(
                xmp_ns::EXIF,
                "GPSAltitudeRef",
                &XmpValue::new(if alt < 0.0 { "1" } else { "0" }.to_string()),
            )?;
        }
    }

    if let Some(title) = &write.title {
        meta.set_localized_text(xmp_ns::DC, "title", None, "x-default", title)?;
    }

    if let Some(desc) = &write.description {
        meta.set_localized_text(xmp_ns::DC, "description", None, "x-default", desc)?;
    }

    if !write.keywords.is_empty() {
        // Clear existing dc:subject so we don't accumulate stale entries on
        // re-writes. XMP Toolkit has no bulk set for bags.
        let _ = meta.delete_property(xmp_ns::DC, "subject");
        for kw in &write.keywords {
            meta.append_array_item(
                xmp_ns::DC,
                &XmpValue::new("subject".to_string()).set_is_array(true),
                &XmpValue::new(kw.clone()),
            )?;
        }
    }

    if !write.people.is_empty() {
        let _ = meta.delete_property(xmp_ns::IPTC_EXT, "PersonInImage");
        for name in &write.people {
            meta.append_array_item(
                xmp_ns::IPTC_EXT,
                &XmpValue::new("PersonInImage".to_string()).set_is_array(true),
                &XmpValue::new(name.clone()),
            )?;
        }
    }

    if write.is_hidden {
        meta.set_property_bool(KEI_XMP_NS, "hidden", &XmpValue::new(true))?;
    }
    if write.is_archived {
        meta.set_property_bool(KEI_XMP_NS, "archived", &XmpValue::new(true))?;
    }
    if let Some(subtype) = &write.media_subtype {
        meta.set_property(KEI_XMP_NS, "mediaSubtype", &XmpValue::new(subtype.clone()))?;
    }
    if let Some(burst) = &write.burst_id {
        meta.set_property(KEI_XMP_NS, "burstId", &XmpValue::new(burst.clone()))?;
    }

    Ok(())
}

/// HEIC write path: read any existing XMP, apply our fields on top, and
/// insert the resulting packet as a MIME item inside the HEIC's `meta` box.
/// Operates on file bytes directly via ISO-BMFF atom editing so the encoded
/// image data in `mdat` stays byte-for-byte identical — invariant 2.
fn apply_metadata_heif(path: &Path, write: &MetadataWrite) -> Result<()> {
    ensure_initialized();

    let input = std::fs::read(path)
        .with_context(|| format!("Reading {} for HEIC update", path.display()))?;

    // Preserve any XMP the file already carries (e.g. Apple Live Photo or
    // depth markers) by parsing it into the XmpMeta we mutate. If parsing
    // fails or there's no existing XMP, start from an empty packet.
    let existing_xmp_bytes = heif::extract_xmp_bytes(&input);
    let mut meta = existing_xmp_bytes
        .as_deref()
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .and_then(|s| s.parse::<XmpMeta>().ok())
        .unwrap_or_else(|| XmpMeta::new().unwrap_or_default());
    apply_to_xmp(&mut meta, write)?;
    let xmp_bytes = meta.to_string().into_bytes();

    let new_bytes = heif::insert_xmp(&input, &xmp_bytes)
        .with_context(|| format!("Inserting XMP into HEIC {}", path.display()))?;

    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(".meta-tmp");
    let tmp_path = path.with_file_name(&tmp_name);
    std::fs::write(&tmp_path, &new_bytes)
        .with_context(|| format!("Writing patched HEIC to {}", tmp_path.display()))?;
    let guard = TmpGuard::new(&tmp_path);
    std::fs::rename(&tmp_path, path)
        .with_context(|| format!("Renaming {} -> {}", tmp_path.display(), path.display()))?;
    guard.disarm();
    tracing::debug!(path = %path.display(), "Applied HEIC metadata");
    Ok(())
}

/// Build a standalone XMP packet from a bundle of fields. Thin convenience
/// over [`apply_to_xmp`] for callers (mostly tests) that want the serialized
/// packet bytes directly.
#[cfg(test)]
fn build_xmp_packet(write: &MetadataWrite) -> Result<Vec<u8>> {
    let mut meta = XmpMeta::new().context("creating XmpMeta")?;
    apply_to_xmp(&mut meta, write)?;
    Ok(meta.to_string().into_bytes())
}

/// EXIF stores datetimes as `"YYYY:MM:DD HH:MM:SS"`; XMP wants ISO 8601
/// `"YYYY-MM-DDTHH:MM:SS"`. Best-effort conversion — on malformed input we
/// return the original so XMP Toolkit can reject it with a clear error.
fn exif_datetime_to_iso(s: &str) -> String {
    let bytes = s.as_bytes();
    if bytes.len() == 19 && bytes[4] == b':' && bytes[7] == b':' && bytes[10] == b' ' {
        let mut out = s.to_owned();
        // SAFETY: `out` is a freshly-owned String with no aliases. The length
        // check above proves indices 4, 7, 10 are in-bounds, and the
        // replacement bytes are all valid 7-bit ASCII, so UTF-8
        // well-formedness is preserved.
        unsafe {
            let b = out.as_bytes_mut();
            b[4] = b'-';
            b[7] = b'-';
            b[10] = b'T';
        }
        out
    } else {
        s.to_owned()
    }
}

/// Encode decimal degrees in the EXIF-in-XMP form `"DEG,MIN.FRACHEMI"` used
/// by [Xmp.exif.GPSLatitude] / `Xmp.exif.GPSLongitude`.
fn encode_gps(decimal: f64, pos: char, neg: char) -> String {
    let hemisphere = if decimal >= 0.0 { pos } else { neg };
    let abs = decimal.abs();
    let deg = abs.floor();
    let min = (abs - deg) * 60.0;
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "deg is floor of abs(lat|lon) so 0..=180; always fits in u32 with no sign"
    )]
    let deg_u32 = deg as u32;
    format!("{deg_u32},{min:.4}{hemisphere}")
}

/// XMP `exif:GPSAltitude` is a rational; we use `meters/1` (scale of 1).
fn encode_altitude(meters: f64) -> String {
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "abs() is non-negative; altitudes in millimeters never approach u64::MAX"
    )]
    let scaled = (meters.abs() * 1000.0).round() as u64;
    format!("{scaled}/1000")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn test_tmp_dir(subdir: &str) -> PathBuf {
        std::env::temp_dir().join("claude").join(subdir)
    }

    #[test]
    fn tmp_guard_cleans_up_on_drop() {
        let dir = test_tmp_dir("tmp_guard");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("armed.meta-tmp");
        fs::write(&path, b"pending").unwrap();
        {
            let _guard = TmpGuard::new(&path);
            assert!(path.exists(), "precondition: tmp file exists");
        }
        assert!(
            !path.exists(),
            "TmpGuard Drop must remove the tmp file on scope exit"
        );
    }

    #[test]
    fn tmp_guard_disarm_keeps_file() {
        let dir = test_tmp_dir("tmp_guard");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("disarmed.meta-tmp");
        fs::write(&path, b"keep me").unwrap();
        {
            let guard = TmpGuard::new(&path);
            guard.disarm();
        }
        assert!(path.exists(), "disarmed TmpGuard must not delete the file");
        fs::remove_file(&path).ok();
    }

    /// The xmp_toolkit writer runs closures across an FFI boundary, so a
    /// panic out of that FFI must still clean up `.meta-tmp`.
    #[test]
    fn tmp_guard_cleans_up_even_on_panic() {
        let dir = test_tmp_dir("tmp_guard");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("panic.meta-tmp");
        fs::write(&path, b"about to panic").unwrap();

        let path_for_closure = path.clone();
        let joined = std::panic::catch_unwind(move || {
            let _guard = TmpGuard::new(&path_for_closure);
            panic!("simulated xmp_toolkit FFI panic");
        });
        assert!(joined.is_err(), "closure was expected to panic");
        assert!(
            !path.exists(),
            "tmp file must be removed even when the work panics"
        );
    }

    /// Minimal valid JPEG (SOI + APP0 JFIF + EOI).
    fn minimal_jpeg() -> Vec<u8> {
        vec![
            0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00,
            0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0xFF, 0xD9,
        ]
    }

    fn fresh_jpeg(dir: &Path, name: &str) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        fs::write(&path, minimal_jpeg()).unwrap();
        path
    }

    fn read_meta(path: &Path) -> XmpMeta {
        ensure_initialized();
        let mut file = XmpFile::new().unwrap();
        file.open_file(path, OpenFileOptions::default().for_read())
            .unwrap();
        file.xmp().expect("no XMP in file")
    }

    #[test]
    fn apply_metadata_noop_when_empty() {
        let dir = test_tmp_dir("meta_tests");
        let path = fresh_jpeg(&dir, "noop.jpg");
        let before = fs::read(&path).unwrap();
        apply_metadata(&path, &MetadataWrite::default()).unwrap();
        let after = fs::read(&path).unwrap();
        assert_eq!(before, after, "empty write must not touch the file");
        fs::remove_file(&path).ok();
    }

    #[test]
    fn apply_metadata_datetime_roundtrips() {
        let dir = test_tmp_dir("meta_tests");
        let path = fresh_jpeg(&dir, "dt.jpg");
        apply_metadata(
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
        apply_metadata(
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
        apply_metadata(
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
        let dir = test_tmp_dir("meta_tests");
        let path = fresh_jpeg(&dir, "gps.jpg");
        apply_metadata(
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
        .unwrap();
        let probe = probe_exif(&path).unwrap();
        assert!(probe.has_gps);
        let meta = read_meta(&path);
        let lat = meta.property(xmp_ns::EXIF, "GPSLatitude").unwrap().value;
        assert!(lat.contains('N'), "lat should end with N: {lat}");
        let lng = meta.property(xmp_ns::EXIF, "GPSLongitude").unwrap().value;
        assert!(lng.contains('W'), "lng should end with W: {lng}");
        fs::remove_file(&path).ok();
    }

    #[test]
    fn apply_metadata_description_roundtrips() {
        let dir = test_tmp_dir("meta_tests");
        let path = fresh_jpeg(&dir, "desc.jpg");
        apply_metadata(
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
        apply_metadata(
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
        apply_metadata(
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
        apply_metadata(
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
        apply_metadata(
            &path,
            &MetadataWrite {
                datetime: Some("2024:06:15 10:00:00".to_string()),
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
        let result = apply_metadata(
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
    fn probe_exif_reports_empty_on_fresh_jpeg() {
        let dir = test_tmp_dir("meta_tests");
        let path = fresh_jpeg(&dir, "probe_empty.jpg");
        let probe = probe_exif(&path).unwrap();
        assert!(probe.datetime_original.is_none());
        assert!(!probe.has_gps);
        fs::remove_file(&path).ok();
    }

    #[test]
    fn exif_datetime_to_iso_converts_valid() {
        assert_eq!(
            exif_datetime_to_iso("2024:06:15 10:00:00"),
            "2024-06-15T10:00:00"
        );
    }

    #[test]
    fn exif_datetime_to_iso_leaves_invalid_unchanged() {
        assert_eq!(exif_datetime_to_iso("not a date"), "not a date");
    }

    #[test]
    fn encode_gps_positive_is_north() {
        let s = encode_gps(37.7749, 'N', 'S');
        assert!(s.ends_with('N'));
        assert!(s.starts_with("37,"));
    }

    #[test]
    fn encode_gps_negative_is_west() {
        let s = encode_gps(-122.4194, 'E', 'W');
        assert!(s.ends_with('W'));
        assert!(s.starts_with("122,"));
    }

    // ── HEIC tests ──────────────────────────────────────────────────────

    /// `build_xmp_packet` emits a packet bytes blob that libheif can accept.
    /// Verifies the packet contains the rdf:RDF wrapper and our data.
    #[test]
    fn build_xmp_packet_is_deterministic() {
        let w = MetadataWrite {
            rating: Some(3),
            title: Some("X".into()),
            ..MetadataWrite::default()
        };
        let a = build_xmp_packet(&w).unwrap();
        let b = build_xmp_packet(&w).unwrap();
        assert_eq!(a.len(), b.len(), "XMP packet size must be deterministic");
        assert_eq!(a, b, "XMP packet bytes must be deterministic");
    }

    #[test]
    fn build_xmp_packet_contains_requested_fields() {
        let bytes = build_xmp_packet(&MetadataWrite {
            rating: Some(4),
            title: Some("Beach".into()),
            keywords: vec!["vacation".into(), "sand".into()],
            ..MetadataWrite::default()
        })
        .unwrap();
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(s.contains("rdf:RDF"), "missing rdf:RDF wrapper");
        assert!(s.contains("xmp:Rating"), "missing xmp:Rating");
        assert!(s.contains("Beach"), "missing title value");
        assert!(s.contains("vacation"), "missing keyword");
    }

    const SAMPLE_HEIC: &[u8] = include_bytes!("../../tests/data/sample.heic");

    fn fresh_heic(dir: &Path, name: &str) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        fs::write(&path, SAMPLE_HEIC).unwrap();
        path
    }

    #[test]
    fn apply_metadata_heic_rating_and_title() {
        let dir = test_tmp_dir("meta_heic_tests");
        let path = fresh_heic(&dir, "rating.heic");
        apply_metadata(
            &path,
            &MetadataWrite {
                rating: Some(5),
                title: Some("Vacation".into()),
                keywords: vec!["beach".into()],
                ..MetadataWrite::default()
            },
        )
        .expect("HEIC metadata write");

        let xmp = extract_xmp_from_heic(&fs::read(&path).unwrap()).expect("XMP missing");
        let s = std::str::from_utf8(&xmp).unwrap();
        assert!(s.contains("xmp:Rating"), "XMP missing rating");
        assert!(s.contains("Vacation"), "XMP missing title");
        assert!(s.contains("beach"), "XMP missing keyword");
        fs::remove_file(&path).ok();
    }

    #[test]
    fn apply_metadata_heic_gps_roundtrips() {
        let dir = test_tmp_dir("meta_heic_tests");
        let path = fresh_heic(&dir, "gps.heic");
        apply_metadata(
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
        .expect("HEIC metadata write");

        let xmp = extract_xmp_from_heic(&fs::read(&path).unwrap()).expect("no XMP item");
        let s = std::str::from_utf8(&xmp).unwrap();
        assert!(s.contains("GPSLatitude"));
        assert!(s.contains('N'), "latitude ref missing");
        assert!(s.contains("GPSLongitude"));
        assert!(s.contains('W'), "longitude ref missing");
        assert!(s.contains("GPSAltitude"));
        fs::remove_file(&path).ok();
    }

    #[test]
    fn apply_metadata_heic_preserves_image_data() {
        let dir = test_tmp_dir("meta_heic_tests");
        let path = fresh_heic(&dir, "preserve.heic");
        let original_bytes = SAMPLE_HEIC.to_vec();
        apply_metadata(
            &path,
            &MetadataWrite {
                rating: Some(3),
                ..MetadataWrite::default()
            },
        )
        .unwrap();

        let new_bytes = fs::read(&path).unwrap();
        // XMP was appended, so the file grew by roughly packet size + box overhead.
        assert!(
            new_bytes.len() > original_bytes.len(),
            "file should grow after XMP write"
        );
        assert!(
            new_bytes.len() < original_bytes.len() + 16_384,
            "HEIC file grew unexpectedly by {} bytes",
            new_bytes.len() - original_bytes.len()
        );

        // The encoded image bytes in mdat must be byte-for-byte identical —
        // invariant 2. Locate mdat in both buffers and compare.
        let orig_mdat = find_mdat_bytes(&original_bytes).expect("original mdat");
        let new_mdat = find_mdat_bytes(&new_bytes).expect("new mdat");
        assert_eq!(
            orig_mdat, new_mdat,
            "mdat image data must not change across metadata writes"
        );

        fs::remove_file(&path).ok();
    }

    /// Second write should preserve fields written by the first — confirms
    /// the HEIC path reads existing XMP before mutating, so we don't drop
    /// e.g. Apple's existing XMP markers when adding kei-specific fields.
    #[test]
    fn apply_metadata_heic_preserves_existing_xmp_on_rewrite() {
        let dir = test_tmp_dir("meta_heic_tests");
        let path = fresh_heic(&dir, "preserve_xmp.heic");
        apply_metadata(
            &path,
            &MetadataWrite {
                title: Some("First".into()),
                ..MetadataWrite::default()
            },
        )
        .unwrap();
        apply_metadata(
            &path,
            &MetadataWrite {
                rating: Some(4),
                ..MetadataWrite::default()
            },
        )
        .unwrap();

        let xmp = extract_xmp_from_heic(&fs::read(&path).unwrap()).expect("XMP missing");
        let s = std::str::from_utf8(&xmp).unwrap();
        assert!(
            s.contains("First"),
            "first-write title should survive rewrite"
        );
        assert!(
            s.contains("xmp:Rating"),
            "second-write rating should be present"
        );
        fs::remove_file(&path).ok();
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

        apply_metadata(&path, &write).unwrap();
        let first = fs::read(&path).unwrap();
        apply_metadata(&path, &write).unwrap();
        let second = fs::read(&path).unwrap();

        // Rewriting with the same data must not accumulate XMP items or
        // otherwise grow the file on subsequent passes.
        assert_eq!(
            first.len(),
            second.len(),
            "re-writing identical metadata must be idempotent"
        );
        let xmp_count = count_xmp_items_in_heic(&second);
        assert_eq!(xmp_count, 1, "expected exactly one XMP item after rewrite");
        fs::remove_file(&path).ok();
    }

    /// Walk a HEIC file's top-level atoms and return the XMP packet bytes.
    /// The write path puts XMP in a trailing `mdat`; the iloc entry is
    /// construction_method=0 with a file-absolute offset, so we slice the
    /// file bytes directly.
    fn extract_xmp_from_heic(bytes: &[u8]) -> Option<Vec<u8>> {
        use mp4_atom::{Any, DecodeMaybe, FourCC, Iinf, Iloc};
        let mut cursor: &[u8] = bytes;
        while let Ok(Some(atom)) = Any::decode_maybe(&mut cursor) {
            if let Any::Meta(meta) = atom {
                let iinf = meta.get::<Iinf>()?;
                let iloc = meta.get::<Iloc>()?;
                let xmp_entry = iinf.item_infos.iter().find(|e| {
                    e.item_type == Some(FourCC::new(b"mime"))
                        && e.content_type.as_deref() == Some("application/rdf+xml")
                })?;
                let loc = iloc
                    .item_locations
                    .iter()
                    .find(|l| l.item_id == xmp_entry.item_id)?;
                if loc.construction_method != 0 {
                    return None;
                }
                let extent = loc.extents.first()?;
                let start = loc.base_offset.saturating_add(extent.offset) as usize;
                let end = start + extent.length as usize;
                if end > bytes.len() {
                    return None;
                }
                return Some(bytes[start..end].to_vec());
            }
        }
        None
    }

    fn count_xmp_items_in_heic(bytes: &[u8]) -> usize {
        use mp4_atom::{Any, DecodeMaybe, FourCC, Iinf};
        let mut cursor: &[u8] = bytes;
        while let Ok(Some(atom)) = Any::decode_maybe(&mut cursor) {
            if let Any::Meta(meta) = atom {
                if let Some(iinf) = meta.get::<Iinf>() {
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

    // ── Sidecar + format-dispatch tests ────────────────────────────────

    #[test]
    fn is_embed_writable_path_recognises_supported_formats() {
        for ext in [
            "jpg", "jpeg", "JPG", "png", "PNG", "tif", "tiff", "mp4", "MOV", "heic", "HEIF", "avif",
        ] {
            let p = PathBuf::from(format!("/a/b.{ext}"));
            assert!(is_embed_writable_path(&p), "{ext} should be writable");
        }
    }

    #[test]
    fn is_embed_writable_path_rejects_unsupported_formats() {
        for ext in ["dng", "raf", "aae", "gif", "webp", ""] {
            let p = PathBuf::from(format!("/a/b.{ext}"));
            assert!(!is_embed_writable_path(&p), "{ext} should NOT be writable");
        }
        assert!(!is_embed_writable_path(Path::new("/a/b")));
    }

    #[test]
    fn write_sidecar_is_noop_on_empty_write() {
        let dir = test_tmp_dir("sidecar_tests");
        std::fs::create_dir_all(&dir).unwrap();
        let media_path = dir.join("empty.jpg");
        std::fs::write(&media_path, b"placeholder").unwrap();
        write_sidecar(&media_path, &MetadataWrite::default()).unwrap();
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
        write_sidecar(&media_path, &write).expect("sidecar write");
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
        let dir = test_tmp_dir("sidecar_tests");
        std::fs::create_dir_all(&dir).unwrap();
        let media_path = dir.join("rewrite.jpg");
        std::fs::write(&media_path, b"placeholder").unwrap();

        let first = MetadataWrite {
            title: Some("Before".into()),
            ..MetadataWrite::default()
        };
        write_sidecar(&media_path, &first).unwrap();

        let second = MetadataWrite {
            title: Some("After".into()),
            ..MetadataWrite::default()
        };
        write_sidecar(&media_path, &second).unwrap();

        let sidecar = dir.join("rewrite.jpg.xmp");
        let s = fs::read_to_string(&sidecar).unwrap();
        assert!(s.contains("After"), "second write should replace first");
        assert!(
            !s.contains("Before"),
            "previous title must not leak through"
        );

        let tmp = dir.join("rewrite.jpg.xmp.meta-tmp");
        assert!(!tmp.exists(), "temp sidecar file must be cleaned up");

        fs::remove_file(&sidecar).ok();
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
        write_sidecar(&media_path, &write).expect("sidecar merge");

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
        write_sidecar(&media_path, &write).expect("sidecar merge");

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
    fn write_sidecar_recovers_from_unparsable_existing() {
        // A garbage existing sidecar should not block kei's write; we log
        // and fall back to a fresh packet rather than erroring.
        let dir = test_tmp_dir("sidecar_tests");
        std::fs::create_dir_all(&dir).unwrap();
        let media_path = dir.join("garbage.jpg");
        let sidecar_path = dir.join("garbage.jpg.xmp");
        std::fs::write(&media_path, b"placeholder").unwrap();
        std::fs::write(&sidecar_path, b"<<< this is not XMP >>>").unwrap();

        let write = MetadataWrite {
            title: Some("Clean".into()),
            ..MetadataWrite::default()
        };
        write_sidecar(&media_path, &write).expect("fallback to fresh packet");

        let out = fs::read_to_string(&sidecar_path).unwrap();
        assert!(out.contains("Clean"), "fallback write must land: {out}");

        fs::remove_file(&sidecar_path).ok();
        fs::remove_file(&media_path).ok();
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
        write_sidecar(&media_path, &write).unwrap();

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
