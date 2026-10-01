//! Metadata values shared by probes and writers.

/// GPS triple passed to the embedded metadata writer.
#[derive(Debug, Clone, Copy)]
pub(crate) struct GpsCoords {
    pub(crate) latitude: f64,
    pub(crate) longitude: f64,
    pub(crate) altitude: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct XmpRational {
    pub(super) numerator: u32,
    pub(super) denominator: u32,
}

#[cfg(feature = "xmp")]
impl XmpRational {
    pub(super) fn as_f64(&self) -> f64 {
        f64::from(self.numerator) / f64::from(self.denominator)
    }

    pub(super) fn encode(&self) -> String {
        format!("{}/{}", self.numerator, self.denominator)
    }
}

#[cfg(feature = "xmp")]
#[derive(Debug, Default, PartialEq)]
pub(crate) struct SourceGpsMetadata {
    pub(crate) latitude: Option<f64>,
    pub(crate) longitude: Option<f64>,
    pub(crate) datetime: Option<String>,
    pub(crate) speed: Option<XmpRational>,
    pub(crate) speed_ref: Option<String>,
    pub(crate) horizontal_positioning_error: Option<XmpRational>,
}

/// Bundle of every field the writer knows how to embed. Empty / default
/// fields are skipped.
#[derive(Debug, Default, Clone)]
pub(crate) struct MetadataWrite {
    /// Unzoned datetime: `YYYY:MM:DD HH:MM:SS` for embedded writes, or
    /// `YYYY-MM-DDTHH:MM:SS` with optional fractional seconds for sidecars.
    /// `offset_time_original` supplies the XMP zone separately.
    pub(crate) datetime: Option<String>,
    /// EXIF `OffsetTimeOriginal`, formatted as `+HH:MM` or `-HH:MM`.
    pub(crate) offset_time_original: Option<String>,
    /// Remove offset tags before writing a replacement timestamp.
    pub(crate) clear_datetime_offsets: bool,
    /// Require a native HEIF Exif timestamp pair to be updated or verified.
    pub(crate) require_native_heif_capture_time: bool,
    /// GPS fix timestamp in ISO 8601 UTC form.
    pub(crate) gps_datetime: Option<String>,
    /// GPS receiver speed as an XMP rational in the units named by `gps_speed_ref`.
    pub(crate) gps_speed: Option<XmpRational>,
    /// GPS receiver speed units: K, M, or N.
    pub(crate) gps_speed_ref: Option<String>,
    /// Horizontal positioning error in metres as an XMP rational.
    pub(crate) gps_h_positioning_error: Option<XmpRational>,
    /// Preserve prior kei-owned source GPS fields because the source could not
    /// be read and their current absence is unknown.
    pub(crate) preserve_source_gps: bool,
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
            && self.offset_time_original.is_none()
            && !self.clear_datetime_offsets
            && self.gps_datetime.is_none()
            && self.gps_speed.is_none()
            && self.gps_speed_ref.is_none()
            && self.gps_h_positioning_error.is_none()
            && !self.preserve_source_gps
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
