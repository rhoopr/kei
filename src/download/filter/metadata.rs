//! Metadata payloads and preloaded album/people enrichment.

use std::sync::Arc;

use rustc_hash::FxHashMap;

use crate::download::DownloadConfig;

/// Metadata values surfaced on a `DownloadTask` for write-out to embedded XMP
/// / native EXIF / XMP sidecars.
///
/// Carried separately from the rest of `AssetMetadata` so the download layer
/// only sees fields a writer can actually use. Fields are owned (not borrowed)
/// because the task moves across async boundaries.
#[derive(Debug, Clone, Default)]
#[cfg_attr(not(feature = "xmp"), allow(dead_code))]
pub(in crate::download) struct MetadataPayload {
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
    ) -> Self {
        // Linear scan: typical cardinalities are <10 each, so a HashSet
        // rebuild costs more than it saves.
        for album in albums {
            if !self.keywords.iter().any(|k| k == album) {
                self.keywords.push(album.clone());
            }
        }
        // Skip the allocation when people is empty (common: libraries
        // without face tagging never populate this side of the groupings).
        if !people.is_empty() {
            self.people = people.to_vec();
        }
        self
    }
}

/// Index of per-asset album memberships and face-tag names, preloaded from
/// the state DB at sync start so `filter_asset_to_tasks` can enrich each
/// task's [`MetadataPayload`] without per-asset DB hits.
#[derive(Debug, Default)]
pub(crate) struct AssetGroupings {
    pub(crate) albums: FxHashMap<String, Vec<String>>,
    pub(crate) people: FxHashMap<String, Vec<String>>,
}

impl AssetGroupings {
    pub(in crate::download) fn metadata_payload(
        &self,
        asset_id: &str,
        metadata: &crate::state::AssetMetadata,
    ) -> MetadataPayload {
        let albums = self.albums.get(asset_id).map(Vec::as_slice).unwrap_or(&[]);
        let people = self.people.get(asset_id).map(Vec::as_slice).unwrap_or(&[]);
        MetadataPayload::from_metadata(metadata).with_asset_groupings(albums, people)
    }
}

pub(super) fn build_payload(
    asset: &crate::icloud::photos::PhotoAsset,
    config: &DownloadConfig,
) -> Arc<MetadataPayload> {
    let mut payload = config
        .asset_groupings
        .metadata_payload(asset.state_id(), asset.metadata());
    // The current pass is newer evidence than the cycle's grouping preload.
    if let Some(album) = config.album_name.as_deref().filter(|name| !name.is_empty())
        && !payload.keywords.iter().any(|keyword| keyword == album)
    {
        payload.keywords.push(album.to_owned());
    }
    Arc::new(payload)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::test_helpers::TestPhotoAsset;

    use super::super::test_support::test_config;
    use super::{AssetGroupings, MetadataPayload, build_payload};

    // ── MetadataPayload + AssetGroupings tests ─────────────────────────

    fn asset_metadata_with_keywords(keywords_json: &str) -> crate::state::AssetMetadata {
        crate::state::AssetMetadata {
            title: Some("Beach day".to_string()),
            description: Some("Sunny afternoon".to_string()),
            keywords: Some(keywords_json.to_string()),
            rating: Some(4),
            latitude: Some(37.7),
            longitude: Some(-122.4),
            altitude: Some(10.0),
            is_hidden: true,
            is_archived: false,
            media_subtype: Some("portrait".to_string()),
            burst_id: Some("burst-1".to_string()),
            ..crate::state::AssetMetadata::default()
        }
    }

    #[test]
    fn metadata_payload_parses_keywords_json() {
        let meta = asset_metadata_with_keywords(r#"["vacation","beach","sun"]"#);
        let p = MetadataPayload::from_metadata(&meta);
        assert_eq!(
            p.keywords,
            vec!["vacation".to_string(), "beach".into(), "sun".into()]
        );
    }

    #[test]
    fn metadata_payload_keywords_are_empty_on_bad_json() {
        let meta = asset_metadata_with_keywords("not json");
        let p = MetadataPayload::from_metadata(&meta);
        assert!(
            p.keywords.is_empty(),
            "malformed keywords JSON must not poison payload"
        );
    }

    #[test]
    fn metadata_payload_description_falls_back_to_title() {
        let mut meta = asset_metadata_with_keywords("[]");
        meta.description = None;
        let p = MetadataPayload::from_metadata(&meta);
        assert_eq!(p.description, Some("Beach day".to_string()));
    }

    #[test]
    fn metadata_payload_carries_all_new_fields() {
        let meta = asset_metadata_with_keywords("[]");
        let p = MetadataPayload::from_metadata(&meta);
        assert_eq!(p.title, Some("Beach day".into()));
        assert!(p.is_hidden);
        assert!(!p.is_archived);
        assert_eq!(p.media_subtype, Some("portrait".into()));
        assert_eq!(p.burst_id, Some("burst-1".into()));
    }

    #[test]
    fn with_asset_groupings_merges_albums_into_keywords() {
        let meta = asset_metadata_with_keywords(r#"["sun"]"#);
        let p = MetadataPayload::from_metadata(&meta)
            .with_asset_groupings(&["Favorites".into(), "Trip".into()], &[]);
        assert_eq!(p.keywords, vec!["sun", "Favorites", "Trip"]);
    }

    #[test]
    fn with_asset_groupings_dedupes_existing_album_keywords() {
        let meta = asset_metadata_with_keywords(r#"["Favorites"]"#);
        let p = MetadataPayload::from_metadata(&meta)
            .with_asset_groupings(&["Favorites".into(), "Trip".into()], &[]);
        assert_eq!(
            p.keywords,
            vec!["Favorites", "Trip"],
            "album already in keywords must not appear twice"
        );
    }

    #[test]
    fn with_asset_groupings_populates_people() {
        let meta = asset_metadata_with_keywords("[]");
        let p = MetadataPayload::from_metadata(&meta)
            .with_asset_groupings(&[], &["Alice".into(), "Bob".into()]);
        assert_eq!(p.people, vec!["Alice", "Bob"]);
    }

    #[test]
    fn build_payload_reads_grouping_index_from_config() {
        let asset = TestPhotoAsset::new("GROUP_1").build();
        let mut groupings = AssetGroupings::default();
        groupings
            .albums
            .insert("GROUP_1".into(), vec!["Favorites".into()]);
        groupings
            .people
            .insert("GROUP_1".into(), vec!["Alice".into()]);
        let mut config = test_config();
        config.asset_groupings = Arc::new(groupings);
        let payload = build_payload(&asset, &config);
        assert!(payload.keywords.contains(&"Favorites".to_string()));
        assert_eq!(payload.people, vec!["Alice".to_string()]);
    }

    #[test]
    fn build_payload_is_empty_grouping_safe() {
        let asset = TestPhotoAsset::new("EMPTY_1").build();
        let config = test_config();
        let payload = build_payload(&asset, &config);
        assert!(payload.people.is_empty());
    }
}
