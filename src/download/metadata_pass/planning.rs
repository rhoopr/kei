//! Opt-in flags and embedded metadata write planning.

use crate::download::filter::MetadataPayload;
pub(crate) use crate::download::metadata::CaptureTimestampRepair;
use crate::state::types::embedded_absence;
use chrono::{DateTime, FixedOffset};

bitflags::bitflags! {
    /// Per-tag write toggles. `any_embed()` drives the `.part`-and-modify-before-rename
    /// flow; individual flags gate which fields get embedded into the media file.
    ///
    /// `EMBED_XMP` enables the XMP-only fields that have no native EXIF equivalent
    /// (title and keywords). Album names and people require separate opt-ins.
    /// `XMP_SIDECAR` is orthogonal - it writes a `.xmp` file next to the photo
    /// without touching the photo bytes.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub(crate) struct MetadataFlags: u16 {
        const DATETIME    = 1 << 0;
        const RATING      = 1 << 1;
        const GPS         = 1 << 2;
        const DESCRIPTION = 1 << 3;
        const EMBED_XMP   = 1 << 4;
        const XMP_SIDECAR = 1 << 5;
        const INCLUDE_ALBUMS = 1 << 6;
        const INCLUDE_PEOPLE = 1 << 7;
        const ORIENTATION = 1 << 8;
    }
}

impl MetadataFlags {
    /// Standard-field policy; version 1 bundled provider fields and groupings.
    pub(crate) const POLICY_REVISION: u8 = 4;

    /// Set of flags that drive the `.part`-and-modify-before-rename flow.
    /// Sidecar writes happen after the rename so `XMP_SIDECAR` is excluded.
    /// Enrichment choices alone do not authorize a write.
    const EMBED_MASK: Self = Self::all().difference(
        Self::XMP_SIDECAR
            .union(Self::INCLUDE_ALBUMS)
            .union(Self::INCLUDE_PEOPLE),
    );

    /// Whether any flag needs the downloaded bytes to stay as a `.part` file
    /// for in-place metadata editing before the atomic rename.
    pub(crate) fn any_embed(self) -> bool {
        self.intersects(Self::EMBED_MASK)
    }

    pub(crate) fn has_any_write(self) -> bool {
        self.any_embed() || self.contains(Self::XMP_SIDECAR)
    }
}

impl From<&crate::config::MetadataConfig> for MetadataFlags {
    fn from(metadata: &crate::config::MetadataConfig) -> Self {
        let mut flags = Self::empty();
        flags.set(Self::DATETIME, metadata.set_exif_datetime);
        flags.set(Self::ORIENTATION, metadata.set_exif_orientation);
        flags.set(Self::RATING, metadata.set_exif_rating);
        flags.set(Self::GPS, metadata.set_exif_gps);
        flags.set(Self::DESCRIPTION, metadata.set_exif_description);
        #[cfg(feature = "xmp")]
        {
            flags.set(Self::EMBED_XMP, metadata.embed_xmp);
            flags.set(Self::XMP_SIDECAR, metadata.xmp_sidecar);
            if metadata.embed_xmp || metadata.xmp_sidecar {
                flags.set(Self::INCLUDE_ALBUMS, metadata.include_albums);
                flags.set(Self::INCLUDE_PEOPLE, metadata.include_people);
            }
        }
        flags
    }
}

pub(super) fn gps_from_payload(
    payload: &MetadataPayload,
) -> Option<crate::download::metadata::GpsCoords> {
    match (payload.latitude, payload.longitude) {
        (Some(lat), Some(lng)) => Some(crate::download::metadata::GpsCoords {
            latitude: lat,
            longitude: lng,
            altitude: payload.altitude,
        }),
        _ => None,
    }
}

pub(super) fn offset_time_original(payload: &MetadataPayload) -> Option<String> {
    let offset = payload.timezone_offset.and_then(FixedOffset::east_opt)?;
    let seconds = offset.local_minus_utc();
    if seconds % 60 != 0 {
        return None;
    }
    let minutes = i64::from(seconds).abs() / 60;
    let sign = if seconds < 0 { '-' } else { '+' };
    Some(format!("{sign}{:02}:{:02}", minutes / 60, minutes % 60))
}

/// Plan the embed-path write. Per-tag gates:
///
/// - datetime / GPS: only when the flag is on AND the file has no existing
///   value (probe gate preserves camera-supplied data). Explicit capture-time
///   repair may replace an existing datetime only with a usable matching offset.
/// - offset: only alongside a timestamp this pass writes, or one the probe
///   proves already renders the capture-local instant.
/// - rating / description: flag gate only - iCloud is the source of truth.
/// - Title and keywords: gated on `EMBED_XMP`; people additionally require
///   `INCLUDE_PEOPLE`. Payload construction gates album enrichment.
#[cfg(test)]
fn plan_metadata_write(
    flags: MetadataFlags,
    payload: &MetadataPayload,
    created_local: &DateTime<FixedOffset>,
    probe: &crate::download::metadata::ExifProbe,
) -> crate::download::metadata::MetadataWrite {
    plan_metadata_write_with_repair(
        flags,
        payload,
        created_local,
        CaptureTimestampRepair::Preserve,
        probe,
    )
}

pub(super) fn plan_metadata_write_with_repair(
    flags: MetadataFlags,
    payload: &MetadataPayload,
    created_local: &DateTime<FixedOffset>,
    capture_timestamp_repair: CaptureTimestampRepair,
    probe: &crate::download::metadata::ExifProbe,
) -> crate::download::metadata::MetadataWrite {
    let mut write = crate::download::metadata::MetadataWrite::default();
    write.prior_embedded.clone_from(&payload.prior_embedded);
    if flags.contains(MetadataFlags::GPS) && payload.observed_absent & embedded_absence::GPS != 0 {
        write
            .clear_embedded
            .gps
            .clone_from(&payload.prior_embedded.gps);
        write
            .clear_embedded
            .native_gps
            .clone_from(&payload.prior_embedded.native_gps);
    }
    if flags.contains(MetadataFlags::RATING)
        && payload.observed_absent & embedded_absence::RATING != 0
    {
        write
            .clear_embedded
            .rating_percent
            .clone_from(&payload.prior_embedded.rating_percent);
        write
            .clear_embedded
            .rating
            .clone_from(&payload.prior_embedded.rating);
    }
    if flags.contains(MetadataFlags::DESCRIPTION)
        && payload.observed_absent & embedded_absence::DESCRIPTION != 0
    {
        write
            .clear_embedded
            .description
            .clone_from(&payload.prior_embedded.description);
        write
            .clear_embedded
            .native_description
            .clone_from(&payload.prior_embedded.native_description);
    }
    if flags.contains(MetadataFlags::EMBED_XMP) {
        if payload.observed_absent & embedded_absence::TITLE != 0 {
            write
                .clear_embedded
                .title
                .clone_from(&payload.prior_embedded.title);
        }
        if payload.observed_absent & embedded_absence::KEYWORDS != 0 && payload.keywords.is_empty()
        {
            write
                .clear_embedded
                .keywords
                .clone_from(&payload.prior_embedded.keywords);
        }
        if flags.contains(MetadataFlags::INCLUDE_PEOPLE)
            && payload.observed_absent & embedded_absence::PEOPLE != 0
            && payload.people.is_empty()
        {
            write
                .clear_embedded
                .people
                .clone_from(&payload.prior_embedded.people);
        }
    }
    if flags.contains(MetadataFlags::ORIENTATION) && probe.orientation_missing {
        write.orientation = payload.original_orientation;
    }

    if flags.contains(MetadataFlags::DATETIME) {
        let offset_time_original = offset_time_original(payload);
        write.require_native_heif_capture_time = matches!(
            capture_timestamp_repair,
            CaptureTimestampRepair::ReplaceWithCaptureLocal
        ) && offset_time_original.is_some();
        let replace_existing = matches!(
            capture_timestamp_repair,
            CaptureTimestampRepair::ReplaceWithCaptureLocal
        ) && offset_time_original.as_deref().is_some_and(
            |expected_offset| {
                probe.datetime_original.is_some()
                    && (!probe.denotes_capture_time(created_local)
                        || probe.offset_time_original.as_deref() != Some(expected_offset))
            },
        );
        if probe.datetime_original.is_none() || replace_existing {
            write.datetime = Some(created_local.format("%Y:%m:%d %H:%M:%S").to_string());
            let millis = created_local.timestamp_subsec_millis();
            write.datetime_subseconds = (millis != 0).then(|| format!("{millis:03}"));
            write.clear_datetime_offsets = probe.has_any_datetime_offset();
        }
        // An offset describes one specific timestamp. Attach it only to a
        // timestamp this pass writes, or to one already proven to render the
        // capture-local instant.
        if write.datetime.is_some()
            || (probe.offset_time_original.is_none() && probe.denotes_capture_time(created_local))
        {
            write.offset_time_original = offset_time_original;
        }
    }
    if flags.contains(MetadataFlags::RATING) {
        write.rating = payload.rating;
    }
    if flags.contains(MetadataFlags::GPS) && !probe.has_gps {
        write.gps = gps_from_payload(payload);
    }
    if flags.contains(MetadataFlags::DESCRIPTION)
        && payload.observed_absent & embedded_absence::DESCRIPTION == 0
    {
        write.description = payload
            .description
            .as_ref()
            .filter(|value| !value.is_empty())
            .or_else(|| payload.title.as_ref().filter(|value| !value.is_empty()))
            .cloned();
    }
    #[cfg(feature = "xmp")]
    if flags.contains(MetadataFlags::EMBED_XMP) {
        if payload.observed_absent & embedded_absence::TITLE == 0 {
            write.title = payload
                .title
                .as_ref()
                .filter(|value| !value.is_empty())
                .cloned();
        }
        write.keywords.clone_from(&payload.keywords);
        if flags.contains(MetadataFlags::INCLUDE_PEOPLE) {
            write.people.clone_from(&payload.people);
        }
    }

    write
}

#[cfg(test)]
mod tests;
