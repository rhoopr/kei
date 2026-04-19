//! Embedded metadata (XMP + native EXIF/IPTC reconciliation) via Adobe's
//! XMP Toolkit.
//!
//! The writer runs through [`xmp_toolkit::XmpFile`], which vendors Adobe's
//! reference XMPFiles implementation. One code path covers JPEG, HEIC, PNG,
//! TIFF, MP4, MOV, and more — whatever kei downloads from iCloud ends up with
//! the same metadata embedded in its file bytes.
//!
//! XMP Toolkit also reconciles XMP with native EXIF/IPTC blocks on formats
//! that carry them (notably JPEG), so a consumer reading only EXIF still
//! sees values like `Rating`, GPS, and `DateTimeOriginal`.

use std::path::Path;
use std::sync::Once;

use anyhow::{Context, Result};
use xmp_toolkit::{xmp_ns, OpenFileOptions, XmpFile, XmpMeta, XmpValue};

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
/// HEIC/HEIF routes through `libheif` because Adobe's vendored XMPFiles has
/// no HEIF handler. Everything else (JPEG, PNG, TIFF, MP4, MOV, …) uses
/// `xmp_toolkit`.
///
/// Atomic: we copy the input to a sibling `.meta-tmp`, patch it in place,
/// then rename over the target. A crash mid-write leaves the original
/// untouched.
pub(crate) fn apply_metadata(path: &Path, write: &MetadataWrite) -> Result<()> {
    if write.is_empty() {
        return Ok(());
    }
    if is_heif_path(path) {
        apply_metadata_heif(path, write)
    } else {
        apply_metadata_xmp_toolkit(path, write)
    }
}

/// Whether this path's extension is HEIF / HEIC / HIF / AVIF — formats
/// that XMP Toolkit's bundled handlers can't open but libheif can.
fn is_heif_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| {
            let lower = e.to_ascii_lowercase();
            matches!(lower.as_str(), "heic" | "heif" | "hif" | "avif")
        })
        .unwrap_or(false)
}

fn apply_metadata_xmp_toolkit(path: &Path, write: &MetadataWrite) -> Result<()> {
    ensure_initialized();

    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(".meta-tmp");
    let tmp_path = path.with_file_name(&tmp_name);
    std::fs::copy(path, &tmp_path)
        .with_context(|| format!("Copying {} -> {}", path.display(), tmp_path.display()))?;

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

        if let Some(dt) = &write.datetime {
            // XMP uses ISO 8601 datetimes; our stored form is the EXIF-style
            // "YYYY:MM:DD HH:MM:SS". Convert for XMP, keep a local EXIF copy
            // so XMP Toolkit's reconciler writes the native block too.
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

    match result {
        Ok(()) => {
            std::fs::rename(&tmp_path, path).with_context(|| {
                format!("Renaming {} -> {}", tmp_path.display(), path.display())
            })?;
            tracing::debug!(path = %path.display(), "Applied metadata");
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp_path);
            Err(e)
        }
    }
}

/// HEIC write path: serialize the requested fields into an XMP packet, then
/// insert it as a MIME item inside the HEIC's `meta` box using an ISO-BMFF
/// atom editor. Operates on file bytes directly so the encoded image data in
/// `mdat` stays byte-for-byte identical — honouring invariant 2 (never modify
/// user data without explicit instruction).
fn apply_metadata_heif(path: &Path, write: &MetadataWrite) -> Result<()> {
    ensure_initialized();

    let input = std::fs::read(path)
        .with_context(|| format!("Reading {} for HEIC update", path.display()))?;
    let xmp_bytes = build_xmp_packet(write)?;
    let new_bytes = insert_xmp_into_heif(&input, &xmp_bytes)
        .with_context(|| format!("Inserting XMP into HEIC {}", path.display()))?;

    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(".meta-tmp");
    let tmp_path = path.with_file_name(&tmp_name);
    std::fs::write(&tmp_path, &new_bytes)
        .with_context(|| format!("Writing patched HEIC to {}", tmp_path.display()))?;
    std::fs::rename(&tmp_path, path)
        .with_context(|| format!("Renaming {} -> {}", tmp_path.display(), path.display()))?;
    tracing::debug!(path = %path.display(), "Applied HEIC metadata");
    Ok(())
}

/// Surgically insert (or replace) the XMP `mime` item inside a HEIC file.
///
/// The HEIC container is ISO-BMFF — a sequence of top-level atoms. XMP lives
/// inside the `meta` atom as an item with `item_type = "mime"` and
/// `content_type = "application/rdf+xml"`. We store the XMP bytes in an
/// `idat` atom inside `meta` (construction_method 1, offsets relative to
/// idat), so the encoded image bytes in `mdat` — referenced by file-absolute
/// offsets from other `iloc` entries — just shift by however much `meta` grew.
fn insert_xmp_into_heif(input: &[u8], xmp: &[u8]) -> Result<Vec<u8>> {
    use mp4_atom::{
        Any, DecodeMaybe, Encode, FourCC, Iloc, ItemInfoEntry, ItemLocation, ItemLocationExtent,
        Mdat,
    };

    // Record each top-level atom along with its original byte offset in the
    // input so we can rewrite file-absolute iloc entries correctly — the
    // existing iloc offsets point into the original mdat, and those offsets
    // must be updated so that after re-serialization they still land on the
    // same image bytes even though the meta box grew.
    let mut cursor: &[u8] = input;
    let mut atoms: Vec<Any> = Vec::new();
    let mut original_offsets: Vec<u64> = Vec::new();
    while !cursor.is_empty() {
        let offset_before_atom = (input.len() - cursor.len()) as u64;
        match Any::decode_maybe(&mut cursor)? {
            Some(a) => {
                atoms.push(a);
                original_offsets.push(offset_before_atom);
            }
            None => anyhow::bail!(
                "unparseable tail at offset {} of {}",
                offset_before_atom,
                input.len()
            ),
        }
    }

    let meta_idx = atoms
        .iter()
        .position(|a| matches!(a, Any::Meta(_)))
        .ok_or_else(|| anyhow::anyhow!("HEIC has no meta box"))?;

    // Step 1: locate and drop the trailing mdat that a prior kei write
    // appended (if any) so we don't accumulate stale XMP payloads on
    // re-sync. We identify it by: (a) the existing XMP iloc entry's
    // extent range, (b) it sitting past the image-data mdat, (c) no
    // other iloc entry pointing into it.
    let stale_mdat_idx = locate_stale_kei_mdat(&atoms, &original_offsets, meta_idx);

    // Step 2: remove the XMP entries from iinf and iloc. No idat mutation
    // needed because we don't put XMP in idat — it goes into a dedicated
    // mdat at end of file (see step 3 below).
    if let Any::Meta(meta) = &mut atoms[meta_idx] {
        let removed_ids = remove_existing_xmp_items(meta);
        if let Some(iloc) = meta.get_mut::<Iloc>() {
            iloc.item_locations
                .retain(|loc| !removed_ids.contains(&loc.item_id));
        }
    }

    // Step 3: drop the stale mdat atom (indexes shift, recompute meta_idx
    // relative to the surviving atoms).
    let meta_idx = if let Some(stale) = stale_mdat_idx {
        atoms.remove(stale);
        original_offsets.remove(stale);
        if stale < meta_idx {
            meta_idx - 1
        } else {
            meta_idx
        }
    } else {
        meta_idx
    };

    // Step 4: reserve the iinf + iloc entries for the new XMP. The iloc
    // offset is the file offset our appended mdat's DATA will have in the
    // re-serialized output. mp4-atom encodes Iloc at fixed width regardless
    // of offset value, so we can append the mdat atom first, compute the
    // resulting running offsets, then populate the iloc offset.
    let new_item_id = {
        let Any::Meta(meta) = &atoms[meta_idx] else {
            unreachable!()
        };
        next_free_item_id(meta)
    };

    atoms.push(Any::Mdat(Mdat { data: xmp.to_vec() }));
    let new_mdat_idx = atoms.len() - 1;

    // Insert placeholder iloc entry (offset=0) and iinf entry so that running
    // offsets reflect the final meta size.
    {
        let Any::Meta(meta) = &mut atoms[meta_idx] else {
            unreachable!()
        };
        push_iinf_entry(
            meta,
            ItemInfoEntry {
                item_id: new_item_id,
                item_protection_index: 0,
                item_type: Some(FourCC::new(b"mime")),
                item_name: String::new(),
                content_type: Some("application/rdf+xml".to_string()),
                content_encoding: Some(String::new()),
                item_not_in_presentation: false,
            },
        );
        push_iloc_entry(
            meta,
            ItemLocation {
                item_id: new_item_id,
                construction_method: 0,
                data_reference_index: 0,
                base_offset: 0,
                extents: vec![ItemLocationExtent {
                    item_reference_index: 0,
                    offset: 0,
                    length: xmp.len() as u64,
                }],
            },
        );
    }

    // Step 5: remap pre-existing file-offset iloc entries and fill in the
    // offset for the XMP iloc entry we just pushed.
    let new_positions = running_offsets(&atoms);
    let xmp_file_offset = new_positions[new_mdat_idx] + header_size_of(&atoms[new_mdat_idx]);

    let file_offset_map: Vec<(u64, u64, u64)> = atoms
        .iter()
        .enumerate()
        .take(new_mdat_idx) // skip the mdat we just added; it has no matching original
        .filter_map(|(idx, a)| {
            let orig = *original_offsets.get(idx)?;
            let size = encoded_size(a);
            Some((orig, orig + size, new_positions[idx]))
        })
        .collect();

    if let Any::Meta(meta) = &mut atoms[meta_idx] {
        if let Some(iloc) = meta.get_mut::<Iloc>() {
            remap_file_offsets(iloc, &file_offset_map);
            // Now fill in the XMP entry's offset (last iloc entry).
            if let Some(xmp_loc) = iloc
                .item_locations
                .iter_mut()
                .find(|l| l.item_id == new_item_id)
            {
                if let Some(extent) = xmp_loc.extents.first_mut() {
                    extent.offset = xmp_file_offset;
                }
            }
        }
    }

    let mut out = Vec::with_capacity(input.len() + xmp.len() + 512);
    for atom in &atoms {
        atom.encode(&mut out)?;
    }
    Ok(out)
}

/// Walk existing iinf/iloc to find any previously-kei-appended XMP mdat.
/// Criteria: an iinf entry flagged as `mime` + `application/rdf+xml`, its
/// iloc entry references a range that lies entirely within a single trailing
/// mdat atom, and no other iloc entry references into that atom.
fn locate_stale_kei_mdat(
    atoms: &[mp4_atom::Any],
    original_offsets: &[u64],
    meta_idx: usize,
) -> Option<usize> {
    use mp4_atom::{Any, FourCC, Iinf, Iloc};
    let meta = if let Any::Meta(m) = &atoms[meta_idx] {
        m
    } else {
        return None;
    };
    let iinf = meta.get::<Iinf>()?;
    let iloc = meta.get::<Iloc>()?;

    let xmp_item_ids: Vec<u32> = iinf
        .item_infos
        .iter()
        .filter(|e| {
            e.item_type == Some(FourCC::new(b"mime"))
                && e.content_type.as_deref() == Some("application/rdf+xml")
        })
        .map(|e| e.item_id)
        .collect();
    if xmp_item_ids.is_empty() {
        return None;
    }

    // For each candidate, find the atom that its extent range falls within.
    for item_id in &xmp_item_ids {
        let Some(loc) = iloc.item_locations.iter().find(|l| l.item_id == *item_id) else {
            continue;
        };
        if loc.construction_method != 0 {
            continue;
        }
        let Some(extent) = loc.extents.first() else {
            continue;
        };
        let abs_start = loc.base_offset.saturating_add(extent.offset);
        let abs_end = abs_start.saturating_add(extent.length);

        // Find the atom whose original byte range fully contains the extent.
        for (idx, atom) in atoms.iter().enumerate() {
            if !matches!(atom, Any::Mdat(_)) {
                continue;
            }
            let atom_start = original_offsets[idx];
            let atom_end = atom_start + encoded_size(atom);
            if abs_start < atom_start || abs_end > atom_end {
                continue;
            }
            // Check that no OTHER iloc entry references this atom.
            let other_refs = iloc.item_locations.iter().any(|other| {
                if other.item_id == *item_id || other.construction_method != 0 {
                    return false;
                }
                other.extents.iter().any(|e| {
                    let o_start = other.base_offset.saturating_add(e.offset);
                    o_start >= atom_start && o_start < atom_end
                })
            });
            if !other_refs {
                return Some(idx);
            }
        }
    }
    None
}

/// Byte size of an atom's box header (the length field + 4-byte kind code).
/// mp4-atom always emits a 32-bit-length header for atoms that fit — large
/// mdats (>4GB) would use a 16-byte header, but kei isn't going to hit that.
fn header_size_of(_atom: &mp4_atom::Any) -> u64 {
    8
}

/// Return a vector where entry `i` is the byte offset at which atom `i` will
/// sit in the re-serialized output (i.e. the running sum of preceding atom
/// sizes).
fn running_offsets(atoms: &[mp4_atom::Any]) -> Vec<u64> {
    let mut offsets = Vec::with_capacity(atoms.len());
    let mut running = 0u64;
    for atom in atoms {
        offsets.push(running);
        running += encoded_size(atom);
    }
    offsets
}

/// Translate each construction_method-0 iloc offset from "original file
/// offset" to "new file offset", using the per-atom old_start/old_end/new_start
/// table. An offset that falls within `[old_start, old_end)` is rebased onto
/// `new_start` with the same intra-atom position.
fn remap_file_offsets(iloc: &mut mp4_atom::Iloc, ranges: &[(u64, u64, u64)]) {
    for loc in &mut iloc.item_locations {
        if loc.construction_method != 0 {
            continue;
        }
        // Some encoders put the whole file offset in `base_offset` and leave
        // extent offsets at 0; others leave base_offset 0 and put absolute
        // offsets on each extent. Handle both by remapping either piece that
        // lands in a known original-atom range.
        loc.base_offset = remap_point(loc.base_offset, ranges).unwrap_or(loc.base_offset);
        for extent in &mut loc.extents {
            let absolute = loc.base_offset.saturating_add(extent.offset);
            if let Some(new_abs) = remap_point(absolute, ranges) {
                extent.offset = new_abs.saturating_sub(loc.base_offset);
            }
        }
    }
}

fn remap_point(file_offset: u64, ranges: &[(u64, u64, u64)]) -> Option<u64> {
    for &(old_start, old_end, new_start) in ranges {
        if file_offset >= old_start && file_offset < old_end {
            return Some(new_start + (file_offset - old_start));
        }
    }
    None
}

fn encoded_size(atom: &mp4_atom::Any) -> u64 {
    use mp4_atom::Encode;
    let mut sink = Vec::new();
    atom.encode(&mut sink).ok();
    sink.len() as u64
}

fn remove_existing_xmp_items(meta: &mut mp4_atom::Meta) -> Vec<u32> {
    use mp4_atom::{FourCC, Iinf};
    let mut removed = Vec::new();
    if let Some(iinf) = meta.get_mut::<Iinf>() {
        iinf.item_infos.retain(|entry| {
            let is_xmp = entry.item_type == Some(FourCC::new(b"mime"))
                && entry.content_type.as_deref() == Some("application/rdf+xml");
            if is_xmp {
                removed.push(entry.item_id);
                false
            } else {
                true
            }
        });
    }
    removed
}

fn next_free_item_id(meta: &mp4_atom::Meta) -> u32 {
    use mp4_atom::Iinf;
    meta.get::<Iinf>()
        .map(|iinf| {
            iinf.item_infos
                .iter()
                .map(|e| e.item_id)
                .max()
                .map(|m| m + 1)
                .unwrap_or(1)
        })
        .unwrap_or(1)
}

fn push_iinf_entry(meta: &mut mp4_atom::Meta, entry: mp4_atom::ItemInfoEntry) {
    use mp4_atom::Iinf;
    if meta.get::<Iinf>().is_none() {
        meta.push(Iinf {
            item_infos: Vec::new(),
        });
    }
    meta.get_mut::<Iinf>().unwrap().item_infos.push(entry);
}

fn push_iloc_entry(meta: &mut mp4_atom::Meta, loc: mp4_atom::ItemLocation) {
    use mp4_atom::Iloc;
    if meta.get::<Iloc>().is_none() {
        meta.push(Iloc {
            item_locations: Vec::new(),
        });
    }
    meta.get_mut::<Iloc>().unwrap().item_locations.push(loc);
}

/// Build a standalone XMP packet from the metadata write bundle.
///
/// Reuses the same field mapping as the xmp_toolkit-backed writer so HEIC
/// and JPEG end up with identical XMP content. Output is the serialized
/// packet bytes (with the `<?xpacket begin=?>` wrapper), ready to hand to
/// libheif's `add_xmp_metadata`.
fn build_xmp_packet(write: &MetadataWrite) -> Result<Vec<u8>> {
    let mut meta = XmpMeta::new().context("creating XmpMeta")?;

    if let Some(dt) = &write.datetime {
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

    for kw in &write.keywords {
        meta.append_array_item(
            xmp_ns::DC,
            &XmpValue::new("subject".to_string()).set_is_array(true),
            &XmpValue::new(kw.clone()),
        )?;
    }

    for name in &write.people {
        meta.append_array_item(
            xmp_ns::IPTC_EXT,
            &XmpValue::new("PersonInImage".to_string()).set_is_array(true),
            &XmpValue::new(name.clone()),
        )?;
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

    Ok(meta.to_string().into_bytes())
}

/// EXIF stores datetimes as `"YYYY:MM:DD HH:MM:SS"`; XMP wants ISO 8601
/// `"YYYY-MM-DDTHH:MM:SS"`. Best-effort conversion — on malformed input we
/// return the original so XMP Toolkit can reject it with a clear error.
fn exif_datetime_to_iso(s: &str) -> String {
    let bytes = s.as_bytes();
    if bytes.len() == 19 && bytes[4] == b':' && bytes[7] == b':' && bytes[10] == b' ' {
        let mut out = s.to_owned();
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
    format!("{},{:.4}{}", deg as u32, min, hemisphere)
}

/// XMP `exif:GPSAltitude` is a rational; we use `meters/1` (scale of 1).
fn encode_altitude(meters: f64) -> String {
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

    /// Check the file extension dispatcher — HEIC paths route to libheif,
    /// everything else to XmpFile.
    #[test]
    fn is_heif_path_recognises_heic_variants() {
        assert!(is_heif_path(Path::new("/a/b.heic")));
        assert!(is_heif_path(Path::new("/a/b.HEIC")));
        assert!(is_heif_path(Path::new("/a/b.HEIF")));
        assert!(is_heif_path(Path::new("/a/b.hif")));
        assert!(is_heif_path(Path::new("/a/b.avif")));
        assert!(!is_heif_path(Path::new("/a/b.jpg")));
        assert!(!is_heif_path(Path::new("/a/b.mov")));
        assert!(!is_heif_path(Path::new("/a/b")));
    }

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
}
