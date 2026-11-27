//! Metadata payloads and preloaded album/people enrichment.

/// Metadata values surfaced on a `DownloadTask` for write-out to embedded XMP
/// / native EXIF / XMP sidecars.
///
/// Carried separately from the rest of `AssetMetadata` so the download layer
/// only sees fields a writer can actually use. Fields are owned (not borrowed)
/// because the task moves across async boundaries.
#[derive(serde::Serialize, Debug, Clone, Default)]
#[cfg_attr(not(feature = "xmp"), allow(dead_code))]
pub(in crate::download) struct MetadataPayload {
    #[serde(skip)]
    pub(in crate::download) prior_embedded: crate::state::types::EmbeddedMetadataFields,
    #[serde(skip_serializing_if = "is_zero")]
    pub(in crate::download) observed_absent: u8,
    /// Provider-qualified orientation for these exact untransformed original bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(in crate::download) original_orientation: Option<u8>,
    /// Source capture offset in seconds from UTC, when valid.
    pub(in crate::download) timezone_offset: Option<i32>,
    /// 1-5 star rating (mapped from `AssetMetadata::rating` or `is_favorite`).
    pub(in crate::download) rating: Option<u8>,
    /// GPS latitude in decimal degrees, WGS84.
    pub(in crate::download) latitude: Option<f64>,
    /// GPS longitude in decimal degrees, WGS84.
    pub(in crate::download) longitude: Option<f64>,
    /// GPS altitude in meters above sea level.
    pub(in crate::download) altitude: Option<f64>,
    /// Short title / caption.
    pub(in crate::download) title: Option<String>,
    /// Image description text (prefers `description`, falls back to `title`).
    pub(in crate::download) description: Option<String>,
    /// `dc:subject` tags - source keywords plus album memberships merge here.
    pub(in crate::download) keywords: Vec<String>,
    /// MWG-RS person names for `iptcExt:PersonInImage`.
    pub(in crate::download) people: Vec<String>,
    /// Hidden from the timeline at the source.
    pub(in crate::download) is_hidden: bool,
    /// Archived at the source.
    pub(in crate::download) is_archived: bool,
    /// Media subtype (panorama, screenshot, burst, slo_mo, …).
    pub(in crate::download) media_subtype: Option<String>,
    /// Opaque source burst grouping id.
    pub(in crate::download) burst_id: Option<String>,
}

impl MetadataPayload {
    /// Build from `AssetMetadata`. Description falls back to title when
    /// `description` is unset. Keywords are parsed from the JSON array blob
    /// leniently — a malformed blob yields an empty list rather than an error.
    pub(in crate::download) fn from_metadata(meta: &crate::state::AssetMetadata) -> Self {
        let description = meta.description.as_ref().or(meta.title.as_ref()).cloned();
        let keywords = meta
            .keywords
            .as_deref()
            .and_then(|s| match serde_json::from_str::<Vec<String>>(s) {
                Ok(v) => Some(v),
                Err(e) => {
                    tracing::warn!(target: "kei::download::filter", error = %e, raw = %s, "Failed to parse keywords JSON");
                    None
                }
            })
            .unwrap_or_default();
        Self {
            prior_embedded: Default::default(),
            observed_absent: crate::icloud::photos::metadata::explicit_metadata_absence(meta),
            original_orientation: None,
            timezone_offset: meta.timezone_offset,
            rating: meta.rating,
            latitude: meta.latitude,
            longitude: meta.longitude,
            altitude: meta.altitude,
            title: meta.title.clone(),
            description,
            keywords,
            people: Vec::new(),
            is_hidden: meta.is_hidden,
            is_archived: meta.is_archived,
            media_subtype: meta.media_subtype.clone(),
            burst_id: meta.burst_id.clone(),
        }
    }

    /// Merge album names into `keywords` (as `dc:subject` tags — the standard
    /// XMP slot photo managers scan for groupings) and set `people`.
    pub(in crate::download) fn with_asset_groupings(
        mut self,
        albums: &[String],
        people: &[String],
        (include_albums, include_people): (bool, bool),
    ) -> Self {
        // Linear scan: typical cardinalities are <10 each, so a HashSet
        // rebuild costs more than it saves.
        if include_albums {
            for album in albums {
                if !self.keywords.iter().any(|k| k == album) {
                    self.keywords.push(album.clone());
                }
            }
        }
        // Skip the allocation when people is empty (common: libraries
        // without face tagging never populate this side of the groupings).
        if include_people && !people.is_empty() {
            self.people = people.to_vec();
        }
        self
    }
}

fn is_zero(value: &u8) -> bool {
    *value == 0
}
