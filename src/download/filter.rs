//! Asset filtering -- determines which iCloud assets need downloading by
//! applying content/date/filename filters, resolving local paths, and
//! detecting collisions with existing files or in-flight downloads.

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, FixedOffset, Local};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::icloud::photos::VersionsMap;
use crate::icloud::photos::types::AssetVersion;
use crate::state::{MediaType, VersionSizeKey};
use crate::types::{
    AssetItemType, AssetVersionSize, FileMatchPolicy, LivePhotoMode, LivePhotoMovFilenamePolicy,
    RawPolicy,
};

use super::DownloadConfig;
use super::file::FinalPublication;
use super::paths;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct MalformedTaskResource {
    pub(super) field: Box<str>,
    pub(super) reason: Box<str>,
}

/// Reason an asset was filtered out during content/metadata filtering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FilterReason {
    MalformedAsset,
    ExcludedAlbum,
    MediaType,
    LivePhoto,
    DateRange,
    Filename,
}

/// Case-insensitive glob matching options for filename exclusion patterns.
const GLOB_CASE_INSENSITIVE: glob::MatchOptions = glob::MatchOptions {
    case_sensitive: false,
    require_literal_separator: false,
    require_literal_leading_dot: false,
};

pub(crate) trait PathDerivationSource {
    fn directory(&self) -> &Path;
    fn folder_structure(&self) -> &str;
    fn resolution(&self) -> crate::types::PhotoResolution;
    fn media(&self) -> &crate::config::MediaSelection;
    fn skip_created_before(&self) -> Option<crate::config::CreatedDateFilter>;
    fn skip_created_after(&self) -> Option<crate::config::CreatedDateFilter>;
    fn live_photo_mode(&self) -> LivePhotoMode;
    fn live_resolution(&self) -> AssetVersionSize;
    fn live_photo_mov_filename_policy(&self) -> LivePhotoMovFilenamePolicy;
    fn edited(&self) -> bool;
    fn alternative(&self) -> bool;
    fn raw_policy(&self) -> RawPolicy;
    fn file_match_policy(&self) -> FileMatchPolicy;
    fn force_resolution(&self) -> bool;
    fn keep_unicode_in_filenames(&self) -> bool;
    fn filename_exclude(&self) -> &[glob::Pattern];
    fn album_name(&self) -> Option<&str>;
    fn exclude_asset_ids(&self) -> &FxHashSet<String>;
}

/// Path/filter settings needed to derive where sync would place an asset.
///
/// `import-existing` uses this instead of constructing a mostly inert
/// [`DownloadConfig`], so path matching cannot accidentally depend on
/// pipeline-only defaults like retry policy, state DB handles, concurrency,
/// sync mode, or bandwidth limiters.
#[derive(Debug, Clone)]
pub(crate) struct PathDerivationConfig {
    pub(crate) directory: Arc<Path>,
    pub(crate) folder_structure: String,
    pub(crate) folder_structure_albums: Arc<str>,
    pub(crate) folder_structure_smart_folders: Arc<str>,
    pub(crate) resolution: crate::types::PhotoResolution,
    pub(crate) media: crate::config::MediaSelection,
    pub(crate) skip_created_before: Option<crate::config::CreatedDateFilter>,
    pub(crate) skip_created_after: Option<crate::config::CreatedDateFilter>,
    pub(crate) live_photo_mode: LivePhotoMode,
    pub(crate) live_resolution: AssetVersionSize,
    pub(crate) live_photo_mov_filename_policy: LivePhotoMovFilenamePolicy,
    pub(crate) edited: bool,
    pub(crate) alternative: bool,
    pub(crate) raw_policy: RawPolicy,
    pub(crate) file_match_policy: FileMatchPolicy,
    pub(crate) force_resolution: bool,
    pub(crate) keep_unicode_in_filenames: bool,
    pub(crate) filename_exclude: Arc<[glob::Pattern]>,
    pub(crate) album_name: Option<Arc<str>>,
    pub(crate) library: Arc<str>,
    pub(crate) exclude_asset_ids: Arc<FxHashSet<String>>,
}

impl PathDerivationConfig {
    pub(crate) fn from_path_fields(
        directory: Arc<Path>,
        fields: crate::config::PathDerivationFields,
        media: crate::config::MediaSelection,
    ) -> Self {
        Self {
            directory,
            folder_structure: fields.folder_structure,
            folder_structure_albums: Arc::from(fields.folder_structure_albums.as_str()),
            folder_structure_smart_folders: Arc::from(
                fields.folder_structure_smart_folders.as_str(),
            ),
            resolution: fields.resolution,
            media,
            skip_created_before: None,
            skip_created_after: None,
            live_photo_mode: fields.live_photo_mode,
            live_resolution: fields.live_resolution.to_asset_version_size(),
            live_photo_mov_filename_policy: fields.live_photo_mov_filename_policy,
            edited: fields.edited,
            alternative: fields.alternative,
            raw_policy: fields.raw_policy,
            file_match_policy: fields.file_match_policy,
            force_resolution: fields.force_resolution,
            keep_unicode_in_filenames: fields.keep_unicode_in_filenames,
            filename_exclude: Arc::from(Vec::<glob::Pattern>::new()),
            album_name: None,
            library: Arc::from(crate::icloud::photos::PRIMARY_ZONE_NAME),
            exclude_asset_ids: Arc::new(FxHashSet::default()),
        }
    }

    /// Clone this path config for one resolved album/smart-folder/unfiled
    /// pass, mirroring [`DownloadConfig::with_pass`] without pulling in
    /// download-pipeline fields.
    pub(crate) fn with_pass(&self, pass: &crate::commands::AlbumPass) -> Self {
        let folder_structure = folder_structure_for_pass(
            &self.folder_structure,
            &self.folder_structure_albums,
            &self.folder_structure_smart_folders,
            &self.library,
            pass,
        );
        Self {
            album_name: Some(Arc::clone(&pass.album.name)),
            folder_structure,
            exclude_asset_ids: Arc::clone(&pass.exclude_ids),
            ..self.clone()
        }
    }

    /// Clone this config with a different CloudKit zone for `{library}` path
    /// expansion.
    pub(crate) fn with_library(&self, library: &str) -> Self {
        Self {
            library: Arc::from(library),
            ..self.clone()
        }
    }
}

/// Pick and expand the active template for one album/smart-folder/unfiled
/// pass. Shared by sync's [`DownloadConfig`] and import's
/// [`PathDerivationConfig`] so `{album}`, `{smart-folder}`, and `{library}`
/// expansion cannot drift between matching and downloading.
pub(super) fn folder_structure_for_pass(
    folder_structure: &str,
    folder_structure_albums: &str,
    folder_structure_smart_folders: &str,
    library: &str,
    pass: &crate::commands::AlbumPass,
) -> String {
    let template: &str = match pass.kind {
        crate::commands::PassKind::Album => folder_structure_albums,
        crate::commands::PassKind::SmartFolder => folder_structure_smart_folders,
        crate::commands::PassKind::Unfiled => folder_structure,
    };
    let name_ref = Some(pass.album.name.as_ref()).filter(|n: &&str| !n.is_empty());
    let category_expanded = paths::expand_named_token(template, pass.kind.token(), name_ref);
    // Apply `{library}` last with the path-friendly truncated zone name,
    // so callers see `SharedSync-A1B2C3D4/...` instead of the full UUID.
    // The state-DB key still uses the full zone name.
    let library_for_path = paths::truncate_library_zone(library);
    paths::expand_named_token(
        &category_expanded,
        paths::TOKEN_LIBRARY,
        Some(library_for_path),
    )
}

impl PathDerivationSource for PathDerivationConfig {
    fn directory(&self) -> &Path {
        &self.directory
    }

    fn folder_structure(&self) -> &str {
        &self.folder_structure
    }

    fn resolution(&self) -> crate::types::PhotoResolution {
        self.resolution
    }

    fn media(&self) -> &crate::config::MediaSelection {
        &self.media
    }

    fn skip_created_before(&self) -> Option<crate::config::CreatedDateFilter> {
        self.skip_created_before
    }

    fn skip_created_after(&self) -> Option<crate::config::CreatedDateFilter> {
        self.skip_created_after
    }

    fn live_photo_mode(&self) -> LivePhotoMode {
        self.live_photo_mode
    }

    fn live_resolution(&self) -> AssetVersionSize {
        self.live_resolution
    }

    fn live_photo_mov_filename_policy(&self) -> LivePhotoMovFilenamePolicy {
        self.live_photo_mov_filename_policy
    }

    fn edited(&self) -> bool {
        self.edited
    }

    fn alternative(&self) -> bool {
        self.alternative
    }

    fn raw_policy(&self) -> RawPolicy {
        self.raw_policy
    }

    fn file_match_policy(&self) -> FileMatchPolicy {
        self.file_match_policy
    }

    fn force_resolution(&self) -> bool {
        self.force_resolution
    }

    fn keep_unicode_in_filenames(&self) -> bool {
        self.keep_unicode_in_filenames
    }

    fn filename_exclude(&self) -> &[glob::Pattern] {
        &self.filename_exclude
    }

    fn album_name(&self) -> Option<&str> {
        self.album_name.as_deref()
    }

    fn exclude_asset_ids(&self) -> &FxHashSet<String> {
        &self.exclude_asset_ids
    }
}

impl PathDerivationSource for DownloadConfig {
    fn directory(&self) -> &Path {
        &self.directory
    }

    fn folder_structure(&self) -> &str {
        &self.folder_structure
    }

    fn resolution(&self) -> crate::types::PhotoResolution {
        self.resolution
    }

    fn media(&self) -> &crate::config::MediaSelection {
        &self.media
    }

    fn skip_created_before(&self) -> Option<crate::config::CreatedDateFilter> {
        self.skip_created_before
    }

    fn skip_created_after(&self) -> Option<crate::config::CreatedDateFilter> {
        self.skip_created_after
    }

    fn live_photo_mode(&self) -> LivePhotoMode {
        self.live_photo_mode
    }

    fn live_resolution(&self) -> AssetVersionSize {
        self.live_resolution
    }

    fn live_photo_mov_filename_policy(&self) -> LivePhotoMovFilenamePolicy {
        self.live_photo_mov_filename_policy
    }

    fn edited(&self) -> bool {
        self.edited
    }

    fn alternative(&self) -> bool {
        self.alternative
    }

    fn raw_policy(&self) -> RawPolicy {
        self.raw_policy
    }

    fn file_match_policy(&self) -> FileMatchPolicy {
        self.file_match_policy
    }

    fn force_resolution(&self) -> bool {
        self.force_resolution
    }

    fn keep_unicode_in_filenames(&self) -> bool {
        self.keep_unicode_in_filenames
    }

    fn filename_exclude(&self) -> &[glob::Pattern] {
        &self.filename_exclude
    }

    fn album_name(&self) -> Option<&str> {
        self.album_name.as_deref()
    }

    fn exclude_asset_ids(&self) -> &FxHashSet<String> {
        &self.exclude_asset_ids
    }
}

/// Determine the media type for an asset based on version size and item type.
pub(crate) fn determine_media_type(
    version_size: VersionSizeKey,
    asset: &crate::icloud::photos::PhotoAsset,
) -> MediaType {
    match version_size {
        VersionSizeKey::LiveOriginal
        | VersionSizeKey::LiveMedium
        | VersionSizeKey::LiveThumb
        | VersionSizeKey::LiveAdjusted => {
            if asset.item_type() == Some(AssetItemType::Image) {
                MediaType::LivePhotoVideo
            } else {
                MediaType::Video
            }
        }
        _ => {
            if asset.item_type() == Some(AssetItemType::Movie) {
                MediaType::Video
            } else if asset.is_live_photo() {
                MediaType::LivePhotoImage
            } else {
                MediaType::Photo
            }
        }
    }
}

/// A normalized path string for case-insensitive collision detection.
///
/// On case-insensitive filesystems (macOS, Windows), we need to detect collisions between
/// paths like `IMG_0996.mov` and `IMG_0996.MOV`. This stores the normalized (lowercased)
/// form as a `Box<str>` and implements `Borrow<str>` to enable zero-copy lookups.
///
/// Use `NormalizedPath::normalize()` for temporary lookup keys to avoid `PathBuf` cloning.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct NormalizedPath(Box<str>);

impl NormalizedPath {
    /// Create a new normalized path from a borrowed `Path`.
    /// For lookup operations, prefer `normalize()` to avoid `PathBuf` cloning.
    pub(super) fn new(path: &Path) -> Self {
        Self(Self::normalize(path).into_owned().into_boxed_str())
    }

    /// Normalize a path reference for map lookups.
    ///
    /// On case-insensitive systems (macOS, Windows), returns a lowercase copy.
    /// On case-sensitive systems (Linux), returns a borrowed view when possible.
    ///
    /// Use with `claimed_paths.contains_key(NormalizedPath::normalize(&path).as_ref())`
    /// to avoid allocating a `PathBuf` just for the lookup.
    pub(super) fn normalize(path: &Path) -> Cow<'_, str> {
        let s = path.to_string_lossy();
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            Cow::Owned(s.to_ascii_lowercase())
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            s
        }
    }
}

impl std::borrow::Borrow<str> for NormalizedPath {
    fn borrow(&self) -> &str {
        &self.0
    }
}

/// Metadata values surfaced on a `DownloadTask` for write-out to embedded XMP
/// / native EXIF / XMP sidecars.
///
/// Carried separately from the rest of `AssetMetadata` so the download layer
/// only sees fields a writer can actually use. Fields are owned (not borrowed)
/// because the task moves across async boundaries.
#[derive(Debug, Clone, Default)]
#[cfg_attr(not(feature = "xmp"), allow(dead_code))]
pub(super) struct MetadataPayload {
    /// Source capture offset in seconds from UTC, when valid.
    pub(super) timezone_offset: Option<i32>,
    /// 1-5 star rating (mapped from `AssetMetadata::rating` or `is_favorite`).
    pub(super) rating: Option<u8>,
    /// GPS latitude in decimal degrees, WGS84.
    pub(super) latitude: Option<f64>,
    /// GPS longitude in decimal degrees, WGS84.
    pub(super) longitude: Option<f64>,
    /// GPS altitude in meters above sea level.
    pub(super) altitude: Option<f64>,
    /// Short title / caption.
    pub(super) title: Option<String>,
    /// Image description text (prefers `description`, falls back to `title`).
    pub(super) description: Option<String>,
    /// `dc:subject` tags - source keywords plus album memberships merge here.
    pub(super) keywords: Vec<String>,
    /// MWG-RS person names for `iptcExt:PersonInImage`.
    pub(super) people: Vec<String>,
    /// Hidden from the timeline at the source.
    pub(super) is_hidden: bool,
    /// Archived at the source.
    pub(super) is_archived: bool,
    /// Media subtype (panorama, screenshot, burst, slo_mo, …).
    pub(super) media_subtype: Option<String>,
    /// Opaque source burst grouping id.
    pub(super) burst_id: Option<String>,
}

impl MetadataPayload {
    /// Build from `AssetMetadata`. Description falls back to title when
    /// `description` is unset. Keywords are parsed from the JSON array blob
    /// leniently — a malformed blob yields an empty list rather than an error.
    pub(super) fn from_metadata(meta: &crate::state::AssetMetadata) -> Self {
        let description = meta.description.as_ref().or(meta.title.as_ref()).cloned();
        let keywords = meta
            .keywords
            .as_deref()
            .and_then(|s| match serde_json::from_str::<Vec<String>>(s) {
                Ok(v) => Some(v),
                Err(e) => {
                    tracing::warn!(error = %e, raw = %s, "Failed to parse keywords JSON");
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
    pub(super) fn with_asset_groupings(mut self, albums: &[String], people: &[String]) -> Self {
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
    pub(super) fn metadata_payload(
        &self,
        asset_id: &str,
        metadata: &crate::state::AssetMetadata,
    ) -> MetadataPayload {
        let albums = self.albums.get(asset_id).map(Vec::as_slice).unwrap_or(&[]);
        let people = self.people.get(asset_id).map(Vec::as_slice).unwrap_or(&[]);
        MetadataPayload::from_metadata(metadata).with_asset_groupings(albums, people)
    }
}

fn build_payload(
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

/// A unit of work produced by the filter phase and consumed by the download phase.
///
/// Fields ordered for optimal memory layout:
/// - Heap types first (`Box<str>`, `PathBuf`, `MetadataPayload`)
/// - 8-byte primitives (u64)
/// - `DateTime` (12-16 bytes)
/// - 1-byte enum last
#[derive(Debug, Clone)]
pub(super) struct DownloadTask {
    // Heap types first
    pub(super) url: Box<str>,
    pub(super) download_path: PathBuf,
    pub(super) publication: FinalPublication,
    pub(super) checksum: Box<str>,
    /// iCloud asset ID for state tracking. Shared with the producer's
    /// dedup set and any deferred state writes via refcount bump.
    pub(super) asset_id: Arc<str>,
    /// Stable `CPLAsset.recordName` used to recover this task's selected state
    /// identity when cleanup re-enumeration returns a different sibling order.
    pub(super) asset_record_name: Arc<str>,
    /// CloudKit zone that owns this asset. Usually matches the pass config's
    /// library, but cross-zone album hydration can produce bounded assets
    /// from another zone while preserving the album pass context.
    pub(super) library: Arc<str>,
    /// Metadata fields surfaced from `AssetMetadata` for writer consumption.
    /// Behind `Arc` so `task.metadata.clone()` in the download hot path is a
    /// refcount bump instead of a deep clone of every `Vec<String>` inside.
    #[cfg_attr(not(feature = "xmp"), allow(dead_code))]
    pub(super) metadata: Arc<MetadataPayload>,
    // 8-byte primitives
    pub(super) size: u64,
    // DateTime
    pub(super) created_local: DateTime<FixedOffset>,
    // 1-byte enum
    /// Version size key for state tracking.
    pub(super) version_size: VersionSizeKey,
    /// Resolved media type at task-creation time. Carried on the task so
    /// the post-success site can split the run's downloaded count by
    /// photos vs videos without re-running `determine_media_type` (and
    /// without holding the heavier `PhotoAsset` reference past the filter
    /// stage).
    pub(super) media_type: MediaType,
}

impl DownloadTask {
    /// Project the task fields the recap renderer needs (basename of the
    /// download path, byte size, capture timestamp). Lives here because
    /// the path-to-filename and `created_local` source are private to
    /// this struct; keeps the success-arm call site a one-liner.
    pub(super) fn to_recap_asset(&self) -> super::recap::RecapAsset {
        super::recap::RecapAsset {
            filename: self
                .download_path
                .file_name()
                .and_then(|f| f.to_str())
                .unwrap_or("")
                .to_string(),
            bytes: self.size,
            created_local: self.created_local.with_timezone(&Local),
        }
    }
}

/// Borrowed view over a `VersionsMap` with an optional virtual swap of
/// the keys at two indices. Lets [`apply_raw_policy`] relabel the
/// `Original` / `Alternative` slots without cloning the version list.
#[derive(Debug, Clone, Copy)]
pub(super) struct VersionsView<'a> {
    versions: &'a VersionsMap,
    /// `(orig_idx, alt_idx)` when the keys at those indices should be
    /// presented swapped; iteration yields `Alternative` at `orig_idx`
    /// and `Original` at `alt_idx`. `None` means iterate as-is.
    swap: Option<(usize, usize)>,
}

impl<'a> VersionsView<'a> {
    fn borrowed(versions: &'a VersionsMap) -> Self {
        Self {
            versions,
            swap: None,
        }
    }

    fn swapped(versions: &'a VersionsMap, orig_idx: usize, alt_idx: usize) -> Self {
        Self {
            versions,
            swap: Some((orig_idx, alt_idx)),
        }
    }

    pub(super) fn iter(
        &self,
    ) -> impl Iterator<Item = (AssetVersionSize, &'a AssetVersion)> + 'a + use<'a> {
        let swap = self.swap;
        self.versions.iter().enumerate().map(move |(idx, (k, v))| {
            let key = match swap {
                Some((orig, _)) if idx == orig => AssetVersionSize::Alternative,
                Some((_, alt)) if idx == alt => AssetVersionSize::Original,
                _ => *k,
            };
            (key, v)
        })
    }

    pub(super) fn get(&self, key: AssetVersionSize) -> Option<&'a AssetVersion> {
        self.iter().find(|(k, _)| *k == key).map(|(_, v)| v)
    }
}

/// Apply the RAW alignment policy by virtually swapping Original and
/// Alternative versions when appropriate, matching Python's
/// `apply_raw_policy()`. Returns a borrowed view over the original map
/// regardless of swap outcome.
#[allow(
    clippy::indexing_slicing,
    reason = "orig_idx / alt_idx come from `enumerate()` over `versions`; \
              indexing back into `versions` is in-bounds by construction"
)]
fn apply_raw_policy(versions: &VersionsMap, policy: RawPolicy) -> VersionsView<'_> {
    if policy == RawPolicy::AsIs {
        return VersionsView::borrowed(versions);
    }

    let (orig_idx, alt_idx) =
        versions
            .iter()
            .enumerate()
            .fold((None, None), |(orig, alt), (idx, (k, _))| match k {
                AssetVersionSize::Original => (Some(idx), alt),
                AssetVersionSize::Alternative => (orig, Some(idx)),
                _ => (orig, alt),
            });

    let Some(alt_idx) = alt_idx else {
        return VersionsView::borrowed(versions);
    };

    let should_swap = match policy {
        RawPolicy::PreferRaw => versions[alt_idx].1.asset_type.contains("raw"),
        RawPolicy::PreferJpeg => {
            orig_idx.is_some_and(|idx| versions[idx].1.asset_type.contains("raw"))
        }
        RawPolicy::AsIs => false,
    };

    match (should_swap, orig_idx) {
        (true, Some(orig_idx)) => VersionsView::swapped(versions, orig_idx, alt_idx),
        _ => VersionsView::borrowed(versions),
    }
}

/// Task keys follow RAW policy; provider metadata keys never do.
pub(crate) fn metadata_for_selected_version(
    asset: &crate::icloud::photos::PhotoAsset,
    config: &(impl PathDerivationSource + ?Sized),
    version: VersionSizeKey,
) -> Arc<crate::state::AssetMetadata> {
    let provider_version = if apply_raw_policy(asset.versions(), config.raw_policy())
        .swap
        .is_some()
    {
        match version {
            VersionSizeKey::Original => VersionSizeKey::Alternative,
            VersionSizeKey::Alternative => VersionSizeKey::Original,
            _ => version,
        }
    } else {
        version
    };
    asset.metadata_arc(provider_version)
}

/// Historical original/alternative keys may have been swapped by RAW policy.
/// State resolves these candidates against its current checksums, not today's policy.
pub(crate) fn metadata_capture(
    asset: &crate::icloud::photos::PhotoAsset,
) -> crate::state::MetadataCapture {
    let mut renditions = asset.rendition_facts().to_vec();
    for (key, facts) in asset.rendition_facts() {
        let paired_key = match key {
            VersionSizeKey::Original => VersionSizeKey::Alternative,
            VersionSizeKey::Alternative => VersionSizeKey::Original,
            _ => continue,
        };
        renditions.push((paired_key, facts.clone()));
    }
    crate::state::MetadataCapture {
        shared: asset.shared_metadata_arc(),
        renditions: renditions.into(),
    }
}

/// Returns the reason this asset should be skipped by content/metadata
/// filters, or `None` if the asset passes all filters.
///
/// Callers must invoke this before `extract_skip_candidates` or
/// `filter_asset_to_tasks` to avoid redundant evaluation.
pub(crate) fn is_asset_filtered(
    asset: &crate::icloud::photos::PhotoAsset,
    config: &(impl PathDerivationSource + ?Sized),
) -> Option<FilterReason> {
    if !asset.has_valid_id() {
        tracing::warn!("Skipping malformed asset with empty CloudKit recordName");
        return Some(FilterReason::MalformedAsset);
    }
    if config
        .exclude_asset_ids()
        .contains(asset.asset_record_name())
        || config.exclude_asset_ids().contains(asset.id())
    {
        tracing::debug!(
            asset_id = %asset.id(),
            asset_record_name = %asset.asset_record_name(),
            "Skipping (excluded album asset)"
        );
        return Some(FilterReason::ExcludedAlbum);
    }
    if asset.is_live_photo() && !config.media().live_photos {
        tracing::debug!(asset_id = %asset.id(), "Skipping live photo (media filter)");
        return Some(FilterReason::MediaType);
    }
    if !config.media().videos && asset.item_type() == Some(AssetItemType::Movie) {
        tracing::debug!(asset_id = %asset.id(), "Skipping video (media filter)");
        return Some(FilterReason::MediaType);
    }
    if !config.media().photos
        && asset.item_type() == Some(AssetItemType::Image)
        && !asset.is_live_photo()
    {
        tracing::debug!(asset_id = %asset.id(), "Skipping photo (media filter)");
        return Some(FilterReason::MediaType);
    }
    if config.live_photo_mode() == LivePhotoMode::Skip && asset.is_live_photo() {
        tracing::debug!(asset_id = %asset.id(), "Skipping live photo (live_photo_mode=skip)");
        return Some(FilterReason::LivePhoto);
    }
    let created_utc = asset.created();
    let created_local = asset.created_local();
    if let Some(before) = config.skip_created_before()
        && before.excludes_before(created_utc, created_local.date_naive())
    {
        tracing::debug!(asset_id = %asset.id(), date = %created_local, "Skipping (before date range)");
        return Some(FilterReason::DateRange);
    }
    if let Some(after) = config.skip_created_after()
        && after.excludes_after(created_utc, created_local.date_naive())
    {
        tracing::debug!(asset_id = %asset.id(), date = %created_local, "Skipping (after date range)");
        return Some(FilterReason::DateRange);
    }
    // Only check filename exclusion when the asset has a real filename.
    // filter_asset_to_tasks separately handles fallback fingerprint filenames.
    if !config.filename_exclude().is_empty()
        && let Some(filename) = asset.filename()
        && config
            .filename_exclude()
            .iter()
            .any(|p| p.matches_with(filename, GLOB_CASE_INSENSITIVE))
    {
        tracing::debug!(asset_id = %asset.id(), filename, "Skipping (filename_exclude match)");
        return Some(FilterReason::Filename);
    }
    None
}

/// Lightweight pre-check: extract (`version_size`, checksum) pairs for an asset
/// after applying content/date filters but WITHOUT path resolution or disk I/O.
///
/// Returns the candidate versions that would be downloaded. Used by the early
/// skip gate to check the state DB before the expensive `filter_asset_to_tasks`.
/// Caller must check [`is_asset_filtered`] first.
pub(super) fn extract_skip_candidates<'a>(
    asset: &'a crate::icloud::photos::PhotoAsset,
    config: &(impl PathDerivationSource + ?Sized),
) -> Vec<(VersionSizeKey, &'a str)> {
    if !asset.has_valid_id() {
        return Vec::new();
    }

    let ctx = DerivationContext::build(asset, config);
    let mut result = Vec::with_capacity(5);
    let mut seen_urls = Vec::<&str>::with_capacity(4);

    if let Some((version, effective_size)) = select_primary(asset, config, &ctx) {
        seen_urls.push(version.url.as_ref());
        result.push((
            VersionSizeKey::from(effective_size),
            version.checksum.as_ref(),
        ));
    }
    if let Some((version, effective_size)) = select_edited_extra(config, &ctx, &seen_urls) {
        seen_urls.push(version.url.as_ref());
        result.push((
            VersionSizeKey::from(effective_size),
            version.checksum.as_ref(),
        ));
    }
    if let Some((version, effective_size)) =
        select_alternative_extra(asset, config, &ctx, &seen_urls)
    {
        seen_urls.push(version.url.as_ref());
        result.push((
            VersionSizeKey::from(effective_size),
            version.checksum.as_ref(),
        ));
    }
    if let Some((version, effective_size)) =
        select_live_edited_extra(asset, config, &ctx, &seen_urls)
    {
        seen_urls.push(version.url.as_ref());
        result.push((
            VersionSizeKey::from(effective_size),
            version.checksum.as_ref(),
        ));
    }
    if let Some((version, effective_size)) = select_mov_companion(asset, config, &ctx, &seen_urls) {
        result.push((
            VersionSizeKey::from(effective_size),
            version.checksum.as_ref(),
        ));
    }

    result
}

/// One file sync would write for an asset, with the metadata `import-existing`
/// needs to match it against the local filesystem.
#[derive(Debug, Clone)]
pub(crate) struct ExpectedAssetPath {
    /// Absolute path the file would land at, before any collision/dedup suffix.
    pub(crate) path: PathBuf,
    /// Byte size iCloud reports for this version. Used as the strict-match key.
    pub(crate) size: u64,
    /// iCloud-side checksum (CloudKit format, not SHA256).
    pub(crate) checksum: Box<str>,
    /// Signed CDN URL for the selected cloud version.
    pub(crate) url: Box<str>,
    /// Which version this is (Original, LiveOriginal, Medium, ...). Drives the
    /// state-DB row key and `MediaType` classification.
    pub(crate) version_size: VersionSizeKey,
}

/// Bare expected path for one version, before any on-disk / claimed-path
/// collision resolution. The single source of truth shared by
/// `expected_paths_for` (import) and `filter_asset_to_tasks` (sync).
#[derive(Debug, Clone)]
pub(super) struct DerivedPath {
    /// Absolute path the file would land at, before any dedup suffix.
    pub(super) path: PathBuf,
    /// Basename of `path`. Sync's collision layer uses this as the input
    /// to `add_dedup_suffix` / `insert_suffix` when colliding with an
    /// existing different-size file.
    pub(super) filename: String,
    /// CDN URL for the version. Carried so sync can build a `DownloadTask`
    /// without re-walking `asset.versions()`. Unused by import.
    pub(super) url: Box<str>,
    pub(super) checksum: Box<str>,
    pub(super) size: u64,
    pub(super) version_size: VersionSizeKey,
    /// True for the primary photo (where AM/PM whitespace variants matter
    /// when matching on disk), false for the MOV companion. Sync's
    /// collision layer threads this into `resolve_download_path`.
    pub(super) check_ampm_on_disk: bool,
}

/// Real filename if the asset has one, otherwise a deterministic
/// fingerprint synthesized from the asset id + first-version UTI. Borrowed
/// when real, owned when synthesized.
fn raw_filename(asset: &crate::icloud::photos::PhotoAsset) -> Cow<'_, str> {
    if let Some(f) = asset.filename() {
        Cow::Borrowed(f)
    } else {
        Cow::Owned(paths::generate_fingerprint_filename(
            asset.id(),
            first_version_asset_type(asset),
        ))
    }
}

fn first_version_asset_type(asset: &crate::icloud::photos::PhotoAsset) -> &str {
    asset
        .versions()
        .first()
        .map_or("", |(_, v)| v.asset_type.as_ref())
}

fn filename_stem_is_empty(filename: &str) -> bool {
    let cleaned = paths::clean_filename(filename);
    let stem = cleaned
        .rsplit_once('.')
        .map_or(cleaned.as_ref(), |(stem, _)| stem);
    stem.is_empty()
}

fn replace_empty_stem_with_fingerprint(
    asset_id: &str,
    asset_type: &str,
    filename: String,
) -> String {
    if !filename_stem_is_empty(&filename) {
        return filename;
    }

    let fallback = paths::generate_fingerprint_filename(asset_id, asset_type);
    let Some((fallback_stem, _)) = fallback.rsplit_once('.') else {
        return fallback;
    };
    let Some((_, ext)) = filename.rsplit_once('.').filter(|(_, ext)| !ext.is_empty()) else {
        return fallback;
    };
    format!("{fallback_stem}.{ext}")
}

fn mapped_version_filename(asset_id: &str, base_filename: &str, asset_type: &str) -> String {
    let mapped = paths::map_filename_extension(base_filename, asset_type);
    replace_empty_stem_with_fingerprint(asset_id, asset_type, mapped)
}

fn usable_asset_base_filename(
    asset: &crate::icloud::photos::PhotoAsset,
    ctx: &DerivationContext<'_>,
) -> String {
    replace_empty_stem_with_fingerprint(
        asset.id(),
        first_version_asset_type(asset),
        ctx.base_filename.clone(),
    )
}

/// Per-asset inputs that don't change between primary and MOV companion
/// derivation.
pub(super) struct DerivationContext<'a> {
    pub(super) base_filename: String,
    pub(super) created_local: DateTime<FixedOffset>,
    pub(super) versions: VersionsView<'a>,
}

impl<'a> DerivationContext<'a> {
    pub(super) fn build(
        asset: &'a crate::icloud::photos::PhotoAsset,
        config: &(impl PathDerivationSource + ?Sized),
    ) -> Self {
        let raw = raw_filename(asset);
        let base_filename: String = if config.keep_unicode_in_filenames() {
            raw.into_owned()
        } else {
            paths::remove_unicode_chars(&raw).into_owned()
        };
        Self {
            base_filename,
            created_local: asset.created_local(),
            versions: apply_raw_policy(asset.versions(), config.raw_policy()),
        }
    }

    fn get_version(&self, key: AssetVersionSize) -> Option<&'a AssetVersion> {
        self.versions.get(key)
    }
}

fn url_seen(version: &AssetVersion, seen_urls: &[&str]) -> bool {
    seen_urls.iter().any(|seen| *seen == version.url.as_ref())
}

fn select_primary<'a>(
    asset: &crate::icloud::photos::PhotoAsset,
    config: &(impl PathDerivationSource + ?Sized),
    ctx: &DerivationContext<'a>,
) -> Option<(&'a AssetVersion, AssetVersionSize)> {
    if matches!(
        config.live_photo_mode(),
        LivePhotoMode::Skip | LivePhotoMode::VideoOnly
    ) && asset.is_live_photo()
    {
        return None;
    }
    let requested = config.resolution().to_asset_version_size()?;
    let get_version = |key: &AssetVersionSize| ctx.get_version(*key);
    version_with_fallback(
        &get_version,
        requested,
        AssetVersionSize::Original,
        config.force_resolution(),
    )
}

fn select_edited_extra<'a>(
    config: &(impl PathDerivationSource + ?Sized),
    ctx: &DerivationContext<'a>,
    seen_urls: &[&str],
) -> Option<(&'a AssetVersion, AssetVersionSize)> {
    if !config.edited() {
        return None;
    }
    let key = AssetVersionSize::Adjusted;
    let version = ctx.get_version(key)?;
    (!url_seen(version, seen_urls)).then_some((version, key))
}

fn select_live_edited_extra<'a>(
    asset: &crate::icloud::photos::PhotoAsset,
    config: &(impl PathDerivationSource + ?Sized),
    ctx: &DerivationContext<'a>,
    seen_urls: &[&str],
) -> Option<(&'a AssetVersion, AssetVersionSize)> {
    if !config.edited() || !asset.is_live_photo() || asset.item_type() != Some(AssetItemType::Image)
    {
        return None;
    }
    let key = AssetVersionSize::LiveAdjusted;
    let version = ctx.get_version(key)?;
    (!url_seen(version, seen_urls)).then_some((version, key))
}

fn select_alternative_extra<'a>(
    asset: &crate::icloud::photos::PhotoAsset,
    config: &(impl PathDerivationSource + ?Sized),
    ctx: &DerivationContext<'a>,
    seen_urls: &[&str],
) -> Option<(&'a AssetVersion, AssetVersionSize)> {
    if !config.alternative() || asset.is_live_photo() {
        return None;
    }
    let version = ctx.get_version(AssetVersionSize::Alternative)?;
    (!url_seen(version, seen_urls)).then_some((version, AssetVersionSize::Alternative))
}

fn select_mov_companion<'a>(
    asset: &crate::icloud::photos::PhotoAsset,
    config: &(impl PathDerivationSource + ?Sized),
    ctx: &DerivationContext<'a>,
    seen_urls: &[&str],
) -> Option<(&'a AssetVersion, AssetVersionSize)> {
    if !matches!(
        config.live_photo_mode(),
        LivePhotoMode::Both | LivePhotoMode::VideoOnly
    ) {
        return None;
    }
    if asset.item_type() != Some(AssetItemType::Image) {
        return None;
    }
    let get_version = |key: &AssetVersionSize| ctx.get_version(*key);
    let selected = version_with_fallback(
        &get_version,
        config.live_resolution(),
        AssetVersionSize::LiveOriginal,
        config.force_resolution(),
    )?;
    (!url_seen(selected.0, seen_urls)).then_some(selected)
}

fn malformed_for_keys(
    asset: &crate::icloud::photos::PhotoAsset,
    keys: impl IntoIterator<Item = AssetVersionSize>,
) -> Option<MalformedTaskResource> {
    let malformed = asset.malformed_resources();
    keys.into_iter().find_map(|key| {
        malformed
            .iter()
            .find(|resource| resource.version_size == key)
            .map(|resource| MalformedTaskResource {
                field: resource.field.clone(),
                reason: resource.reason.clone(),
            })
    })
}

fn primary_candidate_keys(config: &(impl PathDerivationSource + ?Sized)) -> Vec<AssetVersionSize> {
    let mut keys = Vec::with_capacity(2);
    let Some(requested) = config.resolution().to_asset_version_size() else {
        return keys;
    };
    keys.push(requested);
    if !config.force_resolution() && requested != AssetVersionSize::Original {
        keys.push(AssetVersionSize::Original);
    }
    keys
}

fn live_candidate_keys(config: &(impl PathDerivationSource + ?Sized)) -> Vec<AssetVersionSize> {
    let mut keys = Vec::with_capacity(2);
    keys.push(config.live_resolution());
    if !config.force_resolution() && config.live_resolution() != AssetVersionSize::LiveOriginal {
        keys.push(AssetVersionSize::LiveOriginal);
    }
    keys
}

/// Return the malformed resource that explains a post-filter asset producing
/// no downloadable tasks, if CloudKit advertised a selected resource but made
/// it unusable.
pub(super) fn malformed_no_task_resource(
    asset: &crate::icloud::photos::PhotoAsset,
    config: &(impl PathDerivationSource + ?Sized),
) -> Option<MalformedTaskResource> {
    if !derive_expected_paths(asset, config).is_empty() {
        return None;
    }

    if (!matches!(
        config.live_photo_mode(),
        LivePhotoMode::Skip | LivePhotoMode::VideoOnly
    ) || !asset.is_live_photo())
        && let Some(resource) = malformed_for_keys(asset, primary_candidate_keys(config))
    {
        return Some(resource);
    }

    if matches!(
        config.live_photo_mode(),
        LivePhotoMode::Both | LivePhotoMode::VideoOnly
    ) && asset.item_type() == Some(AssetItemType::Image)
        && let Some(resource) = malformed_for_keys(asset, live_candidate_keys(config))
    {
        return Some(resource);
    }

    None
}

/// Build the primary `DerivedPath` (or `None` if no primary should be
/// emitted under this config — Skip-mode live photo, VideoOnly mode,
/// or no usable version under `force_resolution`).
pub(super) fn derive_primary(
    asset: &crate::icloud::photos::PhotoAsset,
    config: &(impl PathDerivationSource + ?Sized),
    ctx: &DerivationContext<'_>,
) -> Option<DerivedPath> {
    let (version, effective_size) = select_primary(asset, config, ctx)?;

    let mapped = mapped_version_filename(asset.state_id(), &ctx.base_filename, &version.asset_type);
    let sized = match effective_size {
        AssetVersionSize::Medium => paths::insert_suffix(&mapped, "medium"),
        AssetVersionSize::Thumb => paths::insert_suffix(&mapped, "thumb"),
        _ => mapped,
    };
    let filename = match config.file_match_policy() {
        FileMatchPolicy::NameId7 => paths::apply_name_id7(&sized, asset.state_id()),
        FileMatchPolicy::NameSizeDedupWithSuffix => sized,
    };
    let path = paths::local_download_path(
        config.directory(),
        config.folder_structure(),
        &ctx.created_local,
        &filename,
        config.album_name(),
    );

    Some(DerivedPath {
        path,
        filename,
        url: version.url.clone(),
        checksum: version.checksum.clone(),
        size: version.size,
        version_size: VersionSizeKey::from(effective_size),
        check_ampm_on_disk: true,
    })
}

fn boxed_url_seen(version: &AssetVersion, seen_urls: &[Box<str>]) -> bool {
    seen_urls
        .iter()
        .any(|seen| seen.as_ref() == version.url.as_ref())
}

fn derive_suffixed_extra(
    asset: &crate::icloud::photos::PhotoAsset,
    config: &(impl PathDerivationSource + ?Sized),
    ctx: &DerivationContext<'_>,
    version: &AssetVersion,
    key: AssetVersionSize,
    suffix: &str,
    check_ampm_on_disk: bool,
) -> DerivedPath {
    let mapped = mapped_version_filename(asset.state_id(), &ctx.base_filename, &version.asset_type);
    let suffixed = paths::insert_literal_suffix(&mapped, suffix);
    let filename = match config.file_match_policy() {
        FileMatchPolicy::NameId7 => paths::apply_name_id7(&suffixed, asset.state_id()),
        FileMatchPolicy::NameSizeDedupWithSuffix => suffixed,
    };
    let path = paths::local_download_path(
        config.directory(),
        config.folder_structure(),
        &ctx.created_local,
        &filename,
        config.album_name(),
    );
    DerivedPath {
        path,
        filename,
        url: version.url.clone(),
        checksum: version.checksum.clone(),
        size: version.size,
        version_size: VersionSizeKey::from(key),
        check_ampm_on_disk,
    }
}

pub(super) fn derive_edited_extra(
    asset: &crate::icloud::photos::PhotoAsset,
    config: &(impl PathDerivationSource + ?Sized),
    ctx: &DerivationContext<'_>,
    seen_urls: &[Box<str>],
) -> Option<DerivedPath> {
    if !config.edited() {
        return None;
    }
    let key = AssetVersionSize::Adjusted;
    let version = ctx.get_version(key)?;
    if boxed_url_seen(version, seen_urls) {
        return None;
    }
    Some(derive_suffixed_extra(
        asset, config, ctx, version, key, "_edited", true,
    ))
}

pub(super) fn derive_alternative_extra(
    asset: &crate::icloud::photos::PhotoAsset,
    config: &(impl PathDerivationSource + ?Sized),
    ctx: &DerivationContext<'_>,
    seen_urls: &[Box<str>],
) -> Option<DerivedPath> {
    if !config.alternative() || asset.is_live_photo() {
        return None;
    }
    let key = AssetVersionSize::Alternative;
    let version = ctx.get_version(key)?;
    if boxed_url_seen(version, seen_urls) {
        return None;
    }
    let suffix = if version.asset_type.contains("raw") {
        "_RAW"
    } else {
        "_alt"
    };
    Some(derive_suffixed_extra(
        asset, config, ctx, version, key, suffix, true,
    ))
}

pub(super) fn derive_live_edited_extra(
    asset: &crate::icloud::photos::PhotoAsset,
    config: &(impl PathDerivationSource + ?Sized),
    ctx: &DerivationContext<'_>,
    seen_urls: &[Box<str>],
) -> Option<DerivedPath> {
    if !config.edited() || !asset.is_live_photo() || asset.item_type() != Some(AssetItemType::Image)
    {
        return None;
    }
    let key = AssetVersionSize::LiveAdjusted;
    let version = ctx.get_version(key)?;
    if boxed_url_seen(version, seen_urls) {
        return None;
    }
    Some(derive_suffixed_extra(
        asset, config, ctx, version, key, "_edited", false,
    ))
}

/// Build the live-photo MOV companion `DerivedPath` (or `None` when no
/// MOV applies — non-image asset, Skip / ImageOnly mode, no live version
/// available).
///
/// `primary_effective_filename` is the filename the primary lands at:
/// import passes the *derived* primary filename (no collision yet);
/// sync passes the *resolved* primary filename (after dedup suffix, if
/// any), so a dedup'd primary keeps its MOV paired by filename stem.
pub(super) fn derive_mov_companion(
    asset: &crate::icloud::photos::PhotoAsset,
    config: &(impl PathDerivationSource + ?Sized),
    ctx: &DerivationContext<'_>,
    primary_effective_filename: Option<&str>,
) -> Option<DerivedPath> {
    let (live_version, effective_live_size) = select_mov_companion(asset, config, ctx, &[])?;

    let live_base = match config.file_match_policy() {
        FileMatchPolicy::NameId7 => {
            let base = usable_asset_base_filename(asset, ctx);
            paths::apply_name_id7(&base, asset.state_id())
        }
        FileMatchPolicy::NameSizeDedupWithSuffix => primary_effective_filename.map_or_else(
            || usable_asset_base_filename(asset, ctx),
            ToString::to_string,
        ),
    };
    let mov_filename = match config.live_photo_mov_filename_policy() {
        LivePhotoMovFilenamePolicy::Suffix => paths::live_photo_mov_path_suffix(&live_base),
        LivePhotoMovFilenamePolicy::Original => paths::live_photo_mov_path_original(&live_base),
    };
    let mov_path = paths::local_download_path(
        config.directory(),
        config.folder_structure(),
        &ctx.created_local,
        &mov_filename,
        config.album_name(),
    );

    Some(DerivedPath {
        path: mov_path,
        filename: mov_filename,
        url: live_version.url.clone(),
        checksum: live_version.checksum.clone(),
        size: live_version.size,
        version_size: VersionSizeKey::from(effective_live_size),
        check_ampm_on_disk: false,
    })
}

/// Compute the bare expected paths sync would produce for an asset under
/// the given config, without doing collision resolution or disk I/O.
///
/// Returns up to two entries: the primary version and an optional
/// live-photo MOV companion. Empty result means no version applies
/// (`force_resolution` + size unavailable, image-only asset under VideoOnly
/// mode, or live-photo Skip mode).
///
/// Caller must invoke [`is_asset_filtered`] first to apply content/date
/// filters; this function only handles version selection + filename
/// derivation.
pub(super) fn derive_expected_paths(
    asset: &crate::icloud::photos::PhotoAsset,
    config: &(impl PathDerivationSource + ?Sized),
) -> Vec<DerivedPath> {
    if !asset.has_valid_id() {
        return Vec::new();
    }

    let ctx = DerivationContext::build(asset, config);
    let mut out = Vec::<DerivedPath>::with_capacity(5);
    let mut seen_urls = Vec::<Box<str>>::with_capacity(4);
    let mut primary_index: Option<usize> = None;
    if let Some(p) = derive_primary(asset, config, &ctx) {
        seen_urls.push(p.url.clone());
        out.push(p);
        primary_index = Some(out.len() - 1);
    }
    if let Some(p) = derive_edited_extra(asset, config, &ctx, &seen_urls) {
        seen_urls.push(p.url.clone());
        out.push(p);
    }
    if let Some(p) = derive_alternative_extra(asset, config, &ctx, &seen_urls) {
        seen_urls.push(p.url.clone());
        out.push(p);
    }
    if let Some(p) = derive_live_edited_extra(asset, config, &ctx, &seen_urls) {
        seen_urls.push(p.url.clone());
        out.push(p);
    }
    let primary_filename = primary_index
        .and_then(|index| out.get(index))
        .map(|p| p.filename.as_str());
    if let Some(mov) = derive_mov_companion(asset, config, &ctx, primary_filename) {
        if seen_urls
            .iter()
            .any(|seen| seen.as_ref() == mov.url.as_ref())
        {
            return out;
        }
        out.push(mov);
    }
    out
}

/// Compute the file paths sync would produce for an asset under the given
/// config, mapped to the `ExpectedAssetPath` shape `import-existing`
/// consumes. Thin wrapper over [`derive_expected_paths`].
pub(crate) fn expected_paths_for(
    asset: &crate::icloud::photos::PhotoAsset,
    config: &(impl PathDerivationSource + ?Sized),
) -> Vec<ExpectedAssetPath> {
    derive_expected_paths(asset, config)
        .into_iter()
        .map(|d| ExpectedAssetPath {
            path: d.path,
            size: d.size,
            checksum: d.checksum,
            url: d.url,
            version_size: d.version_size,
        })
        .collect()
}

/// Look up a version by key, falling back to `fallback_key` when the requested
/// size is unavailable (unless `force_resolution` is set). Shared by both
/// `extract_skip_candidates` and `filter_asset_to_tasks`.
fn version_with_fallback<'a>(
    get_version: &dyn Fn(&AssetVersionSize) -> Option<&'a AssetVersion>,
    requested: AssetVersionSize,
    fallback: AssetVersionSize,
    force_resolution: bool,
) -> Option<(&'a AssetVersion, AssetVersionSize)> {
    match get_version(&requested) {
        Some(v) => Some((v, requested)),
        None if requested != fallback && !force_resolution => {
            get_version(&fallback).map(|v| (v, fallback))
        }
        _ => None,
    }
}

/// Pre-populate the `DirCache` for the asset's date-based parent directory
/// on the blocking threadpool, so that subsequent sync `DirCache` lookups
/// inside `filter_asset_to_tasks` are guaranteed cache-hits.
pub(super) async fn pre_ensure_asset_dir(
    dir_cache: &mut paths::DirCache,
    asset: &crate::icloud::photos::PhotoAsset,
    config: &DownloadConfig,
) {
    let created_local = asset.created_local();
    let parent = paths::local_download_dir(
        &config.directory,
        &config.folder_structure,
        &created_local,
        config.album_name.as_deref(),
    );
    dir_cache.ensure_dir_async(&parent).await;
}

/// How to resolve a path that collides with an existing file or in-flight download.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CollisionStrategy {
    /// Compare sizes to detect collisions and choose a deterministic alternate
    /// path. Same-name/same-size is not enough identity to skip; the caller's
    /// state-backed path check owns safe skips for already-downloaded assets.
    /// When `skip_zero_size` is true, a version with size 0 is treated as
    /// "size unknown" and never matches (always dedup).
    SizeDedup { skip_zero_size: bool },
    /// The file's identity is already encoded in the filename (name-id7).
    /// Any existing file at the path means "already downloaded" -- skip.
    SkipIfExists,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PathResolution {
    Download(PathBuf),
    Skip(PathBuf),
}

impl PathResolution {
    fn download_path(self) -> Option<PathBuf> {
        match self {
            Self::Download(path) => Some(path),
            Self::Skip(_) => None,
        }
    }

    fn effective_path(&self) -> &Path {
        match self {
            Self::Download(path) | Self::Skip(path) => path.as_path(),
        }
    }
}

/// Shared context for `resolve_download_path` -- groups the mutable/config
/// references that every call needs so the function stays under clippy's
/// argument limit.
#[derive(Debug)]
struct ResolveContext<'a> {
    config: &'a DownloadConfig,
    created_local: &'a DateTime<FixedOffset>,
    claimed_paths: &'a FxHashMap<NormalizedPath, u64>,
    dir_cache: &'a mut paths::DirCache,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CollisionFilenameKind {
    /// The policy's historical collision filename: size suffix for primary
    /// media, asset-id suffix for live-photo MOV companions.
    Default,
    /// Asset-id suffix. Used when a same-size collision cannot be proven to be
    /// the same asset/version by the state layer.
    AssetIdentity,
    /// Stable fallback when the plain asset-id path is also occupied.
    AssetIdentityOrdinal(u64),
}

fn size_or_identity_collision_filename(
    filename: &str,
    size: u64,
    asset_id: &str,
    kind: CollisionFilenameKind,
) -> String {
    match kind {
        CollisionFilenameKind::Default => paths::add_dedup_suffix(filename, size),
        CollisionFilenameKind::AssetIdentity => {
            paths::insert_asset_identity_suffix(filename, asset_id)
        }
        CollisionFilenameKind::AssetIdentityOrdinal(n) => {
            paths::insert_asset_identity_ordinal_suffix(filename, asset_id, n)
        }
    }
}

fn identity_collision_filename(
    filename: &str,
    asset_id: &str,
    kind: CollisionFilenameKind,
) -> String {
    match kind {
        CollisionFilenameKind::Default | CollisionFilenameKind::AssetIdentity => {
            paths::insert_asset_identity_suffix(filename, asset_id)
        }
        CollisionFilenameKind::AssetIdentityOrdinal(n) => {
            paths::insert_asset_identity_ordinal_suffix(filename, asset_id, n)
        }
    }
}

fn collision_path_for_filename(ctx: &ResolveContext<'_>, filename: &str) -> PathBuf {
    paths::local_download_path(
        &ctx.config.directory,
        &ctx.config.folder_structure,
        ctx.created_local,
        filename,
        ctx.config.album_name.as_deref(),
    )
}

fn first_available_collision_path(
    ctx: &mut ResolveContext<'_>,
    preferred: CollisionFilenameKind,
    make_filename: impl Fn(CollisionFilenameKind) -> String,
) -> PathBuf {
    let mut tried = Vec::<Box<str>>::with_capacity(4);

    for kind in [preferred, CollisionFilenameKind::AssetIdentity] {
        if let Some(path) = available_collision_path(ctx, &make_filename, kind, &mut tried) {
            return path;
        }
    }

    let mut ordinal = 2u64;
    loop {
        if let Some(path) = available_collision_path(
            ctx,
            &make_filename,
            CollisionFilenameKind::AssetIdentityOrdinal(ordinal),
            &mut tried,
        ) {
            return path;
        }
        ordinal = ordinal.checked_add(1).unwrap_or(2);
    }
}

fn available_collision_path(
    ctx: &mut ResolveContext<'_>,
    make_filename: &impl Fn(CollisionFilenameKind) -> String,
    kind: CollisionFilenameKind,
    tried: &mut Vec<Box<str>>,
) -> Option<PathBuf> {
    let filename = make_filename(kind);
    let path = collision_path_for_filename(ctx, &filename);
    let normalized = NormalizedPath::normalize(&path).into_owned();
    if tried.iter().any(|seen| seen.as_ref() == normalized) {
        return None;
    }

    let unavailable =
        ctx.dir_cache.exists(&path) || ctx.claimed_paths.contains_key(normalized.as_str());
    tried.push(normalized.into_boxed_str());
    if unavailable { None } else { Some(path) }
}

/// Resolve the final download path for a single version, handling on-disk
/// files, AM/PM whitespace variants, and in-flight claimed paths.
///
/// Returns [`PathResolution::Download`] when the file should be downloaded, or
/// [`PathResolution::Skip`] when an on-disk or in-flight path already covers it.
///
/// `check_ampm`: when true, also checks AM/PM whitespace variants on disk
/// (relevant for primary photos whose timestamps contain AM/PM).
///
/// `make_collision_filename`: called when a collision is detected. Returns the
/// deterministic alternate filename to try.
fn resolve_download_path(
    download_path: &Path,
    version_size: u64,
    asset_id: &str,
    strategy: CollisionStrategy,
    ctx: &mut ResolveContext<'_>,
    check_ampm: bool,
    make_collision_filename: impl Fn(CollisionFilenameKind) -> String,
    label: &str,
) -> PathResolution {
    // Check for the file on disk. For primary photos, also check AM/PM
    // whitespace variants (e.g., "1.40.01 PM.PNG" vs "1.40.01\u{202F}PM.PNG").
    let on_disk_match = ctx
        .dir_cache
        .file_size(download_path)
        .map(|size| (size, download_path.to_path_buf()))
        .or_else(|| {
            if !check_ampm {
                return None;
            }
            let variant = ctx.dir_cache.find_ampm_variant(download_path)?;
            Some((ctx.dir_cache.file_size(&variant).unwrap_or(0), variant))
        });

    // Determine whether the existing size (on disk or in-flight) is a match.
    // `source` is used only for log messages.
    let existing_match = if let Some((size, path)) = on_disk_match {
        Some((size, "on-disk", path))
    } else {
        let normalized = NormalizedPath::normalize(download_path);
        if let Some(&size) = ctx.claimed_paths.get(normalized.as_ref()) {
            Some((size, "in-flight", download_path.to_path_buf()))
        } else {
            None
        }
    };

    let Some((existing_size, source, matched_path)) = existing_match else {
        // Path is unclaimed -- use it directly.
        return PathResolution::Download(download_path.to_path_buf());
    };

    match strategy {
        CollisionStrategy::SkipIfExists => {
            if source == "on-disk" {
                tracing::info!(
                    asset_id,
                    path = %download_path.display(),
                    "Skipping {label}: file exists (name-id7)"
                );
            } else {
                tracing::info!(
                    asset_id,
                    path = %download_path.display(),
                    "Skipping {label}: path claimed in-flight (name-id7)"
                );
            }
            PathResolution::Skip(matched_path)
        }
        CollisionStrategy::SizeDedup { skip_zero_size } => {
            let sizes_match =
                (!skip_zero_size || version_size > 0) && existing_size == version_size;

            let preferred = if sizes_match {
                CollisionFilenameKind::AssetIdentity
            } else {
                CollisionFilenameKind::Default
            };
            let collision_path =
                first_available_collision_path(ctx, preferred, make_collision_filename);
            if source == "on-disk" {
                tracing::debug!(
                    asset_id,
                    path = %download_path.display(),
                    on_disk_size = existing_size,
                    expected_size = version_size,
                    collision_path = %collision_path.display(),
                    same_size = sizes_match,
                    "Resolved {label} path collision"
                );
            } else {
                tracing::debug!(
                    asset_id,
                    path = %download_path.display(),
                    claimed_size = existing_size,
                    expected_size = version_size,
                    collision_path = %collision_path.display(),
                    same_size = sizes_match,
                    "Resolved {label} {source} path collision"
                );
            }
            PathResolution::Download(collision_path)
        }
    }
}

/// Apply content filters (type, date range) and local existence check,
/// producing download tasks for assets that need fetching.
/// Returns up to two tasks: the primary photo/video and an optional live photo MOV.
///
/// The `claimed_paths` map tracks paths that have been claimed by earlier tasks
/// in the same download session, preventing race conditions where two assets
/// with the same filename both see "file doesn't exist" during concurrent downloads.
/// Caller must check [`is_asset_filtered`] first.
pub(super) fn filter_asset_to_tasks(
    asset: &crate::icloud::photos::PhotoAsset,
    config: &DownloadConfig,
    claimed_paths: &mut FxHashMap<NormalizedPath, u64>,
    dir_cache: &mut paths::DirCache,
) -> Vec<DownloadTask> {
    if !asset.has_valid_id() {
        return Vec::new();
    }

    // Sync-only fingerprint-fallback exclusion: when `asset.filename()` is
    // None, log the synthesized name and apply `filename_exclude` patterns
    // to it (`is_asset_filtered` only sees real filenames). Import never
    // populates `filename_exclude`.
    if asset.filename().is_none() {
        let fp = raw_filename(asset);
        tracing::info!(
            asset_id = %asset.id(),
            filename = %fp,
            "Using fingerprint fallback filename"
        );
        if config
            .filename_exclude
            .iter()
            .any(|p| p.matches_with(&fp, GLOB_CASE_INSENSITIVE))
        {
            tracing::debug!(
                asset_id = %asset.id(),
                filename = %fp,
                "Skipping (filename_exclude match on fallback)"
            );
            return Vec::new();
        }
    }

    let ctx = DerivationContext::build(asset, config);
    let payload = build_payload(asset, config);
    let mut tasks = Vec::with_capacity(5);
    let mut effective_primary_filename: Option<String> = None;
    let mut seen_urls = Vec::<Box<str>>::with_capacity(4);
    let task_library: Arc<str> = asset
        .source_zone()
        .map(Arc::from)
        .unwrap_or_else(|| Arc::clone(&config.library));

    if let Some(d) = derive_primary(asset, config, &ctx) {
        let strategy = match config.file_match_policy {
            FileMatchPolicy::NameId7 => CollisionStrategy::SkipIfExists,
            FileMatchPolicy::NameSizeDedupWithSuffix => CollisionStrategy::SizeDedup {
                skip_zero_size: true,
            },
        };

        let DerivedPath {
            path,
            filename,
            url,
            checksum,
            size,
            version_size,
            check_ampm_on_disk,
        } = d;
        let primary_resolution = {
            let mut rctx = ResolveContext {
                config,
                created_local: &ctx.created_local,
                claimed_paths,
                dir_cache,
            };
            resolve_download_path(
                &path,
                size,
                asset.state_id(),
                strategy,
                &mut rctx,
                check_ampm_on_disk,
                |kind| size_or_identity_collision_filename(&filename, size, asset.state_id(), kind),
                "asset",
            )
        };

        if let Some(stem) = primary_resolution
            .effective_path()
            .file_name()
            .and_then(|f| f.to_str())
        {
            effective_primary_filename = Some(stem.to_string());
        }
        if let Some(p) = primary_resolution.download_path() {
            claimed_paths.insert(NormalizedPath::new(&p), size);
            seen_urls.push(url.clone());
            tasks.push(DownloadTask {
                url,
                download_path: p,
                publication: FinalPublication::NoReplace,
                checksum,
                asset_id: asset.state_id_arc(),
                asset_record_name: asset.asset_record_name_arc(),
                library: Arc::clone(&task_library),
                metadata: Arc::clone(&payload),
                size,
                created_local: ctx.created_local,
                version_size,
                media_type: determine_media_type(version_size, asset),
            });
        }
    }

    type SyncExtraDeriver = for<'a> fn(
        &crate::icloud::photos::PhotoAsset,
        &DownloadConfig,
        &DerivationContext<'a>,
        &[Box<str>],
    ) -> Option<DerivedPath>;
    let extra_derivers: [SyncExtraDeriver; 3] = [
        derive_edited_extra,
        derive_alternative_extra,
        derive_live_edited_extra,
    ];
    for derive_extra in extra_derivers {
        let Some(d) = derive_extra(asset, config, &ctx, &seen_urls) else {
            continue;
        };
        let DerivedPath {
            path,
            filename,
            url,
            checksum,
            size,
            version_size,
            check_ampm_on_disk,
        } = d;
        let final_path = {
            let mut rctx = ResolveContext {
                config,
                created_local: &ctx.created_local,
                claimed_paths,
                dir_cache,
            };
            resolve_download_path(
                &path,
                size,
                asset.state_id(),
                CollisionStrategy::SizeDedup {
                    skip_zero_size: true,
                },
                &mut rctx,
                check_ampm_on_disk,
                |kind| size_or_identity_collision_filename(&filename, size, asset.state_id(), kind),
                "asset extra",
            )
            .download_path()
        };

        if let Some(p) = final_path {
            claimed_paths.insert(NormalizedPath::new(&p), size);
            seen_urls.push(url.clone());
            tasks.push(DownloadTask {
                url,
                download_path: p,
                publication: FinalPublication::NoReplace,
                checksum,
                asset_id: asset.state_id_arc(),
                asset_record_name: asset.asset_record_name_arc(),
                library: Arc::clone(&task_library),
                metadata: Arc::clone(&payload),
                size,
                created_local: ctx.created_local,
                version_size,
                media_type: determine_media_type(version_size, asset),
            });
        }
    }

    if let Some(d) =
        derive_mov_companion(asset, config, &ctx, effective_primary_filename.as_deref())
    {
        let DerivedPath {
            path,
            filename,
            url,
            checksum,
            size,
            version_size,
            check_ampm_on_disk,
        } = d;
        if seen_urls.iter().any(|seen| seen.as_ref() == url.as_ref()) {
            return tasks;
        }
        let asset_id = asset.state_id();
        let final_mov_path = {
            let mut rctx = ResolveContext {
                config,
                created_local: &ctx.created_local,
                claimed_paths,
                dir_cache,
            };
            resolve_download_path(
                &path,
                size,
                asset_id,
                CollisionStrategy::SizeDedup {
                    skip_zero_size: false,
                },
                &mut rctx,
                check_ampm_on_disk,
                |kind| identity_collision_filename(&filename, asset_id, kind),
                "live photo MOV",
            )
            .download_path()
        };

        if let Some(p) = final_mov_path {
            claimed_paths.insert(NormalizedPath::new(&p), size);
            seen_urls.push(url.clone());
            tasks.push(DownloadTask {
                url,
                download_path: p,
                publication: FinalPublication::NoReplace,
                checksum,
                asset_id: asset.state_id_arc(),
                asset_record_name: asset.asset_record_name_arc(),
                library: task_library,
                metadata: Arc::clone(&payload),
                size,
                created_local: ctx.created_local,
                version_size,
                media_type: determine_media_type(version_size, asset),
            });
        }
    }

    tasks
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use rustc_hash::FxHashSet;

    #[cfg(unix)]
    use crate::icloud::photos::PRIMARY_ZONE_NAME;
    use crate::icloud::photos::PhotoAsset;
    #[cfg(unix)]
    use crate::state::SqliteStateDb;
    #[cfg(unix)]
    use crate::test_helpers::TestAssetRecord;
    use crate::test_helpers::TestPhotoAsset;
    use crate::types::LivePhotoMode;
    use serde_json::json;
    use std::fs;
    use tempfile::TempDir;

    fn test_config() -> DownloadConfig {
        DownloadConfig::test_default()
    }

    /// Helper that calls filter_asset_to_tasks with a fresh claimed_paths map.
    /// Use this for simple tests that don't need to track paths across calls.
    fn filter_asset_fresh(asset: &PhotoAsset, config: &DownloadConfig) -> Vec<DownloadTask> {
        let mut claimed_paths = FxHashMap::default();
        let mut dir_cache = paths::DirCache::new();
        filter_asset_to_tasks(asset, config, &mut claimed_paths, &mut dir_cache)
    }

    #[cfg(unix)]
    const POST_RENAME_PRE_STATE_CHILD_ENV: &str = "KEI_POST_RENAME_PRE_STATE_SIGKILL_CHILD";
    #[cfg(unix)]
    const POST_RENAME_PRE_STATE_DOWNLOAD_DIR_ENV: &str = "KEI_CRASH_HARNESS_DOWNLOAD_DIR";
    #[cfg(unix)]
    const POST_RENAME_PRE_STATE_DB_ENV: &str = "KEI_CRASH_HARNESS_DB";
    #[cfg(unix)]
    const POST_RENAME_PRE_STATE_PART_ENV: &str = "KEI_CRASH_HARNESS_PART";
    #[cfg(unix)]
    const POST_RENAME_PRE_STATE_CHILD_TEST: &str =
        "download::filter::tests::post_rename_pre_state_sigkill_child";
    #[cfg(unix)]
    const POST_RENAME_PRE_STATE_ASSET_ID: &str = "ASSET_REAL_KILL";
    #[cfg(unix)]
    const POST_RENAME_PRE_STATE_FILENAME: &str = "IMG_REAL_KILL.JPG";
    #[cfg(unix)]
    const POST_RENAME_PRE_STATE_CHECKSUM: &str = "ck_real_kill";
    #[cfg(unix)]
    const POST_RENAME_PRE_STATE_PART_FILE: &str = "published-before-state.kei-tmp";

    #[cfg(unix)]
    fn post_rename_pre_state_crash_asset() -> PhotoAsset {
        TestPhotoAsset::new(POST_RENAME_PRE_STATE_ASSET_ID)
            .filename(POST_RENAME_PRE_STATE_FILENAME)
            .orig_size(1000)
            .orig_url("https://p01.icloud-content.com/real-kill")
            .orig_checksum(POST_RENAME_PRE_STATE_CHECKSUM)
            .build()
    }

    #[cfg(unix)]
    fn post_rename_pre_state_config(download_dir: &std::path::Path) -> DownloadConfig {
        let mut config = test_config();
        config.directory = Arc::from(download_dir);
        config
    }

    #[cfg(unix)]
    fn post_rename_pre_state_final_path_for(download_dir: &std::path::Path) -> std::path::PathBuf {
        let asset = post_rename_pre_state_crash_asset();
        let config = post_rename_pre_state_config(download_dir);
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 1);
        tasks[0].download_path.clone()
    }

    fn plain_photo_asset() -> PhotoAsset {
        TestPhotoAsset::new("TEST_1").build()
    }

    fn video_asset() -> PhotoAsset {
        TestPhotoAsset::new("VID_1")
            .filename("movie.mov")
            .item_type("com.apple.quicktime-movie")
            .orig_file_type("com.apple.quicktime-movie")
            .orig_size(50_000)
            .orig_url("https://p01.icloud-content.com/vid")
            .orig_checksum("vid_ck")
            .build()
    }

    fn aae_asset() -> PhotoAsset {
        TestPhotoAsset::new("EXCL_1")
            .filename("IMG_0001.AAE")
            .build()
    }

    fn lowercase_aae_asset() -> PhotoAsset {
        TestPhotoAsset::new("EXCL_2").filename("Photo.aae").build()
    }

    fn excluded_asset() -> PhotoAsset {
        TestPhotoAsset::new("EXCLUDED_1")
            .filename("IMG_0001.JPG")
            .build()
    }

    fn keep_asset() -> PhotoAsset {
        TestPhotoAsset::new("KEEP_1")
            .filename("IMG_0002.JPG")
            .build()
    }

    fn old_asset() -> PhotoAsset {
        TestPhotoAsset::new("OLD_1")
            .asset_date(1_592_179_200_000.0) // 2020-06-15T00:00:00Z
            .build()
    }

    fn new_asset() -> PhotoAsset {
        TestPhotoAsset::new("NEW_1")
            .asset_date(1_750_003_200_000.0) // 2025-06-15T00:00:00Z
            .build()
    }

    fn date_time(s: &str) -> crate::config::CreatedDateFilter {
        crate::config::CreatedDateFilter::Instant(
            DateTime::parse_from_rfc3339(s).expect("test date").into(),
        )
    }

    fn no_config_change(_: &mut DownloadConfig) {}

    fn skip_videos(config: &mut DownloadConfig) {
        config.media.videos = false;
    }

    fn skip_photos(config: &mut DownloadConfig) {
        config.media.photos = false;
    }

    fn skip_before_february_2025(config: &mut DownloadConfig) {
        config.skip_created_before = Some(date_time("2025-02-01T00:00:00Z"));
    }

    fn skip_after_january_2025(config: &mut DownloadConfig) {
        config.skip_created_after = Some(date_time("2025-01-01T00:00:00Z"));
    }

    fn skip_before_2024(config: &mut DownloadConfig) {
        config.skip_created_before = Some(date_time("2024-01-01T00:00:00Z"));
    }

    fn skip_after_2023(config: &mut DownloadConfig) {
        config.skip_created_after = Some(date_time("2023-01-01T00:00:00Z"));
    }

    fn skip_live_photos(config: &mut DownloadConfig) {
        config.live_photo_mode = LivePhotoMode::Skip;
    }

    fn live_photo_image_only(config: &mut DownloadConfig) {
        config.live_photo_mode = LivePhotoMode::ImageOnly;
    }

    fn live_photo_video_only(config: &mut DownloadConfig) {
        config.live_photo_mode = LivePhotoMode::VideoOnly;
    }

    fn medium_resolution_with_fallback(config: &mut DownloadConfig) {
        config.resolution = crate::types::PhotoResolution::Medium;
        config.force_resolution = false;
    }

    fn medium_resolution_without_fallback(config: &mut DownloadConfig) {
        config.resolution = crate::types::PhotoResolution::Medium;
        config.force_resolution = true;
    }

    fn live_adjusted_with_fallback(config: &mut DownloadConfig) {
        config.live_resolution = AssetVersionSize::LiveAdjusted;
        config.force_resolution = false;
    }

    fn live_adjusted_without_fallback(config: &mut DownloadConfig) {
        config.live_resolution = AssetVersionSize::LiveAdjusted;
        config.force_resolution = true;
    }

    fn exclude_aae_filenames(config: &mut DownloadConfig) {
        config.filename_exclude = Arc::from(vec![glob::Pattern::new("*.AAE").unwrap()]);
    }

    fn exclude_known_asset_id(config: &mut DownloadConfig) {
        let mut ids = FxHashSet::default();
        ids.insert("EXCLUDED_1".to_string());
        config.exclude_asset_ids = Arc::new(ids);
    }

    fn exclude_other_asset_id(config: &mut DownloadConfig) {
        let mut ids = FxHashSet::default();
        ids.insert("OTHER_ID".to_string());
        config.exclude_asset_ids = Arc::new(ids);
    }

    #[test]
    fn test_filter_asset_produces_task() {
        let asset = TestPhotoAsset::new("TEST_1").build();
        let config = test_config();
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 1);
        assert_eq!(&*tasks[0].url, "https://p01.icloud-content.com/orig");
        assert_eq!(&*tasks[0].checksum, "abc123");
        assert_eq!(tasks[0].size, 1000);
    }

    #[test]
    fn capture_local_date_filters_and_paths_cover_boundaries_and_fallbacks() {
        let check = |asset_date: f64,
                     offset: Option<i32>,
                     boundary: &str,
                     expected_filter: Option<FilterReason>| {
            let mut builder = TestPhotoAsset::new("TIMEZONE_CASE")
                .filename("photo.jpg")
                .asset_date(asset_date);
            if let Some(offset) = offset {
                builder = builder.timezone_offset(offset);
            }
            let asset = builder.build();
            let boundary = boundary.parse().unwrap();
            let mut config = test_config();
            config.skip_created_before =
                Some(crate::config::CreatedDateFilter::CaptureDate(boundary));
            config.skip_created_after =
                Some(crate::config::CreatedDateFilter::CaptureDate(boundary));

            assert_eq!(is_asset_filtered(&asset, &config), expected_filter);
            if expected_filter.is_none() {
                let tasks = filter_asset_fresh(&asset, &config);
                assert_eq!(tasks.len(), 1);
                let expected_path = format!("{}/photo.JPG", boundary.format("%Y/%m/%d"));
                assert!(
                    tasks[0].download_path.ends_with(&expected_path),
                    "{}",
                    tasks[0].download_path.display()
                );
            }
        };

        check(1_769_898_719_000.0, Some(39_600), "2026-02-01", None);
        check(1_767_227_400_000.0, Some(-3_600), "2025-12-31", None);
        check(
            1_769_898_719_000.0,
            Some(39_600),
            "2026-01-31",
            Some(FilterReason::DateRange),
        );
        let host_boundary = DateTime::from_timestamp_millis(1_736_899_200_000)
            .unwrap()
            .with_timezone(&Local)
            .date_naive()
            .to_string();
        check(1_736_899_200_000.0, None, &host_boundary, None);
        check(1_736_899_200_000.0, Some(i32::MAX), &host_boundary, None);
    }

    #[test]
    fn empty_record_name_is_filtered_before_path_planning() {
        let asset = PhotoAsset::new(
            json!({"recordName": "", "fields": {
                "filenameEnc": {"value": "photo.jpg", "type": "STRING"},
                "itemType": {"value": "public.jpeg"},
                "resOriginalRes": {"value": {
                    "size": 1000,
                    "downloadURL": "https://p01.icloud-content.com/orig",
                    "fileChecksum": "abc123"
                }},
                "resOriginalFileType": {"value": "public.jpeg"}
            }}),
            json!({"fields": {"assetDate": {"value": 1736899200000.0}}}),
        );
        let config = test_config();

        assert_eq!(
            is_asset_filtered(&asset, &config),
            Some(FilterReason::MalformedAsset)
        );
        assert!(
            extract_skip_candidates(&asset, &config).is_empty(),
            "invalid id must not enter state-skip decisions"
        );
        assert!(
            expected_paths_for(&asset, &config).is_empty(),
            "invalid id must not derive import/sync paths"
        );
        assert!(
            filter_asset_fresh(&asset, &config).is_empty(),
            "invalid id must not produce download tasks"
        );
    }

    // ── expected_paths_for tests ────────────────────────────────────────
    //
    // These cover `import-existing`'s view of sync's filename derivation:
    // file_match_policy, size suffix, live photo MOV companion, raw alignment,
    // force_resolution, keep_unicode. Sync's `filter_asset_to_tasks` is the source
    // of truth; collision/dedup-suffix handling is intentionally NOT replayed
    // here (callers don't have claimed_paths state to consult).

    #[test]
    fn expected_paths_default_returns_one_original_path() {
        let asset = TestPhotoAsset::new("TEST_1")
            .filename("IMG_0001.JPG")
            .build();
        let config = test_config();
        let paths = expected_paths_for(&asset, &config);
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].size, 1000);
        assert_eq!(&*paths[0].checksum, "abc123");
        assert_eq!(paths[0].version_size, VersionSizeKey::Original);
        assert!(
            paths[0].path.to_string_lossy().ends_with("IMG_0001.JPG"),
            "expected ...IMG_0001.JPG, got {}",
            paths[0].path.display()
        );
    }

    #[test]
    fn expected_paths_apply_name_id7_suffix_to_primary() {
        let asset = TestPhotoAsset::new("TEST_1")
            .filename("IMG_0001.JPG")
            .build();
        let mut config = test_config();
        config.file_match_policy = FileMatchPolicy::NameId7;
        let paths = expected_paths_for(&asset, &config);
        assert_eq!(paths.len(), 1);
        let name = paths[0].path.file_name().unwrap().to_string_lossy();
        assert!(
            name.starts_with("IMG_0001_") && name.ends_with(".JPG"),
            "expected IMG_0001_<id7>.JPG, got {name}"
        );
        assert_ne!(name, "IMG_0001.JPG", "id7 suffix not applied");
    }

    #[test]
    fn expected_paths_live_photo_yields_primary_and_mov() {
        let asset = TestPhotoAsset::new("LIVE_1")
            .filename("IMG_2000.HEIC")
            .item_type("public.heic")
            .orig_file_type("public.heic")
            .live_photo("https://p01.icloud-content.com/mov", "mov_ck", 3000)
            .build();
        let config = test_config();
        let paths = expected_paths_for(&asset, &config);
        assert_eq!(paths.len(), 2);
        assert_eq!(paths[0].version_size, VersionSizeKey::Original);
        assert_eq!(paths[1].version_size, VersionSizeKey::LiveOriginal);
        assert_eq!(paths[1].size, 3000);
        assert!(
            paths[1]
                .path
                .to_string_lossy()
                .ends_with("IMG_2000_HEVC.MOV"),
            "expected ...IMG_2000_HEVC.MOV, got {}",
            paths[1].path.display()
        );
    }

    #[test]
    fn expected_paths_video_only_skips_primary() {
        let asset = TestPhotoAsset::new("LIVE_2")
            .filename("IMG_2001.HEIC")
            .item_type("public.heic")
            .orig_file_type("public.heic")
            .live_photo("https://p01.icloud-content.com/mov", "mov_ck", 3000)
            .build();
        let mut config = test_config();
        config.live_photo_mode = LivePhotoMode::VideoOnly;
        let paths = expected_paths_for(&asset, &config);
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].version_size, VersionSizeKey::LiveOriginal);
    }

    #[test]
    fn expected_paths_image_only_skips_mov() {
        let asset = TestPhotoAsset::new("LIVE_3")
            .filename("IMG_2002.HEIC")
            .item_type("public.heic")
            .orig_file_type("public.heic")
            .live_photo("https://p01.icloud-content.com/mov", "mov_ck", 3000)
            .build();
        let mut config = test_config();
        config.live_photo_mode = LivePhotoMode::ImageOnly;
        let paths = expected_paths_for(&asset, &config);
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].version_size, VersionSizeKey::Original);
    }

    /// `LivePhotoMode::Skip` is documented as "skip live photos entirely (both
    /// image and MOV)." A live-photo asset under Skip must yield no paths so
    /// import-existing doesn't scan for files sync never wrote.
    #[test]
    fn expected_paths_skip_mode_emits_nothing_for_live_photo() {
        let asset = TestPhotoAsset::new("LIVE_4")
            .filename("IMG_2003.HEIC")
            .item_type("public.heic")
            .orig_file_type("public.heic")
            .live_photo("https://p01.icloud-content.com/mov", "mov_ck", 3000)
            .build();
        let mut config = test_config();
        config.live_photo_mode = LivePhotoMode::Skip;
        let paths = expected_paths_for(&asset, &config);
        assert!(
            paths.is_empty(),
            "Skip + live photo must drop the asset, got {paths:?}"
        );
    }

    /// Skip applies only to live photos: a non-live asset under Skip still
    /// produces its primary path.
    #[test]
    fn expected_paths_skip_mode_keeps_non_live_primary() {
        let asset = TestPhotoAsset::new("STILL_1")
            .filename("IMG_0001.JPG")
            .build();
        let mut config = test_config();
        config.live_photo_mode = LivePhotoMode::Skip;
        let paths = expected_paths_for(&asset, &config);
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].version_size, VersionSizeKey::Original);
    }

    #[test]
    fn expected_paths_force_resolution_missing_returns_empty() {
        let asset = TestPhotoAsset::new("TEST_1")
            .filename("IMG_0001.JPG")
            .build();
        let mut config = test_config();
        config.resolution = crate::types::PhotoResolution::Medium;
        config.force_resolution = true;
        let paths = expected_paths_for(&asset, &config);
        assert!(
            paths.is_empty(),
            "force_resolution + missing size should yield no paths, got {paths:?}"
        );
    }

    #[test]
    fn path_derivation_config_matches_download_config_expected_paths() {
        let asset = TestPhotoAsset::new("PARITY_1")
            .filename("über.heic")
            .live_photo("https://p01.icloud-content.com/live", "live123", 700)
            .adjusted_version(
                "https://p01.icloud-content.com/adjusted",
                "adjusted123",
                900,
                "public.heic",
            )
            .live_adjusted(
                "https://p01.icloud-content.com/live-adjusted",
                "live-adjusted123",
                650,
            )
            .build();
        let mut sync_config = test_config();
        sync_config.folder_structure = "{album}/%Y/%m".to_string();
        sync_config.album_name = Some(Arc::from("Trips"));
        sync_config.edited = true;
        sync_config.file_match_policy = FileMatchPolicy::NameId7;
        sync_config.live_photo_mov_filename_policy = LivePhotoMovFilenamePolicy::Original;
        sync_config.keep_unicode_in_filenames = true;

        let fields = crate::config::PathDerivationFields {
            folder_structure: sync_config.folder_structure.clone(),
            folder_structure_albums: sync_config.folder_structure_albums.to_string(),
            folder_structure_smart_folders: sync_config.folder_structure_smart_folders.to_string(),
            resolution: sync_config.resolution,
            live_photo_mode: sync_config.live_photo_mode,
            live_resolution: crate::types::LivePhotoResolution::Original,
            live_photo_mov_filename_policy: sync_config.live_photo_mov_filename_policy,
            edited: sync_config.edited,
            alternative: sync_config.alternative,
            raw_policy: sync_config.raw_policy,
            file_match_policy: sync_config.file_match_policy,
            force_resolution: sync_config.force_resolution,
            keep_unicode_in_filenames: sync_config.keep_unicode_in_filenames,
        };
        let mut path_config = PathDerivationConfig::from_path_fields(
            Arc::clone(&sync_config.directory),
            fields,
            sync_config.media,
        );
        path_config.album_name = sync_config.album_name.clone();

        let to_tuples = |paths: Vec<ExpectedAssetPath>| {
            paths
                .into_iter()
                .map(|p| (p.path, p.size, p.checksum, p.url, p.version_size))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            to_tuples(expected_paths_for(&asset, &path_config)),
            to_tuples(expected_paths_for(&asset, &sync_config)),
            "import path config must derive the same expected paths as sync config"
        );
    }

    #[test]
    fn expected_paths_size_fallback_to_original_when_force_resolution_off() {
        let asset = TestPhotoAsset::new("TEST_1")
            .filename("IMG_0001.JPG")
            .build();
        let mut config = test_config();
        config.resolution = crate::types::PhotoResolution::Medium;
        config.force_resolution = false;
        let paths = expected_paths_for(&asset, &config);
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].version_size, VersionSizeKey::Original);
        let name = paths[0].path.file_name().unwrap().to_string_lossy();
        assert!(
            !name.contains("-medium"),
            "fallback to Original should not carry medium suffix, got {name}"
        );
    }

    #[test]
    fn expected_paths_live_photo_with_name_id7_applies_suffix_to_both() {
        let asset = TestPhotoAsset::new("LIVE_5")
            .filename("IMG_3000.HEIC")
            .item_type("public.heic")
            .orig_file_type("public.heic")
            .live_photo("https://p01.icloud-content.com/mov", "mov_ck", 3000)
            .build();
        let mut config = test_config();
        config.file_match_policy = FileMatchPolicy::NameId7;
        let paths = expected_paths_for(&asset, &config);
        assert_eq!(paths.len(), 2);
        let primary = paths[0].path.file_name().unwrap().to_string_lossy();
        let mov = paths[1].path.file_name().unwrap().to_string_lossy();
        assert!(
            primary.starts_with("IMG_3000_") && primary.ends_with(".HEIC"),
            "primary missing id7 suffix: {primary}"
        );
        assert!(
            mov.starts_with("IMG_3000_") && mov.ends_with("_HEVC.MOV"),
            "MOV companion missing id7 suffix: {mov}"
        );
    }

    #[test]
    fn expected_paths_no_versions_returns_empty() {
        // Build a minimal asset with no resOriginalRes — all version lookups
        // fail, expected_paths_for returns empty (caller skips).
        let master = json!({
            "recordName": "EMPTY_1",
            "fields": {
                "filenameEnc": {"value": "x.jpg", "type": "STRING"},
                "itemType": {"value": "public.jpeg"},
            },
        });
        let asset_record = json!({
            "fields": {"assetDate": {"value": 1736899200000.0_f64}},
        });
        let asset = PhotoAsset::new(master, asset_record);
        let config = test_config();
        let paths = expected_paths_for(&asset, &config);
        assert!(paths.is_empty());
    }

    // ── expected_paths_for / filter_asset_to_tasks parity ────────────────
    //
    // expected_paths_for is import-existing's view of where sync would have
    // written each asset. filter_asset_to_tasks is sync's source of truth
    // for the same derivation. They must agree on the bare path (before
    // collision-suffix resolution) so import-existing scans the file sync
    // actually produces. These tests pin parity across the configurations
    // most likely to drift apart (file_match_policy, resolution variants, live
    // photo modes, raw alignment).

    fn assert_path_parity(
        asset: &PhotoAsset,
        config: &DownloadConfig,
        which: VersionSizeKey,
        label: &str,
    ) {
        let want_live = matches!(which, VersionSizeKey::LiveOriginal);
        let expected = expected_paths_for(asset, config);
        let tasks = filter_asset_fresh(asset, config);
        let exp = expected
            .iter()
            .find(|p| matches!(p.version_size, VersionSizeKey::LiveOriginal) == want_live)
            .map(|p| p.path.clone())
            .unwrap_or_default();
        let got = tasks
            .iter()
            .find(|t| matches!(t.version_size, VersionSizeKey::LiveOriginal) == want_live)
            .map(|t| t.download_path.to_path_buf())
            .unwrap_or_default();
        assert_eq!(
            exp, got,
            "{label}: expected_paths_for path drifted from filter_asset_to_tasks"
        );
    }

    #[test]
    fn expected_paths_parity_default_config() {
        let asset = TestPhotoAsset::new("PAR_1")
            .filename("IMG_5001.JPG")
            .build();
        let config = test_config();
        assert_path_parity(&asset, &config, VersionSizeKey::Original, "default");
    }

    #[test]
    fn expected_paths_parity_unicode_stripped_fingerprint_fallback() {
        let asset = TestPhotoAsset::new("PAR_UNI")
            .filename("日本語.jpg")
            .build();
        let config = test_config();
        assert_path_parity(
            &asset,
            &config,
            VersionSizeKey::Original,
            "unicode stripped fingerprint fallback",
        );
    }

    #[test]
    fn expected_paths_parity_name_id7() {
        let asset = TestPhotoAsset::new("PAR_2")
            .filename("IMG_5002.JPG")
            .build();
        let mut config = test_config();
        config.file_match_policy = FileMatchPolicy::NameId7;
        assert_path_parity(&asset, &config, VersionSizeKey::Original, "NameId7");
    }

    #[test]
    fn expected_paths_parity_size_medium_with_fallback() {
        // size=Medium but no medium version available; both call sites
        // must fall back to Original consistently (force_resolution=false).
        let asset = TestPhotoAsset::new("PAR_3")
            .filename("IMG_5003.JPG")
            .build();
        let mut config = test_config();
        config.resolution = crate::types::PhotoResolution::Medium;
        config.force_resolution = false;
        assert_path_parity(&asset, &config, VersionSizeKey::Original, "Medium fallback");
    }

    #[test]
    fn expected_paths_parity_live_photo_both() {
        let asset = TestPhotoAsset::new("PAR_4")
            .filename("IMG_5004.HEIC")
            .item_type("public.heic")
            .orig_file_type("public.heic")
            .live_photo("https://p01.icloud-content.com/mov", "mov_ck", 3000)
            .build();
        let config = test_config();
        assert_path_parity(
            &asset,
            &config,
            VersionSizeKey::Original,
            "live both primary",
        );
        assert_path_parity(
            &asset,
            &config,
            VersionSizeKey::LiveOriginal,
            "live both mov",
        );
    }

    #[test]
    fn expected_paths_parity_live_photo_name_id7() {
        let asset = TestPhotoAsset::new("PAR_5")
            .filename("IMG_5005.HEIC")
            .item_type("public.heic")
            .orig_file_type("public.heic")
            .live_photo("https://p01.icloud-content.com/mov", "mov_ck", 3000)
            .build();
        let mut config = test_config();
        config.file_match_policy = FileMatchPolicy::NameId7;
        assert_path_parity(
            &asset,
            &config,
            VersionSizeKey::Original,
            "live id7 primary",
        );
        assert_path_parity(
            &asset,
            &config,
            VersionSizeKey::LiveOriginal,
            "live id7 mov",
        );
    }

    #[test]
    fn expected_paths_parity_live_photo_video_only() {
        // VideoOnly: primary path absent in both, MOV present in both.
        let asset = TestPhotoAsset::new("PAR_6")
            .filename("IMG_5006.HEIC")
            .item_type("public.heic")
            .orig_file_type("public.heic")
            .live_photo("https://p01.icloud-content.com/mov", "mov_ck", 3000)
            .build();
        let mut config = test_config();
        config.live_photo_mode = LivePhotoMode::VideoOnly;
        assert_path_parity(
            &asset,
            &config,
            VersionSizeKey::Original,
            "video-only primary (absent)",
        );
        assert_path_parity(
            &asset,
            &config,
            VersionSizeKey::LiveOriginal,
            "video-only mov",
        );
    }

    #[test]
    fn expected_paths_parity_mov_filename_policy_original() {
        // The non-default MOV filename policy is a known drift suspect:
        // the live_photo_mov_path_original branch in expected_paths_for
        // reuses a helper from paths.rs that filter_asset_to_tasks also
        // calls; this pins them.
        let asset = TestPhotoAsset::new("PAR_7")
            .filename("IMG_5007.HEIC")
            .item_type("public.heic")
            .orig_file_type("public.heic")
            .live_photo("https://p01.icloud-content.com/mov", "mov_ck", 3000)
            .build();
        let mut config = test_config();
        config.live_photo_mov_filename_policy = LivePhotoMovFilenamePolicy::Original;
        assert_path_parity(
            &asset,
            &config,
            VersionSizeKey::Original,
            "mov policy=Original primary",
        );
        assert_path_parity(
            &asset,
            &config,
            VersionSizeKey::LiveOriginal,
            "mov policy=Original mov",
        );
    }

    #[test]
    fn expected_paths_parity_custom_album_in_folder_template() {
        let asset = TestPhotoAsset::new("PAR_8")
            .filename("IMG_5008.JPG")
            .build();
        let mut config = test_config();
        config.folder_structure = "{album}/%Y".to_string();
        config.album_name = Some(Arc::from("Vacation 2025"));
        assert_path_parity(
            &asset,
            &config,
            VersionSizeKey::Original,
            "album in template",
        );
    }

    // ── size / live_resolution matrix on present versions ───────────────
    //
    // The matrix expansion below pins behaviour for [photos].resolution and
    // [photos].live_resolution when the requested version is published. The
    // pre-existing `expected_paths_size_*` tests cover the fallback (resolution
    // missing) and force_resolution branches; these cover the "actually use the
    // requested resolution" branch and the independence between primary and
    // live-photo resolution.

    /// Builds a primary photo with original + medium + thumb JPEG
    /// resolutions. Mirrors `multi_size_photo_asset` (defined later in
    /// this mod) but is independent so test reordering can't break it.
    fn primary_multi_size_asset(record: &str, filename: &str) -> PhotoAsset {
        PhotoAsset::new(
            json!({"recordName": record, "fields": {
                "filenameEnc": {"value": filename, "type": "STRING"},
                "itemType": {"value": "public.jpeg"},
                "resOriginalRes": {"value": {
                    "size": 5000_u64,
                    "downloadURL": "https://p01.icloud-content.com/orig",
                    "fileChecksum": "orig_ck"
                }},
                "resOriginalFileType": {"value": "public.jpeg"},
                "resJPEGMedRes": {"value": {
                    "size": 2000_u64,
                    "downloadURL": "https://p01.icloud-content.com/med",
                    "fileChecksum": "med_ck"
                }},
                "resJPEGMedFileType": {"value": "public.jpeg"},
                "resJPEGThumbRes": {"value": {
                    "size": 500_u64,
                    "downloadURL": "https://p01.icloud-content.com/thumb",
                    "fileChecksum": "thumb_ck"
                }},
                "resJPEGThumbFileType": {"value": "public.jpeg"}
            }}),
            json!({"fields": {"assetDate": {"value": 1_736_899_200_000.0_f64}}}),
        )
    }

    /// Live-photo HEIC primary with both LiveOriginal and LiveMedium MOV
    /// companions. Covers the live_resolution=Medium path.
    fn live_photo_multi_size_asset(record: &str) -> PhotoAsset {
        PhotoAsset::new(
            json!({"recordName": record, "fields": {
                "filenameEnc": {"value": "IMG_LIVE.HEIC", "type": "STRING"},
                "itemType": {"value": "public.heic"},
                "resOriginalRes": {"value": {
                    "size": 4000_u64,
                    "downloadURL": "https://p01.icloud-content.com/heic_orig",
                    "fileChecksum": "heic_ck"
                }},
                "resOriginalFileType": {"value": "public.heic"},
                "resOriginalVidComplRes": {"value": {
                    "size": 3000_u64,
                    "downloadURL": "https://p01.icloud-content.com/live_orig",
                    "fileChecksum": "live_orig_ck"
                }},
                "resOriginalVidComplFileType": {"value": "com.apple.quicktime-movie"},
                "resVidMedRes": {"value": {
                    "size": 1500_u64,
                    "downloadURL": "https://p01.icloud-content.com/live_med",
                    "fileChecksum": "live_med_ck"
                }},
                "resVidMedFileType": {"value": "com.apple.quicktime-movie"},
            }}),
            json!({"fields": {"assetDate": {"value": 1_736_899_200_000.0_f64}}}),
        )
    }

    /// CG-2: regression-guards `resolution = "medium"` actually-published path.
    /// A bug in the size-suffix branch of `expected_paths_for` would emit
    /// an unsuffixed path; sync would write `IMG-medium.JPG` while
    /// import-existing scans for `IMG.JPG` (silent miss).
    #[test]
    fn expected_paths_size_medium_present_emits_medium_suffix() {
        let asset = primary_multi_size_asset("MED_PRESENT", "IMG_6001.JPG");
        let mut config = test_config();
        config.resolution = crate::types::PhotoResolution::Medium;
        let paths = expected_paths_for(&asset, &config);
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].version_size, VersionSizeKey::Medium);
        assert_eq!(paths[0].size, 2000);
        let name = paths[0].path.file_name().unwrap().to_string_lossy();
        assert!(
            name.contains("-medium"),
            "size=Medium present should carry '-medium' suffix, got {name}"
        );
    }

    /// CG-2 parity: when Medium is published and `resolution = "medium"` is set,
    /// sync's path and import's path agree.
    #[test]
    fn expected_paths_parity_size_medium_present() {
        let asset = primary_multi_size_asset("PAR_MED", "IMG_6002.JPG");
        let mut config = test_config();
        config.resolution = crate::types::PhotoResolution::Medium;
        assert_path_parity(&asset, &config, VersionSizeKey::Medium, "Medium present");
    }

    /// CG-3: regression-guards `resolution = "thumb"` actually-published path.
    #[test]
    fn expected_paths_size_thumb_present_emits_thumb_suffix() {
        let asset = primary_multi_size_asset("THUMB_PRESENT", "IMG_6003.JPG");
        let mut config = test_config();
        config.resolution = crate::types::PhotoResolution::Thumb;
        let paths = expected_paths_for(&asset, &config);
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].version_size, VersionSizeKey::Thumb);
        assert_eq!(paths[0].size, 500);
        let name = paths[0].path.file_name().unwrap().to_string_lossy();
        assert!(
            name.contains("-thumb"),
            "size=Thumb present should carry '-thumb' suffix, got {name}"
        );
    }

    /// CG-3 parity.
    #[test]
    fn expected_paths_parity_size_thumb_present() {
        let asset = primary_multi_size_asset("PAR_THUMB", "IMG_6004.JPG");
        let mut config = test_config();
        config.resolution = crate::types::PhotoResolution::Thumb;
        assert_path_parity(&asset, &config, VersionSizeKey::Thumb, "Thumb present");
    }

    /// CG-4: regression-guards `live_resolution = "medium"`. A bug in the
    /// `version_with_fallback` call inside the live branch would silently
    /// land the LiveOriginal MOV at the LiveMedium config, producing the
    /// wrong path.
    #[test]
    fn expected_paths_live_resolution_medium_emits_live_medium_path() {
        let asset = live_photo_multi_size_asset("LIVE_MED_1");
        let mut config = test_config();
        config.live_resolution = AssetVersionSize::LiveMedium;
        let paths = expected_paths_for(&asset, &config);
        assert_eq!(paths.len(), 2, "expected primary + MOV companion");
        let mov = paths
            .iter()
            .find(|p| matches!(p.version_size, VersionSizeKey::LiveMedium))
            .expect("LiveMedium MOV path missing");
        assert_eq!(mov.size, 1500);
        assert_eq!(&*mov.checksum, "live_med_ck");
        // The primary stays at Original and unaffected by live_resolution.
        let primary = paths
            .iter()
            .find(|p| matches!(p.version_size, VersionSizeKey::Original))
            .expect("primary path missing");
        assert_eq!(primary.version_size, VersionSizeKey::Original);
        assert_eq!(primary.size, 4000);
    }

    /// CG-5: `resolution` and `live_resolution` are independent. A
    /// regression that couples them (e.g. live branch reading
    /// `config.resolution` instead of `config.live_resolution`) lands silently
    /// without this assertion.
    #[test]
    fn expected_paths_size_medium_with_live_resolution_thumb_independent() {
        // Build a HEIC primary with original + medium res, and a live MOV
        // companion at LiveOriginal + LiveMedium. We don't have a
        // LiveThumb resolution to point to, so we use LiveMedium for the
        // live resolution and Medium for the primary -- different
        // non-default values across the two settings.
        let asset = PhotoAsset::new(
            json!({"recordName": "INDEP_1", "fields": {
                "filenameEnc": {"value": "IMG_INDEP.HEIC", "type": "STRING"},
                "itemType": {"value": "public.heic"},
                "resOriginalRes": {"value": {
                    "size": 4000_u64,
                    "downloadURL": "https://p01.icloud-content.com/heic_orig",
                    "fileChecksum": "heic_ck"
                }},
                "resOriginalFileType": {"value": "public.heic"},
                "resJPEGMedRes": {"value": {
                    "size": 1800_u64,
                    "downloadURL": "https://p01.icloud-content.com/heic_med",
                    "fileChecksum": "heic_med_ck"
                }},
                "resJPEGMedFileType": {"value": "public.jpeg"},
                "resOriginalVidComplRes": {"value": {
                    "size": 3000_u64,
                    "downloadURL": "https://p01.icloud-content.com/live_orig",
                    "fileChecksum": "live_orig_ck"
                }},
                "resOriginalVidComplFileType": {"value": "com.apple.quicktime-movie"},
                "resVidMedRes": {"value": {
                    "size": 1500_u64,
                    "downloadURL": "https://p01.icloud-content.com/live_med",
                    "fileChecksum": "live_med_ck"
                }},
                "resVidMedFileType": {"value": "com.apple.quicktime-movie"},
            }}),
            json!({"fields": {"assetDate": {"value": 1_736_899_200_000.0_f64}}}),
        );
        let mut config = test_config();
        config.resolution = crate::types::PhotoResolution::Medium;
        config.live_resolution = AssetVersionSize::LiveMedium;

        let paths = expected_paths_for(&asset, &config);
        assert_eq!(paths.len(), 2);
        let primary = paths
            .iter()
            .find(|p| matches!(p.version_size, VersionSizeKey::Medium))
            .expect("primary at Medium missing");
        assert_eq!(primary.size, 1800);
        let mov = paths
            .iter()
            .find(|p| matches!(p.version_size, VersionSizeKey::LiveMedium))
            .expect("MOV at LiveMedium missing");
        assert_eq!(mov.size, 1500);
        // Crucially: primary did not key off live_resolution, MOV did
        // not key off primary resolution. (If the two settings were coupled, both would
        // share one variant.)
        assert_ne!(primary.version_size, VersionSizeKey::LiveMedium);
        assert_ne!(mov.version_size, VersionSizeKey::Medium);
    }

    /// CG-6: `raw_policy = "prefer-raw"` + `resolution = "medium"`. apply_raw_policy
    /// runs before size selection; this pins that the swap doesn't
    /// silently re-key the size lookup off the wrong version.
    #[test]
    fn expected_paths_raw_policy_prefer_raw_with_resolution_medium_keys_correctly() {
        // RAW + JPEG pair where the alt is the JPEG. With
        // raw_policy=PreferRaw promotes the RAW side into the Original slot
        // (the "user-visible" original) per the existing `raw_policy_*`
        // tests; the question here is whether `resolution = "medium"` then keys
        // off the Medium version of that promoted RAW side (the test's primary
        // has no medium published, so we expect fallback to Original size with
        // force_resolution=false).
        let asset = TestPhotoAsset::new("ALIGN_MED")
            .filename("IMG_RAW_MED.DNG")
            .item_type("public.camera-raw-image")
            .orig_file_type("public.camera-raw-image")
            .alt_version(
                "https://p01.icloud-content.com/jpeg",
                "jpeg_ck",
                2500,
                "public.jpeg",
            )
            .build();
        let mut config = test_config();
        config.raw_policy = RawPolicy::PreferRaw;
        config.resolution = crate::types::PhotoResolution::Medium;
        config.force_resolution = false;
        let paths = expected_paths_for(&asset, &config);
        assert_eq!(paths.len(), 1);
        // No medium published in the swapped Original (which is the
        // RAW alt promoted to Original under PreferRaw); the
        // fallback should land on Original-without-suffix.
        assert_eq!(paths[0].version_size, VersionSizeKey::Original);
        let name = paths[0].path.file_name().unwrap().to_string_lossy();
        assert!(
            !name.contains("-medium"),
            "fallback to Original under raw_policy must drop the medium suffix, got {name}"
        );
    }

    /// CG-7: `force_resolution = true` applied to the live-photo companion. With
    /// force_resolution=true and live_resolution=LiveMedium but only LiveOriginal
    /// published, the MOV companion should drop entirely (not silently
    /// land at LiveOriginal).
    #[test]
    fn expected_paths_force_resolution_drops_live_companion_when_live_size_missing() {
        let asset = TestPhotoAsset::new("FORCE_LIVE")
            .filename("IMG_FL.HEIC")
            .item_type("public.heic")
            .orig_file_type("public.heic")
            .live_photo("https://p01.icloud-content.com/live_orig", "live_ck", 3000)
            .build();
        let mut config = test_config();
        config.live_resolution = AssetVersionSize::LiveMedium;
        config.force_resolution = true;
        let paths = expected_paths_for(&asset, &config);
        // Primary HEIC is still Original and kept (force_resolution applies to
        // the requested primary resolution and to the requested
        // `live_resolution`; primary resolution is Original which is
        // present).
        assert!(
            paths
                .iter()
                .any(|p| matches!(p.version_size, VersionSizeKey::Original)),
            "primary should remain present when its requested size is published"
        );
        // The MOV companion should drop because LiveMedium is missing
        // and force_resolution=true forbids fallback.
        assert!(
            !paths.iter().any(|p| matches!(
                p.version_size,
                VersionSizeKey::LiveOriginal | VersionSizeKey::LiveMedium
            )),
            "force_resolution + missing LiveMedium should drop the MOV companion entirely, got {paths:?}"
        );
    }

    /// CG-8: `--live-photo-mov-filename-policy original` + `name-id7`.
    /// The Original-policy branch must still apply the name-id7 suffix
    /// to the MOV (otherwise id7 users on the non-default MOV policy
    /// silently break).
    #[test]
    fn expected_paths_mov_policy_original_with_name_id7_carries_suffix() {
        let asset = TestPhotoAsset::new("MOV_ID7_ORIG")
            .filename("IMG_8001.HEIC")
            .item_type("public.heic")
            .orig_file_type("public.heic")
            .live_photo("https://p01.icloud-content.com/mov", "mov_ck", 3000)
            .build();
        let mut config = test_config();
        config.file_match_policy = FileMatchPolicy::NameId7;
        config.live_photo_mov_filename_policy = LivePhotoMovFilenamePolicy::Original;
        let paths = expected_paths_for(&asset, &config);
        assert_eq!(paths.len(), 2);
        let mov = paths
            .iter()
            .find(|p| matches!(p.version_size, VersionSizeKey::LiveOriginal))
            .expect("MOV companion missing");
        let mov_name = mov.path.file_name().unwrap().to_string_lossy();
        // Under Original policy the MOV reuses the primary's stem (no
        // _HEVC suffix); under NameId7 the stem itself carries the id7
        // marker, so the MOV path must carry it too. The HEIC->MOV
        // extension map happens regardless of policy.
        assert!(
            mov_name.starts_with("IMG_8001_") && mov_name.ends_with(".MOV"),
            "MOV under Original policy + id7 should be IMG_8001_<id7>.MOV, got {mov_name}"
        );
        assert!(
            !mov_name.contains("_HEVC"),
            "Original MOV policy should NOT add _HEVC suffix, got {mov_name}"
        );
    }

    // ── expected_paths_for negative-space coverage ───────────────────────
    //
    // The 11 happy-path expected_paths_* tests above leave a lot of input
    // surface untested. These pin behavior on the filename / album-name
    // edges most likely to surprise: non-ASCII when keep_unicode is on vs
    // off, traversal-style names, names that vanish after sanitization,
    // separators inside filenames, and weird album names.

    #[test]
    fn expected_paths_keeps_unicode_when_flag_set() {
        let asset = TestPhotoAsset::new("UNI_1")
            .filename("héllo_wörld.JPG")
            .build();
        let mut config = test_config();
        config.keep_unicode_in_filenames = true;
        let paths = expected_paths_for(&asset, &config);
        assert_eq!(paths.len(), 1);
        let name = paths[0].path.file_name().unwrap().to_string_lossy();
        assert!(
            name.contains('é') && name.contains('ö'),
            "keep_unicode=true should preserve non-ASCII, got {name}"
        );
    }

    #[test]
    fn expected_paths_strips_unicode_when_flag_off() {
        let asset = TestPhotoAsset::new("UNI_2")
            .filename("héllo_wörld.JPG")
            .build();
        let config = test_config();
        let paths = expected_paths_for(&asset, &config);
        assert_eq!(paths.len(), 1);
        let name = paths[0].path.file_name().unwrap().to_string_lossy();
        assert!(
            !name.contains('é') && !name.contains('ö') && name.contains("hllo_wrld"),
            "keep_unicode=false should strip non-ASCII, got {name}"
        );
    }

    #[test]
    fn expected_paths_filename_emptied_by_unicode_strip_uses_fingerprint() {
        let asset = TestPhotoAsset::new("UNI_3").filename("日本語.jpg").build();
        let config = test_config();
        let paths = expected_paths_for(&asset, &config);
        assert_eq!(paths.len(), 1);
        let name = paths[0].path.file_name().unwrap().to_string_lossy();
        assert_eq!(
            name,
            paths::generate_fingerprint_filename("UNI_3", "public.jpeg"),
            "all-non-ASCII stem must fall back to a visible fingerprint name"
        );
    }

    #[test]
    fn expected_paths_filename_without_extension_emptied_by_unicode_strip_uses_fingerprint() {
        let asset = TestPhotoAsset::new("UNI_NOEXT").filename("日本語").build();
        let config = test_config();
        let paths = expected_paths_for(&asset, &config);
        assert_eq!(paths.len(), 1);
        let name = paths[0].path.file_name().unwrap().to_string_lossy();
        assert_eq!(
            name,
            paths::generate_fingerprint_filename("UNI_NOEXT", "public.jpeg"),
            "all-non-ASCII filename without an extension must use UTI-derived fingerprint name"
        );
    }

    #[test]
    fn expected_paths_keep_unicode_with_decomposed_form() {
        // NFC "é" (U+00E9) vs NFD "e\u{0301}" — kei does no normalization,
        // so both round-trip when keep_unicode=true. Pin that so a future
        // unicode-normalization pass doesn't silently change matches.
        let nfc = "ca\u{00e9}.JPG";
        let nfd = "cae\u{0301}.JPG";
        let mut config = test_config();
        config.keep_unicode_in_filenames = true;
        for (label, fname) in [("NFC", nfc), ("NFD", nfd)] {
            let asset = TestPhotoAsset::new("UNI_4").filename(fname).build();
            let paths = expected_paths_for(&asset, &config);
            assert_eq!(paths.len(), 1, "{label}: expected one path");
            let name = paths[0].path.file_name().unwrap().to_string_lossy();
            assert_eq!(
                name, fname,
                "{label}: filename round-trip should be byte-identical"
            );
        }
    }

    #[test]
    fn expected_paths_filename_with_path_separators_is_safe() {
        // iCloud filenames shouldn't contain `/` but the wire format is
        // a string, so a malformed asset could carry one. The path must
        // still be confined to `directory` (no traversal out).
        let asset = TestPhotoAsset::new("SEP_1")
            .filename("evil/IMG.JPG")
            .build();
        let config = test_config();
        let paths = expected_paths_for(&asset, &config);
        assert_eq!(paths.len(), 1);
        let path_str = paths[0].path.to_string_lossy().into_owned();
        // The directory prefix is stable; everything after must not
        // re-introduce a `/IMG` segment that could escape into a sibling
        // directory.
        let dir_str = config.directory.to_string_lossy().into_owned();
        assert!(
            path_str.starts_with(&dir_str),
            "path escaped directory: {path_str}"
        );
        let suffix = path_str.trim_start_matches(&*dir_str);
        assert!(
            !suffix.contains("/evil/") && !suffix.contains("evil/IMG.JPG"),
            "raw `evil/IMG.JPG` survived sanitization: {suffix}"
        );
    }

    #[test]
    fn expected_paths_filename_with_traversal_is_safe() {
        // `../../etc/passwd.JPG` — the path-separator + traversal
        // sequence has to land inside `directory`, not at /etc/passwd.
        // Sanitization replaces `/` with `_`, so `..` substrings can
        // survive *as part of one filename*, which is harmless. What
        // must NOT happen: the path having extra segments that walk
        // out of `directory`.
        let asset = TestPhotoAsset::new("TRAV_1")
            .filename("../../etc/passwd.JPG")
            .build();
        let config = test_config();
        let paths = expected_paths_for(&asset, &config);
        assert_eq!(paths.len(), 1);
        let path = &paths[0].path;
        assert!(
            path.starts_with(&*config.directory),
            "path escaped configured directory: {}",
            path.display()
        );
        // Folder template is `%Y/%m/%d` (3 dated dirs) + 1 filename
        // = 4 components past `directory`. Anything more means a
        // traversal segment leaked into the path tree.
        let suffix = path.strip_prefix(&*config.directory).unwrap();
        assert_eq!(
            suffix.components().count(),
            4,
            "extra path segments (traversal leak): {}",
            suffix.display()
        );
        // And the literal `/etc/passwd` must not appear as part of a
        // path component sequence.
        let path_str = path.to_string_lossy();
        assert!(
            !path_str.contains("/etc/") && !path_str.contains("/passwd."),
            "raw traversal segments survived in the path: {path_str}"
        );
    }

    #[test]
    fn expected_paths_album_name_with_separators_sanitized() {
        let asset = TestPhotoAsset::new("ALB_1")
            .filename("IMG_0001.JPG")
            .build();
        let mut config = test_config();
        config.folder_structure = "{album}".to_string();
        config.album_name = Some(Arc::from("evil/../escape"));
        let paths = expected_paths_for(&asset, &config);
        assert_eq!(paths.len(), 1);
        let path_str = paths[0].path.to_string_lossy().into_owned();
        assert!(
            !path_str.contains("..") && !path_str.contains("/escape/"),
            "album traversal survived: {path_str}"
        );
    }

    #[test]
    fn expected_paths_filename_only_dots_and_spaces() {
        // "  ...  " trims to empty — filename derivation has to produce
        // *some* name, not a literal "" segment.
        let asset = TestPhotoAsset::new("DOTS_1").filename("  ...  ").build();
        let config = test_config();
        let paths = expected_paths_for(&asset, &config);
        assert_eq!(paths.len(), 1);
        let name = paths[0].path.file_name().unwrap().to_string_lossy();
        assert!(
            !name.is_empty() && !name.trim_matches(|c: char| c == '.' || c == ' ').is_empty(),
            "filename must not collapse to a dots/spaces-only name, got {name:?}"
        );
    }

    #[test]
    fn test_filter_skips_videos_when_configured() {
        let asset = TestPhotoAsset::new("VID_1")
            .filename("movie.mov")
            .item_type("com.apple.quicktime-movie")
            .orig_file_type("com.apple.quicktime-movie")
            .orig_size(50000)
            .orig_url("https://p01.icloud-content.com/vid")
            .orig_checksum("vid_ck")
            .build();
        let mut config = test_config();
        config.media.videos = false;
        assert_eq!(
            is_asset_filtered(&asset, &config),
            Some(FilterReason::MediaType)
        );
    }

    #[test]
    fn test_filter_video_task_carries_size() {
        let asset = TestPhotoAsset::new("VID_2")
            .filename("movie.mov")
            .item_type("com.apple.quicktime-movie")
            .orig_file_type("com.apple.quicktime-movie")
            .orig_size(500_000_000)
            .orig_url("https://p01.icloud-content.com/big_vid")
            .orig_checksum("big_ck")
            .build();
        let config = test_config();
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].size, 500_000_000);
    }

    #[test]
    fn test_filter_skips_photos_when_configured() {
        let asset = TestPhotoAsset::new("TEST_1").build();
        let mut config = test_config();
        config.media.photos = false;
        assert_eq!(
            is_asset_filtered(&asset, &config),
            Some(FilterReason::MediaType)
        );
    }

    #[test]
    fn test_filter_live_photos_only_skips_normal_photos_and_videos() {
        let photo = TestPhotoAsset::new("PHOTO_1").build();
        let video = TestPhotoAsset::new("VID_1")
            .filename("movie.mov")
            .item_type("com.apple.quicktime-movie")
            .orig_file_type("com.apple.quicktime-movie")
            .orig_size(50000)
            .orig_url("https://p01.icloud-content.com/vid")
            .orig_checksum("vid_ck")
            .build();
        let live = test_live_photo_asset();
        let mut config = test_config();
        config.media.photos = false;
        config.media.videos = false;
        config.media.live_photos = true;

        assert_eq!(
            is_asset_filtered(&photo, &config),
            Some(FilterReason::MediaType)
        );
        assert_eq!(
            is_asset_filtered(&video, &config),
            Some(FilterReason::MediaType)
        );
        assert_eq!(is_asset_filtered(&live, &config), None);
    }

    #[test]
    fn test_filter_uses_fingerprint_fallback_without_filename() {
        // Asset ID with special chars uses SHA-256 hash for collision resistance:
        // SHA-256("AB/CD+EF==GH") → "c492ec6c51ec..."
        let asset = PhotoAsset::new(
            json!({"recordName": "AB/CD+EF==GH", "fields": {
                "itemType": {"value": "public.jpeg"},
                "resOriginalRes": {"value": {
                    "size": 1000,
                    "downloadURL": "https://p01.icloud-content.com/orig",
                    "fileChecksum": "abc123"
                }},
                "resOriginalFileType": {"value": "public.jpeg"}
            }}),
            json!({"fields": {"assetDate": {"value": 1736899200000.0}}}),
        );
        let config = test_config();
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 1);
        assert!(
            tasks[0]
                .download_path
                .to_string_lossy()
                .contains("c492ec6c51ec.JPG"),
            "Expected fingerprint hash fallback filename, got: {:?}",
            tasks[0].download_path
        );
    }

    #[test]
    fn test_filter_skips_asset_without_requested_version() {
        let asset = PhotoAsset::new(
            json!({"recordName": "SMALL_ONLY", "fields": {
                "filenameEnc": {"value": "photo.jpg", "type": "STRING"},
                "itemType": {"value": "public.jpeg"},
                "resJPEGThumbRes": {"value": {
                    "size": 100,
                    "downloadURL": "https://p01.icloud-content.com/thumb",
                    "fileChecksum": "th_ck"
                }},
                "resJPEGThumbFileType": {"value": "public.jpeg"}
            }}),
            json!({"fields": {"assetDate": {"value": 1736899200000.0}}}),
        );
        let config = test_config(); // requests Original, but only Thumb available
        assert!(filter_asset_fresh(&asset, &config).is_empty());
    }

    #[test]
    fn test_filter_routes_existing_same_size_file_to_identity_path_without_state() {
        let dir = TempDir::new().unwrap();
        let asset = TestPhotoAsset::new("TEST_1").build();
        let mut config = test_config();
        config.directory = std::sync::Arc::from(dir.path());

        // First call should produce a task (file doesn't exist yet)
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 1);

        // Create the file with matching size (1000 bytes). The path layer no
        // longer treats same name + same size as proof of identity; production
        // state checks own safe already-downloaded skips.
        fs::create_dir_all(tasks[0].download_path.parent().unwrap()).unwrap();
        fs::write(&tasks[0].download_path, vec![0u8; 1000]).unwrap();
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 1);
        assert!(
            tasks[0]
                .download_path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.contains("TEST_1")),
            "same-size collision must use an identity path, got {:?}",
            tasks[0].download_path
        );
    }

    #[test]
    fn test_filter_deduplicates_file_with_different_size() {
        let dir = TempDir::new().unwrap();

        let asset = TestPhotoAsset::new("TEST_1").build(); // version.size = 1000
        let mut config = test_config();
        config.directory = std::sync::Arc::from(dir.path());

        // First call: file doesn't exist yet
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 1);
        let original_path = tasks[0].download_path.clone();

        // Create a file with DIFFERENT size (simulating a collision with different content)
        fs::create_dir_all(original_path.parent().unwrap()).unwrap();
        fs::write(&original_path, vec![0u8; 500]).unwrap(); // 500 bytes, not 1000

        // Second call: should produce a task with deduped path (size suffix)
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 1);
        let dedup_path = tasks[0].download_path.to_str().unwrap();
        assert!(
            dedup_path.contains("-1000."),
            "Expected size suffix '-1000.' in deduped path, got: {}",
            dedup_path,
        );
    }

    fn test_live_photo_asset() -> PhotoAsset {
        TestPhotoAsset::new("LIVE_1")
            .filename("IMG_0001.HEIC")
            .item_type("public.heic")
            .orig_file_type("public.heic")
            .orig_size(2000)
            .orig_url("https://p01.icloud-content.com/heic_orig")
            .orig_checksum("heic_ck")
            .live_photo("https://p01.icloud-content.com/live_mov", "mov_ck", 3000)
            .build()
    }

    #[test]
    fn test_filter_produces_live_photo_mov_task() {
        let asset = test_live_photo_asset();
        let config = test_config();
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 2);
        assert_eq!(&*tasks[0].url, "https://p01.icloud-content.com/heic_orig");
        assert_eq!(tasks[0].size, 2000);
        assert_eq!(&*tasks[1].url, "https://p01.icloud-content.com/live_mov");
        assert_eq!(tasks[1].size, 3000);
        assert!(
            tasks[1]
                .download_path
                .to_str()
                .unwrap()
                .contains("IMG_0001_HEVC.MOV")
        );
    }

    #[test]
    fn test_filter_skips_live_photo_mov_when_image_only() {
        let asset = test_live_photo_asset();
        let mut config = test_config();
        config.live_photo_mode = LivePhotoMode::ImageOnly;
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 1);
        assert_eq!(&*tasks[0].url, "https://p01.icloud-content.com/heic_orig");
    }

    #[test]
    fn test_filter_live_photo_original_policy() {
        let asset = test_live_photo_asset();
        let mut config = test_config();
        config.live_photo_mov_filename_policy = crate::types::LivePhotoMovFilenamePolicy::Original;
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 2);
        assert!(
            tasks[1]
                .download_path
                .to_str()
                .unwrap()
                .contains("IMG_0001.MOV")
        );
    }

    #[test]
    fn test_filter_skips_existing_live_photo_mov() {
        let dir = TempDir::new().unwrap();

        let asset = test_live_photo_asset();
        let mut config = test_config();
        config.directory = std::sync::Arc::from(dir.path());

        // First call: both photo and MOV
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 2);

        // Create the MOV file on disk with matching size (3000 bytes)
        fs::create_dir_all(tasks[1].download_path.parent().unwrap()).unwrap();
        fs::write(&tasks[1].download_path, vec![0u8; 3000]).unwrap();

        // Second call: the MOV cannot be proven identical by path+size alone,
        // so it is routed to an identity collision path.
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 2);
        assert_eq!(&*tasks[0].url, "https://p01.icloud-content.com/heic_orig");
        assert!(
            tasks[1]
                .download_path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.contains("LIVE_1")),
            "same-size MOV collision must use an identity path, got {:?}",
            tasks[1].download_path
        );
    }

    #[test]
    fn test_filter_deduplicates_live_photo_mov_collision() {
        let dir = TempDir::new().unwrap();

        let asset = test_live_photo_asset();
        let mut config = test_config();
        config.directory = std::sync::Arc::from(dir.path());

        // First call to get the expected MOV path
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 2);
        let mov_path = &tasks[1].download_path;

        // Create a file at the MOV path with a DIFFERENT size (simulating a
        // regular video that collides with the live photo companion name).
        fs::create_dir_all(mov_path.parent().unwrap()).unwrap();
        fs::write(mov_path, vec![0u8; 9999]).unwrap();

        // Second call: should produce a deduped MOV path with asset ID suffix
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 2);
        assert_eq!(&*tasks[1].url, "https://p01.icloud-content.com/live_mov");
        let dedup_path = tasks[1].download_path.to_str().unwrap();
        assert!(
            dedup_path.contains("LIVE_1"),
            "Expected asset ID 'LIVE_1' in deduped path, got: {}",
            dedup_path,
        );
    }

    #[test]
    fn test_filter_live_photo_dedup_suffix_consistent_with_mov() {
        // Regression test for #102: when two live photos share the same base
        // filename but have different sizes (triggering dedup), the MOV companion
        // must derive from the deduped HEIC name so they remain visually paired.
        let dir = TempDir::new().unwrap();

        let asset1 = TestPhotoAsset::new("LIVE_A")
            .filename("IMG_0001.HEIC")
            .item_type("public.heic")
            .orig_file_type("public.heic")
            .orig_size(2000)
            .orig_url("https://p01.icloud-content.com/heic_a")
            .orig_checksum("ck_a")
            .live_photo("https://p01.icloud-content.com/mov_a", "mov_ck_a", 3000)
            .build();

        let asset2 = TestPhotoAsset::new("LIVE_B")
            .filename("IMG_0001.HEIC")
            .item_type("public.heic")
            .orig_file_type("public.heic")
            .orig_size(4000)
            .orig_url("https://p01.icloud-content.com/heic_b")
            .orig_checksum("ck_b")
            .live_photo("https://p01.icloud-content.com/mov_b", "mov_ck_b", 5000)
            .build();

        let mut config = test_config();
        config.directory = std::sync::Arc::from(dir.path());

        // Process asset1: creates IMG_0001.HEIC (2000 bytes) and its MOV
        let mut claimed_paths = FxHashMap::default();
        let mut dir_cache = paths::DirCache::new();
        let tasks1 = filter_asset_to_tasks(&asset1, &config, &mut claimed_paths, &mut dir_cache);
        assert_eq!(tasks1.len(), 2);
        let heic1_path = &tasks1[0].download_path;

        // Write asset1's HEIC to disk so asset2 sees a collision
        fs::create_dir_all(heic1_path.parent().unwrap()).unwrap();
        fs::write(heic1_path, vec![0u8; 2000]).unwrap();

        // Process asset2: same filename, different size → should dedup HEIC
        // Clear dir_cache since we just wrote a new file
        dir_cache.clear();
        let tasks2 = filter_asset_to_tasks(&asset2, &config, &mut claimed_paths, &mut dir_cache);
        assert_eq!(tasks2.len(), 2, "Expected HEIC + MOV tasks for asset2");

        let heic2_path = tasks2[0].download_path.to_str().unwrap();
        let mov2_path = tasks2[1].download_path.to_str().unwrap();

        // The deduped HEIC should have a size suffix
        assert!(
            heic2_path.contains("-4000."),
            "Expected size suffix '-4000.' in deduped HEIC path, got: {}",
            heic2_path,
        );

        // The MOV companion must also contain the size suffix from the HEIC,
        // keeping them visually paired (this is the #102 fix).
        assert!(
            mov2_path.contains("-4000"),
            "MOV companion should derive from deduped HEIC name (contain '-4000'), got: {}",
            mov2_path,
        );
    }

    #[test]
    fn live_photo_mov_reuses_existing_deduped_primary_stem_after_primary_skip() {
        let dir = TempDir::new().unwrap();

        let asset = TestPhotoAsset::new("AV0P9wRWvFhGzyYKSmpJu89S3bY6")
            .filename("FullSizeRender.HEIC")
            .item_type("public.heic")
            .orig_file_type("public.heic")
            .orig_size(2_067_405)
            .orig_url("https://p01.icloud-content.com/heic")
            .orig_checksum("heic_ck")
            .live_photo("https://p01.icloud-content.com/mov", "mov_ck", 2_266_088)
            .build();

        let mut config = test_config();
        config.directory = std::sync::Arc::from(dir.path());

        let natural_heic = paths::local_download_path(
            &config.directory,
            &config.folder_structure,
            &asset.created_local(),
            "FullSizeRender.HEIC",
            None,
        );
        let deduped_heic = paths::local_download_path(
            &config.directory,
            &config.folder_structure,
            &asset.created_local(),
            "FullSizeRender-2067405.HEIC",
            None,
        );
        let natural_mov = paths::local_download_path(
            &config.directory,
            &config.folder_structure,
            &asset.created_local(),
            "FullSizeRender_HEVC.MOV",
            None,
        );
        let paired_mov = paths::local_download_path(
            &config.directory,
            &config.folder_structure,
            &asset.created_local(),
            "FullSizeRender-2067405_HEVC.MOV",
            None,
        );
        fs::create_dir_all(natural_heic.parent().unwrap()).unwrap();
        fs::write(&natural_heic, vec![0u8; 1_808_776]).unwrap();
        fs::write(&deduped_heic, vec![0u8; 2_067_405]).unwrap();
        fs::write(&natural_mov, vec![0u8; 123]).unwrap();

        let mut claimed_paths = FxHashMap::default();
        let mut dir_cache = paths::DirCache::new();
        let tasks = filter_asset_to_tasks(&asset, &config, &mut claimed_paths, &mut dir_cache);
        assert_eq!(
            tasks.len(),
            2,
            "without state identity, the existing same-size HEIC is routed to an identity path"
        );
        assert!(
            tasks[0]
                .download_path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.contains("AV0P9wRWvFhGzyYKSmpJu89S3bY6")),
            "same-size HEIC collision must use an identity path: {:?}",
            tasks[0].download_path
        );
        assert_ne!(
            tasks[1].download_path, paired_mov,
            "MOV pairing follows the newly planned identity-path primary when state has not proven the existing HEIC"
        );

        fs::write(&paired_mov, vec![0u8; 2_266_088]).unwrap();
        dir_cache.clear();
        let tasks = filter_asset_to_tasks(&asset, &config, &mut claimed_paths, &mut dir_cache);
        assert_eq!(
            tasks.len(),
            2,
            "same-size existing HEIC/MOV files are not safe skips without state identity"
        );
    }

    #[test]
    fn test_filter_live_photo_medium_size() {
        let asset = PhotoAsset::new(
            json!({"recordName": "LIVE_MED", "fields": {
                "filenameEnc": {"value": "IMG_0002.HEIC", "type": "STRING"},
                "itemType": {"value": "public.heic"},
                "resOriginalRes": {"value": {
                    "size": 2000,
                    "downloadURL": "https://p01.icloud-content.com/heic_orig",
                    "fileChecksum": "heic_ck"
                }},
                "resOriginalFileType": {"value": "public.heic"},
                "resVidMedRes": {"value": {
                    "size": 1500,
                    "downloadURL": "https://p01.icloud-content.com/live_med",
                    "fileChecksum": "med_ck"
                }},
                "resVidMedFileType": {"value": "com.apple.quicktime-movie"}
            }}),
            json!({"fields": {"assetDate": {"value": 1736899200000.0}}}),
        );
        let mut config = test_config();
        config.live_resolution = AssetVersionSize::LiveMedium;
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 2);
        assert_eq!(&*tasks[1].url, "https://p01.icloud-content.com/live_med");
    }

    #[test]
    fn test_filter_no_live_photo_for_videos() {
        let asset = TestPhotoAsset::new("VID_1")
            .filename("movie.mov")
            .item_type("com.apple.quicktime-movie")
            .orig_file_type("com.apple.quicktime-movie")
            .orig_size(50000)
            .orig_url("https://p01.icloud-content.com/vid")
            .orig_checksum("vid_ck")
            .live_photo("https://p01.icloud-content.com/live_mov", "mov_ck", 3000)
            .build();
        let config = test_config();
        let tasks = filter_asset_fresh(&asset, &config);
        // Videos should get 1 task (the video itself), not a live photo MOV
        assert_eq!(tasks.len(), 1);
    }

    fn photo_asset_with_original_and_alternative(orig_type: &str, alt_type: &str) -> PhotoAsset {
        TestPhotoAsset::new("RAW_TEST")
            .orig_checksum("orig_ck")
            .orig_file_type(orig_type)
            .alt_version(
                "https://p01.icloud-content.com/alt",
                "alt_ck",
                2000,
                alt_type,
            )
            .build()
    }

    /// Helper to get a version from a `VersionsView` by key.
    fn get_ver<'a>(view: &VersionsView<'a>, key: AssetVersionSize) -> Option<&'a AssetVersion> {
        view.get(key)
    }

    /// Helper to check whether a version exists in a `VersionsView`.
    fn has_ver(view: &VersionsView<'_>, key: AssetVersionSize) -> bool {
        view.iter().any(|(k, _)| k == key)
    }

    #[test]
    fn test_raw_policy_as_is_no_swap() {
        let asset = photo_asset_with_original_and_alternative("public.jpeg", "com.adobe.raw-image");
        let versions = apply_raw_policy(asset.versions(), RawPolicy::AsIs);
        assert_eq!(
            &*get_ver(&versions, AssetVersionSize::Original).unwrap().url,
            "https://p01.icloud-content.com/orig"
        );
        assert_eq!(
            &*get_ver(&versions, AssetVersionSize::Alternative)
                .unwrap()
                .url,
            "https://p01.icloud-content.com/alt"
        );
    }

    #[test]
    fn ordinary_original_metadata_refresh_does_not_require_resource_urls_or_checksums() {
        for item_type in ["public.jpeg", "com.apple.quicktime-movie"] {
            for resource in [
                serde_json::json!(null),
                serde_json::json!({"value": {"fileChecksum": "replacement"}}),
                serde_json::json!({"value": {"fileChecksum": "previous"}}),
            ] {
                let asset = PhotoAsset::new(
                    serde_json::json!({"recordName": "master", "fields": {
                        "itemType": {"value": item_type},
                        "resOriginalRes": resource,
                        "resOriginalWidth": {"value": 1920},
                        "resOriginalHeight": {"value": 1080},
                    }}),
                    serde_json::json!({"fields": {"duration": {"value": 12.5}, "isFavorite": {"value": 1}}}),
                );
                assert!(asset.versions().is_empty());
                let capture = metadata_capture(&asset);
                let original = capture.resolve(VersionSizeKey::Original, "previous");
                let matched = asset.rendition_facts().iter().any(|(key, facts)| {
                    *key == VersionSizeKey::Original
                        && facts.checksum.as_deref() == Some("previous")
                });
                assert_eq!(original.width, matched.then_some(1920));
                assert_eq!(original.height, matched.then_some(1080));
                assert_eq!(original.duration_secs, matched.then_some(12.5));
                assert!(original.is_favorite);
                assert_eq!(original.metadata_hash, Some(original.compute_hash()));
                assert_eq!(
                    asset.metadata_arc(VersionSizeKey::Original).width,
                    Some(1920)
                );
                // Either logical key may hold the original provider bytes.
                assert_eq!(
                    capture
                        .resolve(VersionSizeKey::Alternative, "previous")
                        .metadata_hash,
                    original.metadata_hash
                );
            }
        }
    }

    #[test]
    fn raw_policy_stored_metadata_requires_unambiguous_url_independent_evidence() {
        let master = serde_json::json!({"recordName": "master", "fields": {
            "itemType": {"value": "public.jpeg"},
            "resOriginalRes": {"value": {"fileChecksum": "original"}},
            "resOriginalAltRes": {"value": {"fileChecksum": "alternative"}},
            "resOriginalWidth": {"value": 4000},
            "resOriginalAltWidth": {"value": 6000},
        }});
        for (fields, checksum, expected) in [
            (serde_json::json!({}), "alternative", Some(6000)),
            (serde_json::json!({}), "original", Some(4000)),
            (serde_json::json!({}), "unknown", None),
            (
                serde_json::json!({
                    "resOriginalAltRes": {"value": {"fileChecksum": "alternative"}},
                    "resOriginalAltWidth": {"value": 4000}
                }),
                "unknown",
                None,
            ),
            (
                serde_json::json!({"resOriginalAltRes": {"value": null}}),
                "alternative",
                None,
            ),
            (
                serde_json::json!({
                    "resOriginalAltRes": {"value": {"fileChecksum": "original"}},
                    "resOriginalAltWidth": {"value": 6000},
                }),
                "original",
                None,
            ),
        ] {
            let asset = PhotoAsset::new(master.clone(), serde_json::json!({"fields": fields}));
            assert!(asset.versions().is_empty());
            let capture = metadata_capture(&asset);
            for key in [VersionSizeKey::Original, VersionSizeKey::Alternative] {
                assert_eq!(capture.resolve(key, checksum).width, expected);
            }
        }
        let typed = PhotoAsset::from_records(
            serde_json::from_value(master).unwrap(),
            &serde_json::from_value(serde_json::json!({"recordName": "asset", "fields": {}}))
                .unwrap(),
        );
        assert_eq!(
            metadata_capture(&typed)
                .resolve(VersionSizeKey::Alternative, "alternative")
                .width,
            Some(6000)
        );
    }

    #[test]
    fn test_raw_policy_prefer_raw_swaps_when_alt_is_raw() {
        let asset = photo_asset_with_original_and_alternative("public.jpeg", "com.adobe.raw-image");
        let versions = apply_raw_policy(asset.versions(), RawPolicy::PreferRaw);
        // Alternative was RAW → swap: Original now has alt URL
        assert_eq!(
            &*get_ver(&versions, AssetVersionSize::Original).unwrap().url,
            "https://p01.icloud-content.com/alt"
        );
        assert_eq!(
            &*get_ver(&versions, AssetVersionSize::Alternative)
                .unwrap()
                .url,
            "https://p01.icloud-content.com/orig"
        );
    }

    #[test]
    fn test_raw_policy_prefer_jpeg_swaps_when_orig_is_raw() {
        let asset = photo_asset_with_original_and_alternative("com.adobe.raw-image", "public.jpeg");
        let versions = apply_raw_policy(asset.versions(), RawPolicy::PreferJpeg);
        // Original was RAW → swap: Alternative now has orig URL
        assert_eq!(
            &*get_ver(&versions, AssetVersionSize::Original).unwrap().url,
            "https://p01.icloud-content.com/alt"
        );
        assert_eq!(
            &*get_ver(&versions, AssetVersionSize::Alternative)
                .unwrap()
                .url,
            "https://p01.icloud-content.com/orig"
        );
    }

    #[test]
    fn test_raw_policy_prefer_raw_no_swap_when_alt_not_raw() {
        let asset = photo_asset_with_original_and_alternative("public.jpeg", "public.jpeg");
        let versions = apply_raw_policy(asset.versions(), RawPolicy::PreferRaw);
        assert_eq!(
            &*get_ver(&versions, AssetVersionSize::Original).unwrap().url,
            "https://p01.icloud-content.com/orig"
        );
    }

    #[test]
    fn test_raw_policy_prefer_jpeg_no_swap_when_orig_not_raw() {
        let asset = photo_asset_with_original_and_alternative("public.jpeg", "public.jpeg");
        let versions = apply_raw_policy(asset.versions(), RawPolicy::PreferJpeg);
        assert_eq!(
            &*get_ver(&versions, AssetVersionSize::Original).unwrap().url,
            "https://p01.icloud-content.com/orig"
        );
    }

    #[test]
    fn test_raw_policy_no_alternative_no_swap() {
        let asset = TestPhotoAsset::new("TEST_1").build(); // only has Original
        let versions = apply_raw_policy(asset.versions(), RawPolicy::PreferRaw);
        assert_eq!(
            &*get_ver(&versions, AssetVersionSize::Original).unwrap().url,
            "https://p01.icloud-content.com/orig"
        );
        assert!(!has_ver(&versions, AssetVersionSize::Alternative));
    }

    /// On a swap, `iter()` must preserve the underlying `VersionsMap`
    /// element order — only the keys at the two swap slots flip. If a
    /// future refactor reorders elements (e.g. surfacing `Original`
    /// first regardless of position), this fails loudly.
    #[test]
    fn raw_policy_view_iter_order_matches_underlying_map() {
        let asset = photo_asset_with_original_and_alternative("public.jpeg", "com.adobe.raw-image");
        let view = apply_raw_policy(asset.versions(), RawPolicy::PreferRaw);

        let elements: Vec<(AssetVersionSize, &str)> =
            view.iter().map(|(k, v)| (k, v.url.as_ref())).collect();
        assert_eq!(elements.len(), 2);
        // Slot 0 (originally Original) reads as Alternative, still
        // pointing at orig_url.
        assert_eq!(elements[0].0, AssetVersionSize::Alternative);
        assert_eq!(elements[0].1, "https://p01.icloud-content.com/orig");
        // Slot 1 (originally Alternative) reads as Original, still
        // pointing at alt_url.
        assert_eq!(elements[1].0, AssetVersionSize::Original);
        assert_eq!(elements[1].1, "https://p01.icloud-content.com/alt");
    }

    /// `AsIs` policy must yield the underlying map verbatim — same
    /// keys, same order — so callers see identical data to bypassing
    /// `apply_raw_policy` entirely.
    #[test]
    fn raw_policy_unchanged_yields_underlying_map_verbatim() {
        let asset = photo_asset_with_original_and_alternative("public.jpeg", "com.adobe.raw-image");
        let view = apply_raw_policy(asset.versions(), RawPolicy::AsIs);

        let got: Vec<(AssetVersionSize, &str)> =
            view.iter().map(|(k, v)| (k, v.url.as_ref())).collect();
        let want: Vec<(AssetVersionSize, &str)> = asset
            .versions()
            .iter()
            .map(|(k, v)| (*k, v.url.as_ref()))
            .collect();
        assert_eq!(got, want);
    }

    #[test]
    fn test_filter_asset_uses_raw_policy_swap() {
        let asset = photo_asset_with_original_and_alternative("public.jpeg", "com.adobe.raw-image");
        let mut config = test_config();
        config.raw_policy = RawPolicy::PreferRaw;
        // With AsOriginal and RAW alternative, the swap makes Original point to alt URL
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 1);
        assert_eq!(&*tasks[0].url, "https://p01.icloud-content.com/alt");
        assert_eq!(&*tasks[0].checksum, "alt_ck");
    }

    fn original_adjusted_alternative_asset() -> PhotoAsset {
        PhotoAsset::new(
            json!({"recordName": "PR4_MULTI", "fields": {
                "filenameEnc": {"value": "IMG_PR4.JPG", "type": "STRING"},
                "itemType": {"value": "public.jpeg"},
                "resOriginalRes": {"value": {
                    "size": 1000_u64,
                    "downloadURL": "https://p01.icloud-content.com/orig",
                    "fileChecksum": "orig_ck"
                }},
                "resOriginalFileType": {"value": "public.jpeg"},
                "resJPEGFullRes": {"value": {
                    "size": 900_u64,
                    "downloadURL": "https://p01.icloud-content.com/edited",
                    "fileChecksum": "edited_ck"
                }},
                "resJPEGFullFileType": {"value": "public.jpeg"},
                "resOriginalAltRes": {"value": {
                    "size": 2000_u64,
                    "downloadURL": "https://p01.icloud-content.com/alt",
                    "fileChecksum": "alt_ck"
                }},
                "resOriginalAltFileType": {"value": "public.jpeg"}
            }}),
            json!({"fields": {"assetDate": {"value": 1_736_899_200_000.0_f64}}}),
        )
    }

    #[test]
    fn edited_and_alternative_emit_three_versions_in_order() {
        let asset = original_adjusted_alternative_asset();
        let mut config = test_config();
        config.edited = true;
        config.alternative = true;

        let paths = expected_paths_for(&asset, &config);
        let versions: Vec<VersionSizeKey> = paths.iter().map(|p| p.version_size).collect();
        assert_eq!(
            versions,
            vec![
                VersionSizeKey::Original,
                VersionSizeKey::Adjusted,
                VersionSizeKey::Alternative
            ]
        );
        let names: Vec<String> = paths
            .iter()
            .map(|p| p.path.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(names[0], "IMG_PR4.JPG");
        assert_eq!(names[1], "IMG_PR4_edited.JPG");
        assert_eq!(names[2], "IMG_PR4_alt.JPG");
    }

    #[test]
    fn edited_and_alternative_use_fingerprint_before_suffix_when_unicode_strip_empties_stem() {
        let asset = TestPhotoAsset::new("UNI_EXTRA")
            .filename("日本語.jpg")
            .adjusted_version(
                "https://p01.icloud-content.com/edited",
                "edited_ck",
                900,
                "public.jpeg",
            )
            .alt_version(
                "https://p01.icloud-content.com/alt",
                "alt_ck",
                2000,
                "public.jpeg",
            )
            .build();
        let mut config = test_config();
        config.edited = true;
        config.alternative = true;

        let paths = expected_paths_for(&asset, &config);
        let names: Vec<String> = paths
            .iter()
            .map(|p| p.path.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        let fingerprint = paths::generate_fingerprint_filename("UNI_EXTRA", "public.jpeg");
        assert_eq!(names[0], fingerprint);
        assert_eq!(
            names[1],
            paths::insert_literal_suffix(&fingerprint, "_edited")
        );
        assert_eq!(names[2], paths::insert_literal_suffix(&fingerprint, "_alt"));
    }

    #[test]
    fn edited_live_photo_emits_adjusted_image_and_live_adjusted_video() {
        let asset = TestPhotoAsset::new("PR4_LIVE_EDITED")
            .filename("IMG_LIVE.HEIC")
            .item_type("public.heic")
            .orig_file_type("public.heic")
            .adjusted_version(
                "https://p01.icloud-content.com/edited_image",
                "edited_image_ck",
                900,
                "public.heic",
            )
            .live_adjusted(
                "https://p01.icloud-content.com/edited_mov",
                "edited_mov_ck",
                2500,
            )
            .live_photo("https://p01.icloud-content.com/live_mov", "mov_ck", 3000)
            .build();
        let mut config = test_config();
        config.edited = true;

        let paths = expected_paths_for(&asset, &config);
        let versions: Vec<VersionSizeKey> = paths.iter().map(|p| p.version_size).collect();
        assert_eq!(
            versions,
            vec![
                VersionSizeKey::Original,
                VersionSizeKey::Adjusted,
                VersionSizeKey::LiveAdjusted,
                VersionSizeKey::LiveOriginal,
            ]
        );
        let names: Vec<String> = paths
            .iter()
            .map(|p| p.path.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(names[0], "IMG_LIVE.HEIC");
        assert_eq!(names[1], "IMG_LIVE_edited.HEIC");
        assert_eq!(names[2], "IMG_LIVE_edited.MOV");
        assert_eq!(names[3], "IMG_LIVE_HEVC.MOV");
    }

    #[test]
    fn video_only_live_photo_uses_fingerprint_mov_when_unicode_strip_empties_stem() {
        let asset = TestPhotoAsset::new("UNI_LIVE_MOV")
            .filename("日本語.HEIC")
            .item_type("public.heic")
            .orig_file_type("public.heic")
            .live_photo("https://p01.icloud-content.com/mov", "mov_ck", 3000)
            .build();
        let mut config = test_config();
        config.live_photo_mode = LivePhotoMode::VideoOnly;

        let paths = expected_paths_for(&asset, &config);
        assert_eq!(paths.len(), 1);
        let name = paths[0].path.file_name().unwrap().to_string_lossy();
        let image_fingerprint = paths::generate_fingerprint_filename("UNI_LIVE_MOV", "public.heic");
        assert_eq!(
            name,
            paths::live_photo_mov_path_suffix(&image_fingerprint),
            "video-only live companion must not collapse to an extension-only MOV"
        );
    }

    #[test]
    fn resolution_none_with_alternative_emits_only_alternative() {
        let asset = original_adjusted_alternative_asset();
        let mut config = test_config();
        config.resolution = crate::types::PhotoResolution::None;
        config.alternative = true;

        let paths = expected_paths_for(&asset, &config);
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].version_size, VersionSizeKey::Alternative);
    }

    #[test]
    fn raw_policy_prefer_raw_makes_raw_primary_and_jpeg_extra() {
        let asset = photo_asset_with_original_and_alternative("public.jpeg", "com.adobe.raw-image");
        let mut config = test_config();
        config.raw_policy = RawPolicy::PreferRaw;
        config.alternative = true;

        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 2);
        assert_eq!(&*tasks[0].url, "https://p01.icloud-content.com/alt");
        assert_eq!(tasks[0].version_size, VersionSizeKey::Original);
        assert_eq!(&*tasks[1].url, "https://p01.icloud-content.com/orig");
        assert_eq!(tasks[1].version_size, VersionSizeKey::Alternative);
    }

    #[test]
    fn raw_policy_prefer_jpeg_makes_jpeg_primary_and_raw_extra() {
        let asset = photo_asset_with_original_and_alternative("com.adobe.raw-image", "public.jpeg");
        let mut config = test_config();
        config.raw_policy = RawPolicy::PreferJpeg;
        config.alternative = true;

        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 2);
        assert_eq!(&*tasks[0].url, "https://p01.icloud-content.com/alt");
        assert_eq!(tasks[0].version_size, VersionSizeKey::Original);
        assert_eq!(&*tasks[1].url, "https://p01.icloud-content.com/orig");
        assert_eq!(tasks[1].version_size, VersionSizeKey::Alternative);
        let raw_extra = tasks[1]
            .download_path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(
            raw_extra.contains("_RAW."),
            "raw alternative extra should use _RAW suffix, got {raw_extra}"
        );
    }

    #[test]
    fn resolution_none_with_missing_edited_extra_skips_asset() {
        let asset = TestPhotoAsset::new("PR4_NONE_NO_EDITED").build();
        let mut config = test_config();
        config.resolution = crate::types::PhotoResolution::None;
        config.edited = true;

        assert!(expected_paths_for(&asset, &config).is_empty());
        assert!(filter_asset_fresh(&asset, &config).is_empty());
    }

    #[test]
    fn force_resolution_does_not_force_missing_extras() {
        let asset = TestPhotoAsset::new("PR4_FORCE_EXTRAS").build();
        let mut config = test_config();
        config.edited = true;
        config.alternative = true;
        config.force_resolution = true;

        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].version_size, VersionSizeKey::Original);
    }

    #[test]
    fn live_resolution_medium_with_edited_emits_live_medium_and_live_adjusted() {
        let asset = PhotoAsset::new(
            json!({"recordName": "PR4_LIVE_MEDIUM_EDITED", "fields": {
                "filenameEnc": {"value": "IMG_LIVE_MEDIUM.HEIC", "type": "STRING"},
                "itemType": {"value": "public.heic"},
                "resOriginalRes": {"value": {
                    "size": 4000_u64,
                    "downloadURL": "https://p01.icloud-content.com/heic_orig",
                    "fileChecksum": "heic_ck"
                }},
                "resOriginalFileType": {"value": "public.heic"},
                "resOriginalVidComplRes": {"value": {
                    "size": 3000_u64,
                    "downloadURL": "https://p01.icloud-content.com/live_orig",
                    "fileChecksum": "live_orig_ck"
                }},
                "resOriginalVidComplFileType": {"value": "com.apple.quicktime-movie"},
                "resVidMedRes": {"value": {
                    "size": 1500_u64,
                    "downloadURL": "https://p01.icloud-content.com/live_med",
                    "fileChecksum": "live_med_ck"
                }},
                "resVidMedFileType": {"value": "com.apple.quicktime-movie"},
                "resVidComplRes": {"value": {
                    "size": 2500_u64,
                    "downloadURL": "https://p01.icloud-content.com/live_adjusted",
                    "fileChecksum": "live_adjusted_ck"
                }},
                "resVidComplFileType": {"value": "com.apple.quicktime-movie"}
            }}),
            json!({"fields": {"assetDate": {"value": 1_736_899_200_000.0_f64}}}),
        );
        let mut config = test_config();
        config.live_resolution = AssetVersionSize::LiveMedium;
        config.edited = true;

        let paths = expected_paths_for(&asset, &config);
        let versions: Vec<VersionSizeKey> = paths.iter().map(|p| p.version_size).collect();
        assert_eq!(
            versions,
            vec![
                VersionSizeKey::Original,
                VersionSizeKey::LiveAdjusted,
                VersionSizeKey::LiveMedium,
            ]
        );
        assert_eq!(&*paths[1].checksum, "live_adjusted_ck");
        assert_eq!(&*paths[2].checksum, "live_med_ck");
    }

    #[test]
    fn live_edited_extra_reads_vidcompl_not_vidfull() {
        let asset = PhotoAsset::new(
            json!({"recordName": "PR4_LIVE_EDITED_VIDCOMPL", "fields": {
                "filenameEnc": {"value": "IMG_LIVE_EDITED.HEIC", "type": "STRING"},
                "itemType": {"value": "public.heic"},
                "resOriginalRes": {"value": {
                    "size": 4000_u64,
                    "downloadURL": "https://p01.icloud-content.com/heic_orig",
                    "fileChecksum": "heic_ck"
                }},
                "resOriginalFileType": {"value": "public.heic"},
                "resOriginalVidComplRes": {"value": {
                    "size": 3000_u64,
                    "downloadURL": "https://p01.icloud-content.com/live_orig",
                    "fileChecksum": "live_orig_ck"
                }},
                "resOriginalVidComplFileType": {"value": "com.apple.quicktime-movie"},
                "resVidComplRes": {"value": {
                    "size": 2500_u64,
                    "downloadURL": "https://p01.icloud-content.com/live_adjusted",
                    "fileChecksum": "live_adjusted_ck"
                }},
                "resVidComplFileType": {"value": "com.apple.quicktime-movie"},
                "resVidFullRes": {"value": {
                    "size": 2400_u64,
                    "downloadURL": "https://p01.icloud-content.com/vid_full",
                    "fileChecksum": "vid_full_ck"
                }},
                "resVidFullFileType": {"value": "com.apple.quicktime-movie"}
            }}),
            json!({"fields": {"assetDate": {"value": 1_736_899_200_000.0_f64}}}),
        );
        let mut config = test_config();
        config.edited = true;

        let paths = expected_paths_for(&asset, &config);
        let adjusted = paths
            .iter()
            .find(|path| path.version_size == VersionSizeKey::LiveAdjusted)
            .expect("edited live photo must yield a LiveAdjusted task");
        assert_eq!(&*adjusted.checksum, "live_adjusted_ck");
    }

    #[test]
    fn resolution_none_live_edited_keeps_import_and_sync_mov_name_in_parity() {
        let asset = TestPhotoAsset::new("PR4_NONE_LIVE_EDITED")
            .filename("IMG_NONE_LIVE.HEIC")
            .item_type("public.heic")
            .orig_file_type("public.heic")
            .adjusted_version(
                "https://p01.icloud-content.com/edited_image",
                "edited_image_ck",
                900,
                "public.heic",
            )
            .live_adjusted(
                "https://p01.icloud-content.com/edited_mov",
                "edited_mov_ck",
                2500,
            )
            .live_photo("https://p01.icloud-content.com/live_mov", "mov_ck", 3000)
            .build();
        let mut config = test_config();
        config.resolution = crate::types::PhotoResolution::None;
        config.edited = true;

        let expected = expected_paths_for(&asset, &config);
        let tasks = filter_asset_fresh(&asset, &config);
        let expected_mov = expected
            .iter()
            .find(|p| p.version_size == VersionSizeKey::LiveOriginal)
            .expect("expected import MOV path");
        let task_mov = tasks
            .iter()
            .find(|t| t.version_size == VersionSizeKey::LiveOriginal)
            .expect("expected sync MOV task");

        let expected_name = expected_mov.path.file_name().unwrap().to_string_lossy();
        let task_name = task_mov
            .download_path
            .file_name()
            .unwrap()
            .to_string_lossy();
        assert_eq!(
            expected_name, task_name,
            "import-existing and sync must agree on MOV filename when primary is disabled"
        );
        assert!(
            !expected_name.contains("_edited"),
            "original MOV must not inherit the edited still filename: {expected_name}"
        );
    }

    #[test]
    fn duplicate_extra_url_is_not_emitted_twice() {
        let asset = PhotoAsset::new(
            json!({"recordName": "PR4_DEDUP", "fields": {
                "filenameEnc": {"value": "IMG_DEDUP.JPG", "type": "STRING"},
                "itemType": {"value": "public.jpeg"},
                "resOriginalRes": {"value": {
                    "size": 1000_u64,
                    "downloadURL": "https://p01.icloud-content.com/same",
                    "fileChecksum": "orig_ck"
                }},
                "resOriginalFileType": {"value": "public.jpeg"},
                "resJPEGFullRes": {"value": {
                    "size": 1000_u64,
                    "downloadURL": "https://p01.icloud-content.com/same",
                    "fileChecksum": "edited_ck"
                }},
                "resJPEGFullFileType": {"value": "public.jpeg"}
            }}),
            json!({"fields": {"assetDate": {"value": 1_736_899_200_000.0_f64}}}),
        );
        let mut config = test_config();
        config.edited = true;

        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].version_size, VersionSizeKey::Original);
    }

    #[test]
    fn edited_and_alternative_same_url_keeps_first_extra_only() {
        let asset = PhotoAsset::new(
            json!({"recordName": "PR4_EXTRA_DEDUP", "fields": {
                "filenameEnc": {"value": "IMG_EXTRA_DEDUP.JPG", "type": "STRING"},
                "itemType": {"value": "public.jpeg"},
                "resOriginalRes": {"value": {
                    "size": 1000_u64,
                    "downloadURL": "https://p01.icloud-content.com/original",
                    "fileChecksum": "orig_ck"
                }},
                "resOriginalFileType": {"value": "public.jpeg"},
                "resJPEGFullRes": {"value": {
                    "size": 900_u64,
                    "downloadURL": "https://p01.icloud-content.com/same_extra",
                    "fileChecksum": "extra_ck"
                }},
                "resJPEGFullFileType": {"value": "public.jpeg"},
                "resOriginalAltRes": {"value": {
                    "size": 900_u64,
                    "downloadURL": "https://p01.icloud-content.com/same_extra",
                    "fileChecksum": "extra_ck"
                }},
                "resOriginalAltFileType": {"value": "public.jpeg"}
            }}),
            json!({"fields": {"assetDate": {"value": 1_736_899_200_000.0_f64}}}),
        );
        let mut config = test_config();
        config.edited = true;
        config.alternative = true;

        let tasks = filter_asset_fresh(&asset, &config);
        let versions: Vec<VersionSizeKey> = tasks.iter().map(|t| t.version_size).collect();
        assert_eq!(
            versions,
            vec![VersionSizeKey::Original, VersionSizeKey::Adjusted]
        );

        let expected = expected_paths_for(&asset, &config);
        let expected_versions: Vec<VersionSizeKey> =
            expected.iter().map(|p| p.version_size).collect();
        assert_eq!(expected_versions, versions);
    }

    #[test]
    fn test_filter_detects_case_insensitive_collision() {
        // On case-insensitive filesystems (macOS, Windows), IMG_0996.mov and IMG_0996.MOV
        // are the same file. Test that claimed_paths detects this collision.
        let dir = TempDir::new().unwrap();

        // First asset: regular video IMG_0996.mov
        let video_asset = TestPhotoAsset::new("VID_0996")
            .filename("IMG_0996.mov")
            .item_type("com.apple.quicktime-movie")
            .orig_file_type("com.apple.quicktime-movie")
            .orig_size(258592890)
            .orig_url("https://p01.icloud-content.com/vid")
            .orig_checksum("vid_ck")
            .asset_date(1713657600000.0)
            .build();

        // Second asset: live photo IMG_0996.JPG whose MOV companion would be IMG_0996.MOV
        let photo_asset = TestPhotoAsset::new("IMG_0996")
            .filename("IMG_0996.JPG")
            .orig_size(5000)
            .orig_url("https://p01.icloud-content.com/jpg")
            .orig_checksum("jpg_ck")
            .live_photo(
                "https://p01.icloud-content.com/live_mov",
                "mov_ck",
                124037918,
            )
            .asset_date(1713657600000.0)
            .build();

        let mut config = test_config();
        config.directory = std::sync::Arc::from(dir.path());

        // Process both assets through claimed_paths
        let mut claimed_paths = FxHashMap::default();
        let mut dir_cache = paths::DirCache::new();
        let video_tasks =
            filter_asset_to_tasks(&video_asset, &config, &mut claimed_paths, &mut dir_cache);
        assert_eq!(video_tasks.len(), 1);
        let video_path = &video_tasks[0].download_path;
        eprintln!("Video path: {:?}", video_path);

        let photo_tasks =
            filter_asset_to_tasks(&photo_asset, &config, &mut claimed_paths, &mut dir_cache);
        assert_eq!(photo_tasks.len(), 2, "Expected 2 tasks (photo + MOV)");

        let mov_task = &photo_tasks[1];
        let mov_path = &mov_task.download_path;
        eprintln!("Live MOV path: {:?}", mov_path);
        eprintln!(
            "Claimed paths: {:?}",
            claimed_paths.keys().collect::<Vec<_>>()
        );

        // Both the video (.mov) and the live-photo MOV get their extension
        // mapped to uppercase .MOV via ITEM_TYPE_EXTENSIONS, so they collide
        // on ALL platforms (not just case-insensitive ones).
        let mov_filename = mov_path.file_name().unwrap().to_str().unwrap();
        assert!(
            mov_filename.contains("-IMG_0996"),
            "MOV should be deduped with asset ID suffix due to path collision. Got: {}",
            mov_filename
        );
    }

    #[test]
    fn test_filter_asset_as_is_downloads_original() {
        let asset = photo_asset_with_original_and_alternative("public.jpeg", "com.adobe.raw-image");
        let config = test_config(); // raw_policy defaults to AsIs
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 1);
        assert_eq!(&*tasks[0].url, "https://p01.icloud-content.com/orig");
        assert_eq!(&*tasks[0].checksum, "orig_ck");
    }

    #[test]
    fn test_download_task_size() {
        use std::mem::size_of;
        assert!(
            size_of::<DownloadTask>() <= 200,
            "DownloadTask size {} exceeds 200 bytes",
            size_of::<DownloadTask>()
        );
    }

    // ── extract_skip_candidates tests ──────────────────────────────

    struct SkipCandidateCase {
        name: &'static str,
        asset: fn() -> PhotoAsset,
        configure: fn(&mut DownloadConfig),
        expected: &'static [(VersionSizeKey, &'static str)],
    }

    #[test]
    fn extract_skip_candidates_decision_matrix() {
        let cases = [
            SkipCandidateCase {
                name: "plain photo emits original",
                asset: plain_photo_asset,
                configure: no_config_change,
                expected: &[(VersionSizeKey::Original, "abc123")],
            },
            SkipCandidateCase {
                name: "live photo emits primary and mov",
                asset: test_live_photo_asset,
                configure: no_config_change,
                expected: &[
                    (VersionSizeKey::Original, "heic_ck"),
                    (VersionSizeKey::LiveOriginal, "mov_ck"),
                ],
            },
            SkipCandidateCase {
                name: "image-only live photo emits primary only",
                asset: test_live_photo_asset,
                configure: live_photo_image_only,
                expected: &[(VersionSizeKey::Original, "heic_ck")],
            },
            SkipCandidateCase {
                name: "skip mode does not affect non-live photos",
                asset: plain_photo_asset,
                configure: skip_live_photos,
                expected: &[(VersionSizeKey::Original, "abc123")],
            },
            SkipCandidateCase {
                name: "video-only live photo emits mov only",
                asset: test_live_photo_asset,
                configure: live_photo_video_only,
                expected: &[(VersionSizeKey::LiveOriginal, "mov_ck")],
            },
            SkipCandidateCase {
                name: "missing medium resolution falls back to original",
                asset: plain_photo_asset,
                configure: medium_resolution_with_fallback,
                expected: &[(VersionSizeKey::Original, "abc123")],
            },
            SkipCandidateCase {
                name: "force medium resolution prevents fallback",
                asset: plain_photo_asset,
                configure: medium_resolution_without_fallback,
                expected: &[],
            },
            SkipCandidateCase {
                name: "filename exclude no-match still emits original",
                asset: plain_photo_asset,
                configure: exclude_aae_filenames,
                expected: &[(VersionSizeKey::Original, "abc123")],
            },
            SkipCandidateCase {
                name: "missing live adjusted falls back to live original",
                asset: test_live_photo_asset,
                configure: live_adjusted_with_fallback,
                expected: &[
                    (VersionSizeKey::Original, "heic_ck"),
                    (VersionSizeKey::LiveOriginal, "mov_ck"),
                ],
            },
            SkipCandidateCase {
                name: "force live adjusted prevents mov fallback",
                asset: test_live_photo_asset,
                configure: live_adjusted_without_fallback,
                expected: &[(VersionSizeKey::Original, "heic_ck")],
            },
        ];

        for case in cases {
            let asset = (case.asset)();
            let mut config = test_config();
            (case.configure)(&mut config);

            let candidates = extract_skip_candidates(&asset, &config);
            let actual: Vec<_> = candidates
                .iter()
                .map(|(version, checksum)| (*version, *checksum))
                .collect();
            assert_eq!(actual.as_slice(), case.expected, "{}", case.name);
        }
    }

    #[test]
    fn test_filter_live_adjusted_falls_back_to_live_original() {
        let asset = test_live_photo_asset(); // has LiveOriginal, no LiveAdjusted
        let mut config = test_config();
        config.live_resolution = AssetVersionSize::LiveAdjusted;
        config.force_resolution = false;
        let tasks = filter_asset_fresh(&asset, &config);
        // Should produce 2 tasks: primary + live companion (fallback to LiveOriginal)
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[1].version_size, VersionSizeKey::LiveOriginal);
        assert_eq!(&*tasks[1].url, "https://p01.icloud-content.com/live_mov");
    }

    #[test]
    fn test_filter_live_adjusted_force_resolution_no_fallback() {
        let asset = test_live_photo_asset(); // has LiveOriginal, no LiveAdjusted
        let mut config = test_config();
        config.live_resolution = AssetVersionSize::LiveAdjusted;
        config.force_resolution = true;
        let tasks = filter_asset_fresh(&asset, &config);
        // force_resolution prevents fallback — only primary, no live companion
        assert_eq!(tasks.len(), 1);
    }

    // ── determine_media_type tests ──────────────────────────────────────

    #[test]
    fn test_determine_media_type_image_no_live_is_photo() {
        let asset = TestPhotoAsset::new("TEST_1").build(); // public.jpeg, no live versions
        assert_eq!(
            determine_media_type(VersionSizeKey::Original, &asset),
            MediaType::Photo
        );
    }

    #[test]
    fn test_determine_media_type_image_with_live_is_live_photo_image() {
        let asset = test_live_photo_asset(); // public.heic with live versions
        assert_eq!(
            determine_media_type(VersionSizeKey::Original, &asset),
            MediaType::LivePhotoImage
        );
    }

    #[test]
    fn test_determine_media_type_movie_original_is_video() {
        let asset = TestPhotoAsset::new("MOV_1")
            .filename("movie.mov")
            .item_type("com.apple.quicktime-movie")
            .orig_file_type("com.apple.quicktime-movie")
            .orig_size(50000)
            .orig_url("https://p01.icloud-content.com/vid")
            .orig_checksum("vid_ck")
            .build();
        assert_eq!(
            determine_media_type(VersionSizeKey::Original, &asset),
            MediaType::Video
        );
    }

    #[test]
    fn test_determine_media_type_live_original_on_image_is_live_photo_video() {
        let asset = test_live_photo_asset();
        assert_eq!(
            determine_media_type(VersionSizeKey::LiveOriginal, &asset),
            MediaType::LivePhotoVideo
        );
    }

    #[test]
    fn test_determine_media_type_live_original_on_movie_is_video() {
        let asset = TestPhotoAsset::new("MOV_2")
            .filename("movie.mov")
            .item_type("com.apple.quicktime-movie")
            .orig_file_type("com.apple.quicktime-movie")
            .orig_size(50000)
            .orig_url("https://p01.icloud-content.com/vid")
            .orig_checksum("vid_ck")
            .build();
        assert_eq!(
            determine_media_type(VersionSizeKey::LiveOriginal, &asset),
            MediaType::Video
        );
    }

    // ── NameId7 filter tests ────────────────────────────────────────────

    #[test]
    fn test_name_id7_produces_task_with_id_suffix() {
        let asset = TestPhotoAsset::new("TEST_1").build(); // recordName "TEST_1"
        let mut config = test_config();
        config.file_match_policy = FileMatchPolicy::NameId7;
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 1);
        let filename = tasks[0]
            .download_path
            .file_name()
            .unwrap()
            .to_str()
            .unwrap();
        // NameId7 uses underscore separator between stem and base64 ID suffix
        assert!(
            filename.contains('_'),
            "NameId7 filename should contain underscore separator, got: {filename}"
        );
    }

    #[test]
    fn test_name_id7_never_embeds_path_separator_in_filename() {
        // Regression: under STANDARD base64, an asset ID containing `?`
        // (0x3F) at position 2 produces `/` as the 4th base64 char,
        // which is a literal path separator. URL-safe base64 must
        // translate that to `_` instead.
        let asset = TestPhotoAsset::new("AB?xxxxx").build();
        let mut config = test_config();
        config.file_match_policy = FileMatchPolicy::NameId7;
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 1);
        let filename = tasks[0]
            .download_path
            .file_name()
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            !filename.contains('/'),
            "NameId7 filename leaked a path separator: {filename}"
        );
        assert!(
            !filename.contains('+'),
            "NameId7 filename leaked a `+` char (standard-base64 leak): {filename}"
        );
        // Confirm the `_` is actually in the suffix slot — proves the
        // URL-safe alphabet kicked in (STANDARD would have put `/`
        // there; `_` is the URL-safe replacement for `/`).
        assert!(
            filename.contains('_'),
            "expected URL-safe `_` in id7 suffix, got: {filename}"
        );
    }

    #[test]
    fn test_name_id7_skips_existing_file() {
        let asset = TestPhotoAsset::new("TEST_1").build();
        let mut config = test_config();
        config.file_match_policy = FileMatchPolicy::NameId7;
        let dir = TempDir::new().unwrap();
        config.directory = std::sync::Arc::from(dir.path());

        // First call to get the expected path
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 1);
        let expected_path = &tasks[0].download_path;

        // Create parent directories and write a file with the matching size
        fs::create_dir_all(expected_path.parent().unwrap()).unwrap();
        fs::write(expected_path, vec![0u8; 1000]).unwrap();

        // Second call should skip since the file exists with matching size
        let tasks2 = filter_asset_fresh(&asset, &config);
        assert!(
            tasks2.is_empty(),
            "NameId7 should skip existing file, got {} tasks",
            tasks2.len()
        );
    }

    #[test]
    fn test_name_id7_live_photo_produces_two_tasks_with_id_suffix() {
        let asset = test_live_photo_asset();
        let mut config = test_config();
        config.file_match_policy = FileMatchPolicy::NameId7;
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(
            tasks.len(),
            2,
            "Live photo should produce 2 tasks (HEIC + MOV)"
        );

        for task in &tasks {
            let filename = task.download_path.file_name().unwrap().to_str().unwrap();
            assert!(
                filename.contains('_'),
                "NameId7 live photo filename should contain underscore separator, got: {filename}"
            );
        }
    }

    // ── keep_unicode_in_filenames tests ─────────────────────────────────

    fn unicode_photo_asset() -> PhotoAsset {
        TestPhotoAsset::new("UNI_1")
            .filename("Caf\u{e9}_photo.jpg")
            .build()
    }

    #[test]
    fn test_keep_unicode_preserves_non_ascii() {
        let asset = unicode_photo_asset();
        let mut config = test_config();
        config.keep_unicode_in_filenames = true;
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 1);
        let filename = tasks[0]
            .download_path
            .file_name()
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            filename.contains("Caf\u{e9}"),
            "keep_unicode=true should preserve unicode, got: {filename}"
        );
    }

    #[test]
    fn test_default_strips_unicode_from_filename() {
        let asset = unicode_photo_asset();
        let config = test_config(); // keep_unicode_in_filenames = false
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 1);
        let filename = tasks[0]
            .download_path
            .file_name()
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            filename.contains("Caf_photo"),
            "keep_unicode=false should strip non-ASCII, got: {filename}"
        );
        assert!(
            !filename.contains("Caf\u{e9}"),
            "keep_unicode=false should not contain unicode chars, got: {filename}"
        );
    }

    // ── Medium/Thumb size suffix tests ──────────────────────────────────

    fn multi_size_photo_asset() -> PhotoAsset {
        PhotoAsset::new(
            json!({"recordName": "MED_1", "fields": {
                "filenameEnc": {"value": "photo.jpg", "type": "STRING"},
                "itemType": {"value": "public.jpeg"},
                "resOriginalRes": {"value": {
                    "size": 5000,
                    "downloadURL": "https://p01.icloud-content.com/orig",
                    "fileChecksum": "orig_ck"
                }},
                "resOriginalFileType": {"value": "public.jpeg"},
                "resJPEGMedRes": {"value": {
                    "size": 2000,
                    "downloadURL": "https://p01.icloud-content.com/med",
                    "fileChecksum": "med_ck"
                }},
                "resJPEGMedFileType": {"value": "public.jpeg"},
                "resJPEGThumbRes": {"value": {
                    "size": 500,
                    "downloadURL": "https://p01.icloud-content.com/thumb",
                    "fileChecksum": "thumb_ck"
                }},
                "resJPEGThumbFileType": {"value": "public.jpeg"}
            }}),
            json!({"fields": {"assetDate": {"value": 1736899200000.0}}}),
        )
    }

    #[test]
    fn test_medium_size_adds_suffix() {
        let asset = multi_size_photo_asset();
        let mut config = test_config();
        config.resolution = crate::types::PhotoResolution::Medium;
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 1);
        let filename = tasks[0]
            .download_path
            .file_name()
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            filename.contains("-medium"),
            "Medium size should add '-medium' suffix, got: {filename}"
        );
    }

    #[test]
    fn test_thumb_size_adds_suffix() {
        let asset = multi_size_photo_asset();
        let mut config = test_config();
        config.resolution = crate::types::PhotoResolution::Thumb;
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 1);
        let filename = tasks[0]
            .download_path
            .file_name()
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            filename.contains("-thumb"),
            "Thumb size should add '-thumb' suffix, got: {filename}"
        );
    }

    // ── NormalizedPath direct tests ─────────────────────────────────────

    #[test]
    fn test_normalized_path_lowercases_on_case_insensitive() {
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            let np = NormalizedPath::new(&PathBuf::from("Foo.JPG"));
            assert_eq!(&*np.0, "foo.jpg");
        }
    }

    #[test]
    fn test_normalized_path_case_equality() {
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            let a = NormalizedPath::new(&PathBuf::from("/photos/IMG.JPG"));
            let b = NormalizedPath::new(&PathBuf::from("/photos/img.jpg"));
            assert_eq!(a, b);
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            let a = NormalizedPath::new(&PathBuf::from("/photos/IMG.JPG"));
            let b = NormalizedPath::new(&PathBuf::from("/photos/img.jpg"));
            assert_ne!(a, b);
        }
    }

    #[test]
    fn test_normalized_path_borrow_for_hashmap_lookup() {
        use std::collections::HashMap;
        let mut map: HashMap<NormalizedPath, u64> = HashMap::new();
        map.insert(NormalizedPath::new(&PathBuf::from("test.jpg")), 42);
        let key = NormalizedPath::normalize(std::path::Path::new("test.jpg"));
        assert_eq!(map.get(key.as_ref()), Some(&42));
    }

    // ── NormalizedPath additional tests ──────────────────────────────────

    #[test]
    fn test_normalized_path_new_stores_normalized_form() {
        let np = NormalizedPath::new(&PathBuf::from("/photos/2025/01/IMG_0001.JPG"));
        // On macOS/Windows the stored form should be lowercase
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        assert_eq!(&*np.0, "/photos/2025/01/img_0001.jpg");
        // On Linux the stored form preserves case
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        assert_eq!(&*np.0, "/photos/2025/01/IMG_0001.JPG");
    }

    #[test]
    fn test_normalized_path_normalize_returns_lowercase_on_macos() {
        let path = Path::new("/Photos/IMG_0001.HEIC");
        let normalized = NormalizedPath::normalize(path);
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        assert_eq!(normalized.as_ref(), "/photos/img_0001.heic");
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        assert_eq!(normalized.as_ref(), "/Photos/IMG_0001.HEIC");
    }

    #[test]
    fn test_normalized_path_hashmap_case_insensitive_lookup() {
        // Insert with one case, look up with another — must find on macOS/Windows
        use std::collections::HashMap;
        let mut map: HashMap<NormalizedPath, u64> = HashMap::new();
        map.insert(NormalizedPath::new(&PathBuf::from("IMG_0001.JPG")), 100);
        let lookup_key = NormalizedPath::normalize(Path::new("img_0001.jpg"));
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        assert_eq!(map.get(lookup_key.as_ref()), Some(&100));
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        assert_eq!(map.get(lookup_key.as_ref()), None);
    }

    #[test]
    fn test_normalized_path_hash_consistency() {
        // NormalizedPath::new and normalize must produce the same hash for HashMap
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let path = PathBuf::from("Test/Photo.JPG");
        let np = NormalizedPath::new(&path);
        let normalized_str = NormalizedPath::normalize(&path);

        let mut h1 = DefaultHasher::new();
        np.hash(&mut h1);
        let hash1 = h1.finish();

        // The str from normalize should hash the same as the NormalizedPath via Borrow<str>
        let mut h2 = DefaultHasher::new();
        let borrow_str: &str = std::borrow::Borrow::borrow(&np);
        borrow_str.hash(&mut h2);
        let hash2 = h2.finish();

        assert_eq!(
            hash1, hash2,
            "NormalizedPath hash must match &str hash via Borrow"
        );
        assert_eq!(borrow_str, normalized_str.as_ref());
    }

    #[test]
    fn test_normalized_path_case_different_paths_equal_on_case_insensitive() {
        let upper = NormalizedPath::new(&PathBuf::from("PHOTO.HEIC"));
        let lower = NormalizedPath::new(&PathBuf::from("photo.heic"));
        let mixed = NormalizedPath::new(&PathBuf::from("Photo.Heic"));
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            assert_eq!(upper, lower);
            assert_eq!(upper, mixed);
            assert_eq!(lower, mixed);
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            assert_ne!(upper, lower);
            assert_ne!(upper, mixed);
        }
    }

    // ── Gap coverage: empty versions, path traversal, empty filename ───

    #[test]
    fn filter_asset_empty_versions_map_produces_no_tasks() {
        // Asset with no version fields at all — filter should produce zero tasks.
        let asset = PhotoAsset::new(
            json!({"recordName": "NO_VERS_1", "fields": {
                "filenameEnc": {"value": "IMG_4502.HEIC", "type": "STRING"},
                "itemType": {"value": "public.heic"}
            }}),
            json!({"fields": {"assetDate": {"value": 1736899200000.0}}}),
        );
        let config = test_config();
        let tasks = filter_asset_fresh(&asset, &config);
        assert!(
            tasks.is_empty(),
            "Asset with no versions should produce 0 tasks, got {}",
            tasks.len()
        );
    }

    #[test]
    fn filter_asset_path_traversal_filename_is_sanitized() {
        // A filename containing path traversal should NOT escape the download
        // directory. The folder_structure + local_download_path should confine it.
        let asset = TestPhotoAsset::new("TRAV_1")
            .filename("../../../etc/passwd")
            .orig_size(512)
            .orig_url("https://p01.icloud-content.com/photos/orig/abc")
            .orig_checksum("a1b2c3d4e5f6")
            .build();
        let config = test_config();
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 1);
        let path_str = tasks[0].download_path.to_string_lossy();
        // The download path must stay inside the configured directory
        assert!(
            path_str.starts_with(config.directory.to_string_lossy().as_ref()),
            "Path traversal filename should be confined to download dir, got: {path_str}"
        );
        assert!(
            !path_str.contains("/etc/passwd"),
            "Path traversal must not escape download directory, got: {path_str}"
        );
    }

    /// Two assets whose filenames differ only in case (`IMG_0001.JPG`
    /// vs `img_0001.jpg`) must NOT silently overwrite each other on a
    /// case-insensitive filesystem. The collision detector must either
    /// rename one with a disambiguation suffix or skip the duplicate; in
    /// no case may both produce identical claimed paths (which would
    /// cause one's bytes to clobber the other's at `rename` time —
    /// silent data loss).
    #[test]
    fn filter_case_only_filename_collision_yields_distinct_claimed_paths() {
        let dir = TempDir::new().unwrap();
        let mut config = test_config();
        config.directory = std::sync::Arc::from(dir.path());

        // Two assets, different IDs + different checksums (so they're
        // genuinely distinct content), but filenames that differ only in
        // case. On macOS / Windows these resolve to the same on-disk file.
        let asset_a = TestPhotoAsset::new("CASE_ONE")
            .filename("IMG_0001.JPG")
            .orig_size(2048)
            .orig_url("https://p01.icloud-content.com/photos/orig/a")
            .orig_checksum("aaaa1111")
            .build();

        let asset_b = TestPhotoAsset::new("CASE_TWO")
            .filename("img_0001.jpg")
            .orig_size(4096)
            .orig_url("https://p01.icloud-content.com/photos/orig/b")
            .orig_checksum("bbbb2222")
            .build();

        let mut claimed_paths = FxHashMap::default();
        let mut dir_cache = paths::DirCache::new();

        let tasks_a = filter_asset_to_tasks(&asset_a, &config, &mut claimed_paths, &mut dir_cache);
        assert_eq!(tasks_a.len(), 1, "first asset should resolve to one task");
        let path_a = tasks_a[0].download_path.clone();

        let tasks_b = filter_asset_to_tasks(&asset_b, &config, &mut claimed_paths, &mut dir_cache);
        assert_eq!(
            tasks_b.len(),
            1,
            "second asset should also resolve to one task"
        );
        let path_b = tasks_b[0].download_path.clone();

        // Critical invariant: the on-disk paths must NOT case-insensitively
        // match. NormalizedPath does the case-fold; pin its result here.
        let np_a = NormalizedPath::new(&path_a);
        let np_b = NormalizedPath::new(&path_b);
        assert_ne!(
            np_a,
            np_b,
            "case-only-collision filenames must produce case-folded-distinct \
             paths to avoid silent overwrite. Got A={} B={}",
            path_a.display(),
            path_b.display()
        );

        // And the raw paths must also differ — the disambiguation must
        // be present in at least the filename portion.
        assert_ne!(
            path_a,
            path_b,
            "case-only-collision filenames must produce literally-distinct \
             paths (got A=B={})",
            path_a.display()
        );

        // claimed_paths should now have both entries.
        assert_eq!(
            claimed_paths.len(),
            2,
            "claimed_paths must contain both case-distinct entries; got {}",
            claimed_paths.len()
        );
    }

    /// A path pre-seeded into claimed_paths must case-insensitively match an
    /// incoming asset's target and route it to a collision path. Same size is
    /// not proof that the claimed file is the same asset/version.
    #[test]
    fn filter_cross_batch_case_insensitive_same_size_collision_uses_identity_path() {
        let dir = TempDir::new().unwrap();
        let mut config = test_config();
        config.directory = std::sync::Arc::from(dir.path());

        let asset = TestPhotoAsset::new("CROSS_BATCH_1")
            .filename("IMG_0500.JPG")
            .orig_size(1000)
            .orig_url("https://p01.icloud-content.com/img")
            .orig_checksum("ck_cb")
            .build();

        let first_tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(first_tasks.len(), 1);
        let downloaded_path = first_tasks[0].download_path.clone();

        let mut claimed_paths = FxHashMap::default();
        claimed_paths.insert(NormalizedPath::new(&downloaded_path), 1000);

        let mut dir_cache = paths::DirCache::new();
        let second_tasks =
            filter_asset_to_tasks(&asset, &config, &mut claimed_paths, &mut dir_cache);
        assert_eq!(second_tasks.len(), 1);
        assert!(
            second_tasks[0]
                .download_path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.contains("CROSS_BATCH_1")),
            "same-size claimed-path collision must use an identity path: {second_tasks:?}"
        );
    }

    #[test]
    fn filter_asset_empty_filename_string_uses_fingerprint_fallback() {
        // Distinct from the missing-field case: the STRING field is PRESENT
        // but contains an empty string. A naive join would produce a path
        // like `"2026-04-19/"` (directory-only), so we must treat empty
        // exactly like missing and route through the fingerprint fallback.
        let asset = PhotoAsset::new(
            json!({"recordName": "EMPTYFN_ASSET1", "fields": {
                "filenameEnc": {"value": "", "type": "STRING"},
                "itemType": {"value": "public.jpeg"},
                "resOriginalRes": {"value": {
                    "size": 2048,
                    "downloadURL": "https://p01.icloud-content.com/photos/orig/emptyfn",
                    "fileChecksum": "deadbeef1234"
                }},
                "resOriginalFileType": {"value": "public.jpeg"}
            }}),
            json!({"fields": {"assetDate": {"value": 1736899200000.0}}}),
        );
        let config = test_config();
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 1);
        let filename = tasks[0]
            .download_path
            .file_name()
            .expect("download_path must include a filename, not bare directory")
            .to_str()
            .unwrap();
        assert!(
            !filename.is_empty() && !filename.starts_with('.'),
            "empty filenameEnc must produce a real filename via fingerprint fallback, \
             got: {filename}"
        );
        assert!(
            filename.ends_with(".JPG"),
            "fingerprint fallback for public.jpeg must yield .JPG, got: {filename}"
        );
    }

    #[test]
    fn filter_asset_unicode_stripped_empty_stem_uses_fingerprint_fallback() {
        let asset = TestPhotoAsset::new("UNI_FILTER")
            .filename("日本語.jpg")
            .build();
        let config = test_config();
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 1);
        let filename = tasks[0]
            .download_path
            .file_name()
            .expect("download_path must include a filename")
            .to_str()
            .unwrap();
        assert_eq!(
            filename,
            paths::generate_fingerprint_filename("UNI_FILTER", "public.jpeg"),
            "sync task path must match the fingerprint fallback used by import"
        );
    }

    #[test]
    fn filter_asset_missing_filename_uses_fingerprint_fallback() {
        // Asset whose filenameEnc field is absent (null) should trigger the
        // fingerprint fallback path, generating a filename from the asset ID.
        let asset = PhotoAsset::new(
            json!({"recordName": "NOFN_ASSET1", "fields": {
                "itemType": {"value": "public.jpeg"},
                "resOriginalRes": {"value": {
                    "size": 2048,
                    "downloadURL": "https://p01.icloud-content.com/photos/orig/nofn",
                    "fileChecksum": "deadbeef1234"
                }},
                "resOriginalFileType": {"value": "public.jpeg"}
            }}),
            json!({"fields": {"assetDate": {"value": 1736899200000.0}}}),
        );
        assert!(
            asset.filename().is_none(),
            "Asset with no filenameEnc should have None filename"
        );
        let config = test_config();
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 1);
        let filename = tasks[0]
            .download_path
            .file_name()
            .unwrap()
            .to_str()
            .unwrap();
        // Fingerprint path: SHA-256 hash of asset ID, first 12 hex chars
        // SHA-256("NOFN_ASSET1") → "aab85e8020e4..."
        assert!(
            filename.contains("aab85e8020e4"),
            "Missing filename should use fingerprint hash of asset ID, got: {filename}"
        );
        assert!(
            filename.ends_with(".JPG"),
            "Fingerprint filename for public.jpeg should have .JPG extension, got: {filename}"
        );
    }

    // ── Gap coverage: skip_created_before AND skip_created_after ────────

    #[test]
    fn filter_asset_narrowing_date_window_matrix() {
        struct DateWindowCase {
            name: &'static str,
            start: &'static str,
            end: &'static str,
            expected_filter: Option<FilterReason>,
        }

        let cases = [
            DateWindowCase {
                name: "inside window",
                start: "2025-01-01T00:00:00Z",
                end: "2025-02-01T00:00:00Z",
                expected_filter: None,
            },
            DateWindowCase {
                name: "before window",
                start: "2025-01-20T00:00:00Z",
                end: "2025-02-01T00:00:00Z",
                expected_filter: Some(FilterReason::DateRange),
            },
            DateWindowCase {
                name: "after window",
                start: "2024-12-01T00:00:00Z",
                end: "2025-01-10T00:00:00Z",
                expected_filter: Some(FilterReason::DateRange),
            },
        ];

        for case in cases {
            let asset = plain_photo_asset();
            let mut config = test_config();
            config.skip_created_before = Some(date_time(case.start));
            config.skip_created_after = Some(date_time(case.end));

            assert_eq!(
                is_asset_filtered(&asset, &config),
                case.expected_filter,
                "{}",
                case.name
            );
            if case.expected_filter.is_none() {
                assert_eq!(
                    filter_asset_fresh(&asset, &config).len(),
                    1,
                    "{} should produce a task",
                    case.name
                );
            }
        }
    }

    // ── Gap coverage: NameId7 produces task when file at original path ──

    #[test]
    fn filter_asset_name_id7_downloads_when_original_path_exists() {
        // With NameId7 policy, the download path includes an ID suffix.
        // Even if a file exists at the *non-suffixed* (original) path,
        // NameId7 should produce a task because its path is different.
        let dir = TempDir::new().unwrap();

        let asset = TestPhotoAsset::new("TEST_1").build(); // recordName "TEST_1", "photo.jpg"
        let mut config = test_config();
        config.directory = std::sync::Arc::from(dir.path());
        config.file_match_policy = FileMatchPolicy::NameId7;

        // Get the NameId7 path
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 1);
        let id7_path = &tasks[0].download_path;

        // Create a file at the non-suffixed original path (without ID suffix)
        // This simulates a file that was downloaded with NameSizeDedupWithSuffix
        let original_path = paths::local_download_path(
            &config.directory,
            &config.folder_structure,
            &tasks[0].created_local,
            "photo.JPG",
            config.album_name.as_deref(),
        );
        fs::create_dir_all(original_path.parent().unwrap()).unwrap();
        fs::write(&original_path, vec![0u8; 1000]).unwrap();

        // The NameId7 path is different from the original path
        assert_ne!(
            id7_path, &original_path,
            "NameId7 path should differ from non-suffixed path"
        );

        // NameId7 should still produce a task because the ID7 path doesn't exist
        let tasks2 = filter_asset_fresh(&asset, &config);
        assert_eq!(
            tasks2.len(),
            1,
            "NameId7 should produce task when only the non-suffixed file exists"
        );

        // Now create the file at the NameId7 path — should skip
        fs::create_dir_all(id7_path.parent().unwrap()).unwrap();
        fs::write(id7_path, vec![0u8; 1000]).unwrap();
        let tasks3 = filter_asset_fresh(&asset, &config);
        assert!(
            tasks3.is_empty(),
            "NameId7 should skip when ID-suffixed file already exists"
        );
    }

    // ── Gap coverage: retry_only known_ids filtering ────────────────────

    #[test]
    fn download_context_retry_only_skips_unknown_assets() {
        // In retry-only mode, the producer checks known_ids before sending
        // tasks. Simulate that filtering logic here.
        let mut ctx = super::super::DownloadContext::default();
        ctx.known_ids
            .entry("PrimarySync".into())
            .or_default()
            .insert("PREV_SYNCED_001".into());
        ctx.known_ids
            .entry("PrimarySync".into())
            .or_default()
            .insert("PREV_SYNCED_002".into());

        let known_asset = TestPhotoAsset::new("TEST_1").build(); // recordName "TEST_1"
        let config = test_config();
        let tasks = filter_asset_fresh(&known_asset, &config);

        // Simulate the retry_only check from the producer loop
        let retry_filtered: Vec<_> = tasks
            .into_iter()
            .filter(|task| ctx.is_known(&task.library, &task.asset_id))
            .collect();

        // "TEST_1" is NOT in known_ids, so retry_only would skip it
        assert!(
            retry_filtered.is_empty(),
            "Unknown asset should be filtered out in retry_only mode"
        );

        // Now add "TEST_1" to known_ids and verify it passes
        ctx.known_ids
            .entry("PrimarySync".into())
            .or_default()
            .insert("TEST_1".into());
        let tasks2 = filter_asset_fresh(&known_asset, &config);
        let retry_filtered2: Vec<_> = tasks2
            .into_iter()
            .filter(|task| ctx.is_known(&task.library, &task.asset_id))
            .collect();
        assert_eq!(
            retry_filtered2.len(),
            1,
            "Known asset should pass retry_only filter"
        );
    }

    // ── Gap coverage: incremental Modified events are downloadable ──────

    #[test]
    fn change_event_modified_asset_is_downloadable() {
        use crate::icloud::photos::asset::ChangeEvent;
        use crate::types::ChangeReason;

        // In the iCloud changes API, both new and modified records arrive as
        // ChangeReason::Created (the enum doc says "new or modified").
        // Verify that a "modified" asset with a ChangeReason::Created is
        // picked up by the download filter.
        let modified_asset = TestPhotoAsset::new("MODIFIED_ASSET_1")
            .filename("IMG_9876.HEIC")
            .item_type("public.heic")
            .orig_file_type("public.heic")
            .orig_size(4500000)
            .orig_url("https://p01.icloud-content.com/photos/orig/modified")
            .orig_checksum("f0e1d2c3b4a5")
            .build();

        let event = ChangeEvent {
            record_name: "MODIFIED_ASSET_1".into(),
            record_type: Some("CPLAsset".into()),
            master_record_name: None,
            reason: ChangeReason::Created,
            asset: Some(modified_asset),
            album: None,
            relation: None,
            token_unsafe_reason: None,
        };

        // Simulate the incremental filtering: Created reason + asset present
        assert!(matches!(event.reason, ChangeReason::Created));
        let asset = event.asset.unwrap();

        // The extracted asset should produce a download task
        let config = test_config();
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(
            tasks.len(),
            1,
            "Modified asset via Created reason should produce a download task"
        );
        assert_eq!(&*tasks[0].checksum, "f0e1d2c3b4a5");
    }

    // ── filter_asset_to_tasks edge-case tests ──────────────────────

    #[test]
    fn test_filter_asset_no_versions_produces_empty() {
        let asset = PhotoAsset::new(
            json!({"recordName": "NO_VERSIONS", "fields": {
                "filenameEnc": {"value": "empty.jpg", "type": "STRING"},
                "itemType": {"value": "public.jpeg"}
            }}),
            json!({"fields": {"assetDate": {"value": 1736899200000.0}}}),
        );
        let config = test_config();
        assert!(
            filter_asset_fresh(&asset, &config).is_empty(),
            "Asset with no versions should produce no tasks"
        );
    }

    struct FilterDecisionCase {
        name: &'static str,
        asset: fn() -> PhotoAsset,
        configure: fn(&mut DownloadConfig),
        expected: Option<FilterReason>,
    }

    #[test]
    fn is_asset_filtered_decision_matrix() {
        let cases = [
            FilterDecisionCase {
                name: "skip videos rejects movie assets",
                asset: video_asset,
                configure: skip_videos,
                expected: Some(FilterReason::MediaType),
            },
            FilterDecisionCase {
                name: "skip photos rejects still assets",
                asset: plain_photo_asset,
                configure: skip_photos,
                expected: Some(FilterReason::MediaType),
            },
            FilterDecisionCase {
                name: "skip live mode rejects live photos",
                asset: test_live_photo_asset,
                configure: skip_live_photos,
                expected: Some(FilterReason::LivePhoto),
            },
            FilterDecisionCase {
                name: "skip_created_before rejects older asset",
                asset: plain_photo_asset,
                configure: skip_before_february_2025,
                expected: Some(FilterReason::DateRange),
            },
            FilterDecisionCase {
                name: "skip_created_after rejects newer asset",
                asset: plain_photo_asset,
                configure: skip_after_january_2025,
                expected: Some(FilterReason::DateRange),
            },
            FilterDecisionCase {
                name: "skip_created_before rejects old historical asset",
                asset: old_asset,
                configure: skip_before_2024,
                expected: Some(FilterReason::DateRange),
            },
            FilterDecisionCase {
                name: "skip_created_after rejects future asset",
                asset: new_asset,
                configure: skip_after_2023,
                expected: Some(FilterReason::DateRange),
            },
            FilterDecisionCase {
                name: "filename exclude matches uppercase AAE",
                asset: aae_asset,
                configure: exclude_aae_filenames,
                expected: Some(FilterReason::Filename),
            },
            FilterDecisionCase {
                name: "filename exclude is case insensitive",
                asset: lowercase_aae_asset,
                configure: exclude_aae_filenames,
                expected: Some(FilterReason::Filename),
            },
            FilterDecisionCase {
                name: "filename exclude no-match passes",
                asset: keep_asset,
                configure: exclude_aae_filenames,
                expected: None,
            },
            FilterDecisionCase {
                name: "exclude asset ids blocks matching id",
                asset: excluded_asset,
                configure: exclude_known_asset_id,
                expected: Some(FilterReason::ExcludedAlbum),
            },
            FilterDecisionCase {
                name: "exclude asset ids passes non-matching id",
                asset: keep_asset,
                configure: exclude_other_asset_id,
                expected: None,
            },
        ];

        for case in cases {
            let asset = (case.asset)();
            let mut config = test_config();
            (case.configure)(&mut config);

            assert_eq!(
                is_asset_filtered(&asset, &config),
                case.expected,
                "{}",
                case.name
            );
            if case.expected.is_none() {
                assert!(
                    !filter_asset_fresh(&asset, &config).is_empty(),
                    "{} should reach task planning",
                    case.name
                );
            }
        }
    }

    #[test]
    fn test_filter_force_resolution_missing_version_no_fallback() {
        // Asset only has Original; request Medium with force_resolution=true
        let asset = TestPhotoAsset::new("FORCE_1").build();
        let mut config = test_config();
        config.resolution = crate::types::PhotoResolution::Medium;
        config.force_resolution = true;
        assert!(
            filter_asset_fresh(&asset, &config).is_empty(),
            "force_resolution=true with missing Medium version should not fall back to Original"
        );
    }

    // ── LivePhotoMode task shaping ─────────────────────────────────

    #[test]
    fn test_filter_video_only_mode_skips_primary_keeps_mov() {
        let asset = test_live_photo_asset();
        let mut config = test_config();
        config.live_photo_mode = LivePhotoMode::VideoOnly;
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 1);
        // The task should be the MOV companion
        assert!(tasks[0].download_path.to_str().unwrap().contains(".MOV"));
    }

    // ── Same filename, same date, same size collision ─────────────────

    #[test]
    fn filter_two_assets_same_path_same_size_second_uses_identity_path() {
        // Arrange: two assets with identical filename, date, and size but
        // different checksums (different photos that happen to share a name).
        let asset_a = TestPhotoAsset::new("ASSET_A")
            .filename("IMG_0001.JPG")
            .orig_size(5000)
            .orig_url("https://p01.icloud-content.com/a")
            .orig_checksum("ck_a")
            .build();
        let asset_b = TestPhotoAsset::new("ASSET_B")
            .filename("IMG_0001.JPG")
            .orig_size(5000)
            .orig_url("https://p01.icloud-content.com/b")
            .orig_checksum("ck_b")
            .build();

        let config = test_config();
        let mut claimed_paths = FxHashMap::default();
        let mut dir_cache = paths::DirCache::new();

        // Act
        let tasks_a = filter_asset_to_tasks(&asset_a, &config, &mut claimed_paths, &mut dir_cache);
        let tasks_b = filter_asset_to_tasks(&asset_b, &config, &mut claimed_paths, &mut dir_cache);

        // Assert: first asset gets the natural path, second gets an identity
        // collision path instead of being silently skipped.
        assert_eq!(tasks_a.len(), 1, "first asset should produce a task");
        assert_eq!(
            tasks_b.len(),
            1,
            "second asset with same path and same size must not be skipped"
        );
        assert!(
            tasks_b[0]
                .download_path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.contains("ASSET_B")),
            "second asset should use an identity collision path, got {:?}",
            tasks_b[0].download_path
        );
    }

    #[test]
    fn filter_two_assets_same_path_different_size_second_deduped() {
        // Arrange: two assets with identical filename and date but different sizes.
        // The second should get a dedup suffix, not be silently skipped.
        let asset_a = TestPhotoAsset::new("ASSET_A")
            .filename("IMG_0001.JPG")
            .orig_size(5000)
            .orig_url("https://p01.icloud-content.com/a")
            .orig_checksum("ck_a")
            .build();
        let asset_b = TestPhotoAsset::new("ASSET_B")
            .filename("IMG_0001.JPG")
            .orig_size(7000)
            .orig_url("https://p01.icloud-content.com/b")
            .orig_checksum("ck_b")
            .build();

        let config = test_config();
        let mut claimed_paths = FxHashMap::default();
        let mut dir_cache = paths::DirCache::new();

        // Act
        let tasks_a = filter_asset_to_tasks(&asset_a, &config, &mut claimed_paths, &mut dir_cache);
        let tasks_b = filter_asset_to_tasks(&asset_b, &config, &mut claimed_paths, &mut dir_cache);

        // Assert: both get tasks, second has dedup suffix
        assert_eq!(tasks_a.len(), 1);
        assert_eq!(tasks_b.len(), 1);
        let path_b = tasks_b[0].download_path.to_str().unwrap();
        assert!(
            path_b.contains("-7000."),
            "second asset should have size dedup suffix, got: {}",
            path_b,
        );
    }

    #[test]
    fn generated_collision_sets_are_deterministic_and_non_overwriting() {
        #[derive(Clone, Copy)]
        struct CollisionAsset {
            id: &'static str,
            filename: &'static str,
            size: u64,
            checksum: &'static str,
        }

        fn run_collision_set(root: &Path, assets: &[CollisionAsset]) -> Vec<Vec<PathBuf>> {
            let mut config = test_config();
            config.directory = Arc::from(root);
            let mut claimed_paths = FxHashMap::default();
            let mut dir_cache = paths::DirCache::new();

            assets
                .iter()
                .map(|case| {
                    let asset = TestPhotoAsset::new(case.id)
                        .filename(case.filename)
                        .orig_size(case.size)
                        .orig_url(&format!("https://p01.icloud-content.com/{}", case.id))
                        .orig_checksum(case.checksum)
                        .build();
                    filter_asset_to_tasks(&asset, &config, &mut claimed_paths, &mut dir_cache)
                        .into_iter()
                        .map(|task| task.download_path)
                        .collect()
                })
                .collect()
        }

        let assets = [
            CollisionAsset {
                id: "REP_A",
                filename: "repeat.JPG",
                size: 1000,
                checksum: "ck_rep_a",
            },
            CollisionAsset {
                id: "REP_B",
                filename: "repeat.JPG",
                size: 1000,
                checksum: "ck_rep_b",
            },
            CollisionAsset {
                id: "REP_C",
                filename: "repeat.JPG",
                size: 2000,
                checksum: "ck_rep_c",
            },
            CollisionAsset {
                id: "TRAV_A",
                filename: "../../etc/passwd.jpg",
                size: 3000,
                checksum: "ck_trav_a",
            },
            CollisionAsset {
                id: "TRAV_B",
                filename: "../../etc/passwd.jpg",
                size: 4000,
                checksum: "ck_trav_b",
            },
            CollisionAsset {
                id: "CASE_A",
                filename: "IMG_0001.JPG",
                size: 5000,
                checksum: "ck_case_a",
            },
            CollisionAsset {
                id: "CASE_B",
                filename: "img_0001.jpg",
                size: 6000,
                checksum: "ck_case_b",
            },
            CollisionAsset {
                id: "EMPTY_A",
                filename: "",
                size: 7000,
                checksum: "ck_empty_a",
            },
            CollisionAsset {
                id: "UNICODE_A",
                filename: "日本語.jpg",
                size: 8000,
                checksum: "ck_unicode_a",
            },
            CollisionAsset {
                id: "RESERVED_A",
                filename: "CON",
                size: 9000,
                checksum: "ck_reserved_a",
            },
        ];

        let dir = TempDir::new().unwrap();
        let first = run_collision_set(dir.path(), &assets);
        let second = run_collision_set(dir.path(), &assets);
        assert_eq!(first, second, "collision resolution must be deterministic");
        assert!(
            first[1][0]
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.contains("REP_B")),
            "same filename and same size should get an identity collision path: {:?}",
            first[1]
        );
        assert!(
            first[2][0]
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.contains("-2000.")),
            "different-size repeated filename should get a size dedup suffix: {:?}",
            first[2]
        );

        let mut exact_paths = FxHashSet::default();
        let mut normalized_paths = FxHashSet::default();
        for (asset, paths) in assets.iter().zip(first.iter()) {
            for path in paths {
                assert!(
                    path.starts_with(dir.path()),
                    "asset {} path escaped root: {}",
                    asset.id,
                    path.display()
                );
                let relative = path
                    .strip_prefix(dir.path())
                    .expect("path starts with root");
                assert!(
                    relative
                        .components()
                        .all(|component| matches!(component, std::path::Component::Normal(_))),
                    "asset {} path contains traversal or root components: {}",
                    asset.id,
                    path.display()
                );
                assert!(
                    exact_paths.insert(path.clone()),
                    "generated collision set produced duplicate path: {}",
                    path.display()
                );
                let normalized = NormalizedPath::new(path);
                assert!(
                    normalized_paths.insert(normalized),
                    "generated collision set produced a normalized duplicate path: {}",
                    path.display()
                );
            }
        }
    }

    // ── Gap: zero-size version triggers dedup, never matches ──────────

    #[test]
    fn filter_zero_size_version_never_matches_existing_file() {
        // When the API reports size=0, the SizeDedup policy with
        // skip_zero_size=true should treat it as "unknown" and never
        // match an existing file -- always produce a dedup path.
        let dir = TempDir::new().unwrap();

        let asset = TestPhotoAsset::new("ZERO_SIZE")
            .filename("IMG_0001.JPG")
            .orig_size(0) // size unknown/zero
            .orig_url("https://p01.icloud-content.com/zero")
            .orig_checksum("zero_ck")
            .build();

        let mut config = test_config();
        config.directory = std::sync::Arc::from(dir.path());

        // Create an existing file with some content (non-zero size)
        let tasks_first = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks_first.len(), 1);
        fs::create_dir_all(tasks_first[0].download_path.parent().unwrap()).unwrap();
        fs::write(&tasks_first[0].download_path, vec![0u8; 500]).unwrap();

        // Second call: zero-size should NOT match the 500-byte file,
        // should produce a dedup path instead of being silently skipped.
        let tasks_second = filter_asset_fresh(&asset, &config);
        assert_eq!(
            tasks_second.len(),
            1,
            "zero-size asset should produce a dedup task, not be skipped"
        );
        let path = tasks_second[0].download_path.to_str().unwrap();
        assert!(
            path.contains("-0."),
            "zero-size asset should have dedup suffix, got: {}",
            path,
        );
    }

    // ── Gap: NameId7 policy skips regardless of size ──────────────────

    #[test]
    fn filter_name_id7_skips_when_file_exists_regardless_of_size() {
        let dir = TempDir::new().unwrap();

        let asset = TestPhotoAsset::new("ASSET_X")
            .filename("IMG_0001.JPG")
            .orig_size(5000)
            .orig_url("https://p01.icloud-content.com/x")
            .orig_checksum("ck_x")
            .build();

        let mut config = test_config();
        config.directory = std::sync::Arc::from(dir.path());
        config.file_match_policy = FileMatchPolicy::NameId7;

        // First call: no file on disk
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 1);
        let path = &tasks[0].download_path;

        // Create the file with a DIFFERENT size (NameId7 doesn't check size)
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, vec![0u8; 1]).unwrap();

        // Second call: file exists, NameId7 should skip regardless of size
        let tasks = filter_asset_fresh(&asset, &config);
        assert!(
            tasks.is_empty(),
            "NameId7 should skip when file exists, regardless of size"
        );
    }

    // ── Post-rename pre-state-write idempotency ──────────────────
    //
    // After `rename_part_to_final` succeeds (file is on disk at the
    // final path) but before `state_db.mark_downloaded()` commits, a
    // SIGKILL leaves the file persisted with no asset row. The next
    // sync must classify this as "already downloaded" via the
    // filesystem check and skip — not re-download (bandwidth waste,
    // extra Apple API calls, possible duplicate-named files) and not
    // fail because the destination exists.
    //
    // The "filesystem check" is `resolve_download_path`'s on-disk
    // probe, exercised here by composing a fresh `DirCache` (the
    // post-restart state) plus the same asset config. Pre-existing
    // tests like `test_filter_skips_existing_file` cover this for the
    // happy path; this test names the crash-recovery scenario
    // explicitly so a regression that ties skip-decision to a DB row
    // (rather than the on-disk truth) lands red.
    #[test]
    fn pipeline_post_rename_pre_state_kill_recovers_idempotently() {
        let dir = TempDir::new().unwrap();

        let asset = TestPhotoAsset::new("ASSET_KILL")
            .filename("IMG_KILL.JPG")
            .orig_size(1000)
            .orig_url("https://p01.icloud-content.com/kill")
            .orig_checksum("ck_kill")
            .build();

        let mut config = test_config();
        config.directory = std::sync::Arc::from(dir.path());

        // Step 1: first filter pass yields one task at the canonical
        // download path. (This is what the pre-kill sync did before
        // crashing.)
        let tasks_pre = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks_pre.len(), 1);
        let final_path = tasks_pre[0].download_path.clone();

        // Step 2: simulate the post-rename state — the file lives on
        // disk at the final path with the right size. The state DB
        // does *not* know about it (we've thrown away the
        // claimed_paths map and DirCache, mirroring a fresh process
        // restart).
        fs::create_dir_all(final_path.parent().unwrap()).unwrap();
        fs::write(&final_path, vec![0u8; 1000]).unwrap();

        // Step 3: re-run filter against the same asset. The path layer must
        // not silently skip based on same name + same size; producer state
        // adoption owns the post-rename crash recovery path.
        let tasks_post = filter_asset_fresh(&asset, &config);
        assert_eq!(
            tasks_post.len(),
            1,
            "post-kill filter rerun should emit an identity collision task; \
             producer state adoption handles the pending DB row. final_path={final_path:?}, \
             tasks={tasks_post:?}",
        );
    }

    #[cfg(unix)]
    #[test]
    fn real_process_death_after_publish_before_mark_downloaded_recovers() {
        use std::os::unix::process::ExitStatusExt;
        use std::process::Command;

        let dir = TempDir::new().unwrap();
        let download_dir = dir.path().join("download");
        let db_path = dir.path().join("state.db");
        let part_path = dir.path().join(POST_RENAME_PRE_STATE_PART_FILE);
        let final_path = post_rename_pre_state_final_path_for(&download_dir);

        let output = Command::new(std::env::current_exe().unwrap())
            .arg("--ignored")
            .arg("--exact")
            .arg(POST_RENAME_PRE_STATE_CHILD_TEST)
            .arg("--nocapture")
            .env(POST_RENAME_PRE_STATE_CHILD_ENV, "1")
            .env(POST_RENAME_PRE_STATE_DOWNLOAD_DIR_ENV, &download_dir)
            .env(POST_RENAME_PRE_STATE_DB_ENV, &db_path)
            .env(POST_RENAME_PRE_STATE_PART_ENV, &part_path)
            .output()
            .expect("spawn crash harness child");

        assert_eq!(
            output.status.signal(),
            Some(libc::SIGKILL),
            "child should die at the post-publish/pre-state-write kill point; \
             status={:?}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            final_path.exists(),
            "final media file must survive the killed child"
        );
        assert!(
            !part_path.exists(),
            "published temp file should be gone after final-file publish"
        );

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let row = rt.block_on(async {
            let db = SqliteStateDb::open(&db_path).await.unwrap();
            db.get_pending()
                .await
                .unwrap()
                .into_iter()
                .find(|record| {
                    &*record.library == PRIMARY_ZONE_NAME
                        && &*record.id == POST_RENAME_PRE_STATE_ASSET_ID
                        && record.version_size.as_str() == "original"
                })
                .expect("post-kill row should remain pending")
        });
        assert_eq!(
            row.local_path, None,
            "SIGKILL before mark_downloaded must not fabricate downloaded state"
        );

        let asset = post_rename_pre_state_crash_asset();
        let config = post_rename_pre_state_config(&download_dir);
        let tasks_post = filter_asset_fresh(&asset, &config);
        assert_eq!(
            tasks_post.len(),
            1,
            "filter must not silently skip a same-size on-disk file; producer \
             state adoption handles the pending row. got tasks: {tasks_post:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "spawned by real_process_death_after_publish_before_mark_downloaded_recovers"]
    fn post_rename_pre_state_sigkill_child() {
        use std::path::PathBuf;

        if std::env::var_os(POST_RENAME_PRE_STATE_CHILD_ENV).is_none() {
            return;
        }

        let download_dir = PathBuf::from(
            std::env::var_os(POST_RENAME_PRE_STATE_DOWNLOAD_DIR_ENV).expect("download dir env"),
        );
        let db_path =
            PathBuf::from(std::env::var_os(POST_RENAME_PRE_STATE_DB_ENV).expect("db env"));
        let part_path =
            PathBuf::from(std::env::var_os(POST_RENAME_PRE_STATE_PART_ENV).expect("part env"));
        let final_path = post_rename_pre_state_final_path_for(&download_dir);

        if let Some(parent) = part_path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        if let Some(parent) = final_path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&part_path, vec![0x42; 1000]).unwrap();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let db = SqliteStateDb::open(&db_path).await.unwrap();
            let record = TestAssetRecord::new(POST_RENAME_PRE_STATE_ASSET_ID)
                .filename(POST_RENAME_PRE_STATE_FILENAME)
                .checksum(POST_RENAME_PRE_STATE_CHECKSUM)
                .size(1000)
                .build();
            db.upsert_seen(&record).await.unwrap();
            crate::download::file::rename_part_to_final(&part_path, &final_path)
                .await
                .unwrap();
        });

        // SAFETY: this child process is a dedicated crash harness. SIGKILL is
        // the test subject: the parent asserts the final file survived while
        // the DB row did not advance through mark_downloaded.
        unsafe {
            libc::raise(libc::SIGKILL);
        }
        std::process::exit(137);
    }

    // ── Gap: VideoOnly mode emits only MOV, no primary image ─────────

    #[test]
    fn filter_video_only_mode_emits_only_mov_companion() {
        let asset = test_live_photo_asset();
        let mut config = test_config();
        config.live_photo_mode = LivePhotoMode::VideoOnly;
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 1, "VideoOnly should emit exactly one task");
        assert!(
            tasks[0].download_path.to_str().unwrap().contains("MOV"),
            "VideoOnly task should be the MOV companion, got: {:?}",
            tasks[0].download_path,
        );
    }

    // ── Gap: exclude_asset_ids prevents download ─────────────────────

    #[test]
    fn filter_excluded_asset_id_is_filtered() {
        let asset = TestPhotoAsset::new("EXCLUDED_1").build();
        let mut config = test_config();
        let mut excluded = FxHashSet::default();
        excluded.insert("EXCLUDED_1".to_string());
        config.exclude_asset_ids = Arc::new(excluded);

        assert_eq!(
            is_asset_filtered(&asset, &config),
            Some(FilterReason::ExcludedAlbum),
            "asset in exclude_asset_ids should be filtered"
        );
    }

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
