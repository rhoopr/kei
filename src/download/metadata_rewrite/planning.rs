//! Opt-in flags and embedded metadata write planning.

use crate::download::DownloadConfig;
use crate::download::filter::MetadataPayload;
use chrono::{DateTime, FixedOffset};

/// Whether an explicitly requested metadata repair may replace an existing
/// embedded capture timestamp.
#[must_use]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum CaptureTimestampRepair {
    #[default]
    Preserve,
    ReplaceWithCaptureLocal,
}

bitflags::bitflags! {
    /// Per-tag write toggles. `any_embed()` drives the `.part`-and-modify-before-rename
    /// flow; individual flags gate which fields get embedded into the media file.
    ///
    /// `EMBED_XMP` enables the XMP-only fields that have no native EXIF equivalent
    /// (title, keywords, people, hidden/archived, media subtype, burst id).
    /// `XMP_SIDECAR` is orthogonal - it writes a `.xmp` file next to the photo
    /// without touching the photo bytes.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub(in crate::download) struct MetadataFlags: u8 {
        const DATETIME    = 1 << 0;
        const RATING      = 1 << 1;
        const GPS         = 1 << 2;
        const DESCRIPTION = 1 << 3;
        const EMBED_XMP   = 1 << 4;
        const XMP_SIDECAR = 1 << 5;
    }
}

impl MetadataFlags {
    /// Set of flags that drive the `.part`-and-modify-before-rename flow.
    /// Sidecar writes happen after the rename so `XMP_SIDECAR` is excluded.
    /// Derived as `all() \ XMP_SIDECAR` so any future embed-style flag
    /// added to this type is automatically picked up.
    const EMBED_MASK: Self = Self::all().difference(Self::XMP_SIDECAR);

    /// Whether any flag needs the downloaded bytes to stay as a `.part` file
    /// for in-place metadata editing before the atomic rename.
    pub(in crate::download) fn any_embed(self) -> bool {
        self.intersects(Self::EMBED_MASK)
    }

    pub(in crate::download) fn has_any_write(self) -> bool {
        !self.is_empty()
    }

    pub(super) fn uses_xmp_groupings(self) -> bool {
        self.intersects(Self::EMBED_XMP | Self::XMP_SIDECAR)
    }
}

impl From<&DownloadConfig> for MetadataFlags {
    fn from(config: &DownloadConfig) -> Self {
        Self::from(&config.metadata)
    }
}

impl From<&crate::config::MetadataConfig> for MetadataFlags {
    fn from(metadata: &crate::config::MetadataConfig) -> Self {
        let mut flags = Self::empty();
        flags.set(Self::DATETIME, metadata.set_exif_datetime);
        flags.set(Self::RATING, metadata.set_exif_rating);
        flags.set(Self::GPS, metadata.set_exif_gps);
        flags.set(Self::DESCRIPTION, metadata.set_exif_description);
        #[cfg(feature = "xmp")]
        {
            flags.set(Self::EMBED_XMP, metadata.embed_xmp);
            flags.set(Self::XMP_SIDECAR, metadata.xmp_sidecar);
        }
        flags
    }
}

#[must_use]
pub(crate) fn writers_enabled(metadata: &crate::config::MetadataConfig) -> bool {
    MetadataFlags::from(metadata).has_any_write()
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
/// - XMP-only fields (title, keywords, people, hidden/archived,
///   media_subtype, burst_id): gated on the `EMBED_XMP` flag.
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
    if flags.contains(MetadataFlags::DESCRIPTION) {
        write.description.clone_from(&payload.description);
    }
    #[cfg(feature = "xmp")]
    if flags.contains(MetadataFlags::EMBED_XMP) {
        write.title.clone_from(&payload.title);
        write.keywords.clone_from(&payload.keywords);
        write.people.clone_from(&payload.people);
        write.is_hidden = payload.is_hidden;
        write.is_archived = payload.is_archived;
        write.media_subtype.clone_from(&payload.media_subtype);
        write.burst_id.clone_from(&payload.burst_id);
    }

    write
}

#[cfg(test)]
mod tests;
