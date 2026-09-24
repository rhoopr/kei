//! Managed XMP properties, namespace initialization, and field encoding.

use super::exif_datetime_to_iso;
use super::values::MetadataWrite;
#[cfg(test)]
use anyhow::{Context, Result};
use little_exif::rational::uR64;
use std::sync::Once;
use xmp_toolkit::{XmpMeta, XmpValue, xmp_ns};

/// Custom XMP namespace for kei-specific fields that don't fit standard
/// schemas (`hidden`, `archived`, `mediaSubtype`, `burstId`). Consumers that
/// care about these know to look for the `kei` prefix.
pub(super) const KEI_XMP_NS: &str = "https://github.com/rhoopr/kei/ns/1.0/";

const KEI_XMP_PREFIX: &str = "kei";

pub(super) const EXIF_EX_XMP_NS: &str = "http://cipa.jp/exif/1.0/";

const EXIF_EX_XMP_PREFIX: &str = "exifEX";

pub(super) const KEI_MANAGED_FIELDS: &str = "managedFields";

#[derive(Clone, Copy)]
enum ManagedXmpField {
    CreateDate,
    ModifyDate,
    DateTimeOriginal,
    DateCreated,
    OffsetTimeOriginal,
    GpsDateTime,
    GpsSpeed,
    GpsSpeedRef,
    GpsHPositioningError,
    Rating,
    GpsLatitude,
    GpsLongitude,
    GpsAltitude,
    GpsAltitudeRef,
    Title,
    Description,
    Keywords,
    People,
    Hidden,
    Archived,
    MediaSubtype,
    BurstId,
}

const MANAGED_XMP_FIELDS: [ManagedXmpField; 22] = [
    ManagedXmpField::CreateDate,
    ManagedXmpField::ModifyDate,
    ManagedXmpField::DateTimeOriginal,
    ManagedXmpField::DateCreated,
    ManagedXmpField::OffsetTimeOriginal,
    ManagedXmpField::GpsDateTime,
    ManagedXmpField::GpsSpeed,
    ManagedXmpField::GpsSpeedRef,
    ManagedXmpField::GpsHPositioningError,
    ManagedXmpField::Rating,
    ManagedXmpField::GpsLatitude,
    ManagedXmpField::GpsLongitude,
    ManagedXmpField::GpsAltitude,
    ManagedXmpField::GpsAltitudeRef,
    ManagedXmpField::Title,
    ManagedXmpField::Description,
    ManagedXmpField::Keywords,
    ManagedXmpField::People,
    ManagedXmpField::Hidden,
    ManagedXmpField::Archived,
    ManagedXmpField::MediaSubtype,
    ManagedXmpField::BurstId,
];

impl ManagedXmpField {
    const fn token(self) -> &'static str {
        match self {
            Self::CreateDate => "xmp:CreateDate",
            Self::ModifyDate => "xmp:ModifyDate",
            Self::DateTimeOriginal => "exif:DateTimeOriginal",
            Self::DateCreated => "photoshop:DateCreated",
            Self::OffsetTimeOriginal => "exifEX:OffsetTimeOriginal",
            Self::GpsDateTime => "exif:GPSTimeStamp",
            Self::GpsSpeed => "exif:GPSSpeed",
            Self::GpsSpeedRef => "exif:GPSSpeedRef",
            // CIPA Table 17 and Apple Photos retain GPS tag 31 in exif,
            // unlike the exifEX-only OffsetTime fields above.
            Self::GpsHPositioningError => "exif:GPSHPositioningError",
            Self::Rating => "xmp:Rating",
            Self::GpsLatitude => "exif:GPSLatitude",
            Self::GpsLongitude => "exif:GPSLongitude",
            Self::GpsAltitude => "exif:GPSAltitude",
            Self::GpsAltitudeRef => "exif:GPSAltitudeRef",
            Self::Title => "dc:title[x-default]",
            Self::Description => "dc:description[x-default]",
            Self::Keywords => "dc:subject",
            Self::People => "iptcExt:PersonInImage",
            Self::Hidden => "kei:hidden",
            Self::Archived => "kei:archived",
            Self::MediaSubtype => "kei:mediaSubtype",
            Self::BurstId => "kei:burstId",
        }
    }

    fn is_present(self, write: &MetadataWrite) -> bool {
        match self {
            Self::CreateDate | Self::ModifyDate | Self::DateTimeOriginal | Self::DateCreated => {
                write.datetime.is_some()
            }
            Self::OffsetTimeOriginal => write.offset_time_original.is_some(),
            Self::GpsDateTime => write.gps_datetime.is_some(),
            Self::GpsSpeed => write.gps_speed.is_some(),
            Self::GpsSpeedRef => write.gps_speed_ref.is_some(),
            Self::GpsHPositioningError => write.gps_h_positioning_error.is_some(),
            Self::Rating => write.rating.is_some(),
            Self::GpsLatitude | Self::GpsLongitude => write.gps.is_some(),
            Self::GpsAltitude | Self::GpsAltitudeRef => {
                write.gps.is_some_and(|gps| gps.altitude.is_some())
            }
            Self::Title => write.title.is_some(),
            Self::Description => write.description.is_some(),
            Self::Keywords => !write.keywords.is_empty(),
            Self::People => !write.people.is_empty(),
            Self::Hidden => write.is_hidden,
            Self::Archived => write.is_archived,
            Self::MediaSubtype => write.media_subtype.is_some(),
            Self::BurstId => write.burst_id.is_some(),
        }
    }

    const fn is_source_gps(self) -> bool {
        matches!(
            self,
            Self::GpsDateTime | Self::GpsSpeed | Self::GpsSpeedRef | Self::GpsHPositioningError
        )
    }

    fn delete(self, meta: &mut XmpMeta) -> xmp_toolkit::XmpResult<()> {
        let (namespace, path) = match self {
            Self::CreateDate => (xmp_ns::XMP, "CreateDate"),
            Self::ModifyDate => (xmp_ns::XMP, "ModifyDate"),
            Self::DateTimeOriginal => (xmp_ns::EXIF, "DateTimeOriginal"),
            Self::DateCreated => (xmp_ns::PHOTOSHOP, "DateCreated"),
            Self::OffsetTimeOriginal => (EXIF_EX_XMP_NS, "OffsetTimeOriginal"),
            Self::GpsDateTime => (xmp_ns::EXIF, "GPSTimeStamp"),
            Self::GpsSpeed => (xmp_ns::EXIF, "GPSSpeed"),
            Self::GpsSpeedRef => (xmp_ns::EXIF, "GPSSpeedRef"),
            Self::GpsHPositioningError => (xmp_ns::EXIF, "GPSHPositioningError"),
            Self::Rating => (xmp_ns::XMP, "Rating"),
            Self::GpsLatitude => (xmp_ns::EXIF, "GPSLatitude"),
            Self::GpsLongitude => (xmp_ns::EXIF, "GPSLongitude"),
            Self::GpsAltitude => (xmp_ns::EXIF, "GPSAltitude"),
            Self::GpsAltitudeRef => (xmp_ns::EXIF, "GPSAltitudeRef"),
            Self::Keywords => (xmp_ns::DC, "subject"),
            Self::People => (xmp_ns::IPTC_EXT, "PersonInImage"),
            Self::Hidden => (KEI_XMP_NS, "hidden"),
            Self::Archived => (KEI_XMP_NS, "archived"),
            Self::MediaSubtype => (KEI_XMP_NS, "mediaSubtype"),
            Self::BurstId => (KEI_XMP_NS, "burstId"),
            Self::Title => {
                let path = XmpMeta::compose_lang_selector(xmp_ns::DC, "title", "x-default")?;
                return meta.delete_property(xmp_ns::DC, &path);
            }
            Self::Description => {
                let path = XmpMeta::compose_lang_selector(xmp_ns::DC, "description", "x-default")?;
                return meta.delete_property(xmp_ns::DC, &path);
            }
        };
        meta.delete_property(namespace, path)
    }
}

static INIT: Once = Once::new();

pub(super) fn ensure_initialized() {
    INIT.call_once(|| {
        // Registering the same namespace twice is fine; XMP Toolkit returns
        // the existing prefix. Ignore failures so standard XMP output still
        // works if a custom namespace cannot be registered.
        let _ = XmpMeta::register_namespace(KEI_XMP_NS, KEI_XMP_PREFIX);
        let _ = XmpMeta::register_namespace(EXIF_EX_XMP_NS, EXIF_EX_XMP_PREFIX);
    });
}

/// Apply a complete sidecar snapshot without deleting metadata that kei cannot
/// prove it wrote. The ownership list records exact properties, so a later
/// clear can remove only the values established by an earlier kei write.
pub(super) fn apply_to_owned_sidecar(
    meta: &mut XmpMeta,
    write: &MetadataWrite,
) -> xmp_toolkit::XmpResult<()> {
    let previous = meta
        .property(KEI_XMP_NS, KEI_MANAGED_FIELDS)
        .map(|value| value.value)
        .unwrap_or_default();
    let previous_tokens: Vec<&str> = previous
        .split(',')
        .filter(|token| !token.is_empty())
        .collect();

    for field in MANAGED_XMP_FIELDS {
        if previous_tokens.contains(&field.token())
            && !field.is_present(write)
            && !(write.preserve_source_gps && field.is_source_gps())
        {
            field.delete(meta)?;
        }
    }

    apply_to_xmp(meta, write)?;

    let mut current_tokens: Vec<String> = previous_tokens
        .into_iter()
        .filter(|token| {
            !MANAGED_XMP_FIELDS.iter().any(|field| {
                field.token() == *token && !(write.preserve_source_gps && field.is_source_gps())
            })
        })
        .map(str::to_owned)
        .collect();
    current_tokens.extend(
        MANAGED_XMP_FIELDS
            .iter()
            .copied()
            .filter(|field| field.is_present(write))
            .map(|field| field.token().to_owned()),
    );

    if current_tokens.is_empty() {
        meta.delete_property(KEI_XMP_NS, KEI_MANAGED_FIELDS)?;
    } else {
        meta.set_property(
            KEI_XMP_NS,
            KEI_MANAGED_FIELDS,
            &XmpValue::new(current_tokens.join(",")),
        )?;
    }
    Ok(())
}

/// Apply the requested metadata fields to an `XmpMeta`. Single source of
/// truth — both the xmp_toolkit-backed and ISO-BMFF-backed writers route
/// through here so the two paths produce identical XMP content.
pub(super) fn apply_to_xmp(
    meta: &mut XmpMeta,
    write: &MetadataWrite,
) -> xmp_toolkit::XmpResult<()> {
    if write.clear_datetime_offsets {
        for path in ["OffsetTimeOriginal", "OffsetTimeDigitized", "OffsetTime"] {
            meta.delete_property(EXIF_EX_XMP_NS, path)?;
        }
    }
    if let Some(dt) = &write.datetime {
        // Embedded plans use EXIF form; sidecar plans already carry unzoned ISO 8601.
        let iso = exif_datetime_to_iso(dt);
        let iso_with_offset = write
            .offset_time_original
            .as_ref()
            .map_or_else(|| iso.clone(), |offset| format!("{iso}{offset}"));
        meta.set_property(
            xmp_ns::XMP,
            "CreateDate",
            &XmpValue::new(iso_with_offset.clone()),
        )?;
        meta.set_property(
            xmp_ns::XMP,
            "ModifyDate",
            &XmpValue::new(iso_with_offset.clone()),
        )?;
        meta.set_property(
            xmp_ns::EXIF,
            "DateTimeOriginal",
            &XmpValue::new(iso_with_offset.clone()),
        )?;
        meta.set_property(
            xmp_ns::PHOTOSHOP,
            "DateCreated",
            &XmpValue::new(iso_with_offset),
        )?;
    }
    if let Some(offset) = &write.offset_time_original {
        meta.set_property(
            EXIF_EX_XMP_NS,
            "OffsetTimeOriginal",
            &XmpValue::new(offset.clone()),
        )?;
    }

    if let Some(dt) = &write.gps_datetime {
        meta.set_property(xmp_ns::EXIF, "GPSTimeStamp", &XmpValue::new(dt.clone()))?;
    }

    if let Some(speed) = &write.gps_speed {
        meta.set_property(xmp_ns::EXIF, "GPSSpeed", &XmpValue::new(speed.encode()))?;
    }
    if let Some(speed_ref) = &write.gps_speed_ref {
        meta.set_property(
            xmp_ns::EXIF,
            "GPSSpeedRef",
            &XmpValue::new(speed_ref.clone()),
        )?;
    }
    if let Some(error) = &write.gps_h_positioning_error {
        meta.set_property(
            xmp_ns::EXIF,
            "GPSHPositioningError",
            &XmpValue::new(error.encode()),
        )?;
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

/// Build a standalone XMP packet from a bundle of fields. Thin convenience
/// over [`apply_to_xmp`] for callers (mostly tests) that want the serialized
/// packet bytes directly.
#[cfg(test)]
pub(super) fn build_xmp_packet(write: &MetadataWrite) -> Result<Vec<u8>> {
    ensure_initialized();
    let mut meta = XmpMeta::new().context("Could not create XMP metadata")?;
    apply_to_xmp(&mut meta, write)?;
    Ok(meta.to_string().into_bytes())
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

/// Preserve provider altitude precision in the rational form required by EXIF reconciliation.
fn encode_altitude(meters: f64) -> String {
    let rational = uR64::from(meters.abs());
    format!("{}/{}", rational.nominator, rational.denominator)
}

#[cfg(test)]
#[allow(clippy::unused_result_ok, reason = "test cleanup is best-effort")]
mod tests {
    use super::super::exif_datetime_to_iso;
    use super::super::probe::probe_from_meta;
    use super::super::values::MetadataWrite;
    #[cfg(test)]
    use super::build_xmp_packet;
    use super::{EXIF_EX_XMP_NS, apply_to_xmp, encode_gps, ensure_initialized};
    use xmp_toolkit::{XmpMeta, XmpValue, xmp_ns};

    #[test]
    fn xmp_datetime_includes_capture_offset() {
        let packet = build_xmp_packet(&MetadataWrite {
            datetime: Some("2026:02:01 09:31:59".to_string()),
            offset_time_original: Some("+11:00".to_string()),
            ..MetadataWrite::default()
        })
        .unwrap();
        let meta = std::str::from_utf8(&packet)
            .unwrap()
            .parse::<XmpMeta>()
            .unwrap();
        assert_eq!(
            meta.property(EXIF_EX_XMP_NS, "OffsetTimeOriginal")
                .unwrap()
                .value,
            "+11:00"
        );
        let probe = probe_from_meta(&meta);
        assert_eq!(probe.offset_time_original.as_deref(), Some("+11:00"));
        assert!(meta.property(xmp_ns::EXIF, "OffsetTimeOriginal").is_none());
        assert_eq!(
            meta.property(xmp_ns::EXIF, "DateTimeOriginal")
                .unwrap()
                .value,
            "2026-02-01T09:31:59+11:00"
        );
    }

    /// An offset names the zone of one specific timestamp. Replacing the
    /// timestamp orphans every offset already in the file, so they have to go
    /// before the resolved one lands, or a stale zone qualifies a capture time
    /// it never described.
    #[test]
    fn xmp_write_clears_offsets_orphaned_from_a_replaced_timestamp() {
        ensure_initialized();
        let mut meta = XmpMeta::new().unwrap();
        for property in ["OffsetTimeOriginal", "OffsetTimeDigitized", "OffsetTime"] {
            meta.set_property(
                EXIF_EX_XMP_NS,
                property,
                &XmpValue::new("+02:00".to_string()),
            )
            .unwrap();
        }

        apply_to_xmp(
            &mut meta,
            &MetadataWrite {
                datetime: Some("2026:02:01 09:31:59".to_string()),
                offset_time_original: Some("+11:00".to_string()),
                clear_datetime_offsets: true,
                ..MetadataWrite::default()
            },
        )
        .unwrap();

        assert_eq!(
            meta.property(EXIF_EX_XMP_NS, "OffsetTimeOriginal")
                .unwrap()
                .value,
            "+11:00"
        );
        for property in ["OffsetTimeDigitized", "OffsetTime"] {
            assert!(
                !meta.contains_property(EXIF_EX_XMP_NS, property),
                "{property} must not survive to qualify a timestamp it never described"
            );
        }
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
}
