//! Asset filtering -- determines which iCloud assets need downloading by
//! applying content/date/filename filters, resolving local paths, and
//! detecting collisions with existing files or in-flight downloads.

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Local};
use rustc_hash::FxHashMap;
use smallvec::SmallVec;

use crate::icloud::photos::types::AssetVersion;
use crate::icloud::photos::VersionsMap;
use crate::state::{MediaType, VersionSizeKey};
use crate::types::{
    AssetItemType, AssetVersionSize, FileMatchPolicy, LivePhotoMode, LivePhotoMovFilenamePolicy,
    RawTreatmentPolicy,
};

use super::paths;
use super::DownloadConfig;

/// Reason an asset was filtered out during content/metadata filtering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FilterReason {
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
    /// Create a new normalized path from an owned `PathBuf`.
    /// For lookup operations, prefer `normalize()` to avoid `PathBuf` cloning.
    pub(super) fn new(path: PathBuf) -> Self {
        Self(Self::normalize(&path).into_owned().into_boxed_str())
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
    /// `dc:subject` tags — provider keywords plus album memberships merge here.
    pub(super) keywords: Vec<String>,
    /// MWG-RS person names for `iptcExt:PersonInImage`.
    pub(super) people: Vec<String>,
    /// Hidden from the timeline at the source.
    pub(super) is_hidden: bool,
    /// Archived at the source.
    pub(super) is_archived: bool,
    /// Media subtype (panorama, screenshot, burst, slo_mo, …).
    pub(super) media_subtype: Option<String>,
    /// Opaque provider burst grouping id.
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

fn build_payload(
    asset: &crate::icloud::photos::PhotoAsset,
    config: &DownloadConfig,
) -> Arc<MetadataPayload> {
    let albums = config
        .asset_groupings
        .albums
        .get(asset.id())
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let people = config
        .asset_groupings
        .people
        .get(asset.id())
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    Arc::new(MetadataPayload::from_metadata(asset.metadata()).with_asset_groupings(albums, people))
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
    pub(super) checksum: Box<str>,
    /// iCloud asset ID for state tracking. Shared with the producer's
    /// dedup set and any deferred state writes via refcount bump.
    pub(super) asset_id: Arc<str>,
    /// Metadata fields surfaced from `AssetMetadata` for writer consumption.
    /// Behind `Arc` so `task.metadata.clone()` in the download hot path is a
    /// refcount bump instead of a deep clone of every `Vec<String>` inside.
    #[cfg_attr(not(feature = "xmp"), allow(dead_code))]
    pub(super) metadata: Arc<MetadataPayload>,
    // 8-byte primitives
    pub(super) size: u64,
    // DateTime
    pub(super) created_local: DateTime<Local>,
    // 1-byte enum
    /// Version size key for state tracking.
    pub(super) version_size: VersionSizeKey,
}

/// Apply the RAW alignment policy by swapping Original and Alternative versions
/// when appropriate, matching Python's `apply_raw_policy()`.
#[allow(
    clippy::indexing_slicing,
    reason = "orig_idx / alt_idx come from `enumerate()` over `versions`; indexing back \
              into `versions` or its clone is in-bounds by construction"
)]
fn apply_raw_policy(versions: &VersionsMap, policy: RawTreatmentPolicy) -> Cow<'_, VersionsMap> {
    if policy == RawTreatmentPolicy::Unchanged {
        return Cow::Borrowed(versions);
    }

    // Find indices for Original and Alternative in a single pass
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
        return Cow::Borrowed(versions);
    };

    let should_swap = match policy {
        RawTreatmentPolicy::PreferOriginal => versions[alt_idx].1.asset_type.contains("raw"),
        RawTreatmentPolicy::PreferAlternative => {
            orig_idx.is_some_and(|idx| versions[idx].1.asset_type.contains("raw"))
        }
        RawTreatmentPolicy::Unchanged => false,
    };

    if !should_swap {
        return Cow::Borrowed(versions);
    }

    // Swap by cloning and modifying the keys
    let mut swapped = versions.clone();
    if let Some(orig_idx) = orig_idx {
        swapped[orig_idx].0 = AssetVersionSize::Alternative;
        swapped[alt_idx].0 = AssetVersionSize::Original;
    }
    Cow::Owned(swapped)
}

/// Returns the reason this asset should be skipped by content/metadata
/// filters, or `None` if the asset passes all filters.
///
/// Callers must invoke this before `extract_skip_candidates` or
/// `filter_asset_to_tasks` to avoid redundant evaluation.
pub(super) fn is_asset_filtered(
    asset: &crate::icloud::photos::PhotoAsset,
    config: &DownloadConfig,
) -> Option<FilterReason> {
    if config.exclude_asset_ids.contains(asset.id()) {
        tracing::debug!(asset_id = %asset.id(), "Skipping (excluded album asset)");
        return Some(FilterReason::ExcludedAlbum);
    }
    if config.skip_videos && asset.item_type() == Some(AssetItemType::Movie) {
        tracing::debug!(asset_id = %asset.id(), "Skipping video (skip_videos enabled)");
        return Some(FilterReason::MediaType);
    }
    if config.skip_photos && asset.item_type() == Some(AssetItemType::Image) {
        tracing::debug!(asset_id = %asset.id(), "Skipping photo (skip_photos enabled)");
        return Some(FilterReason::MediaType);
    }
    if config.live_photo_mode == LivePhotoMode::Skip && asset.is_live_photo() {
        tracing::debug!(asset_id = %asset.id(), "Skipping live photo (live_photo_mode=skip)");
        return Some(FilterReason::LivePhoto);
    }
    let created_utc = asset.created();
    if let Some(before) = &config.skip_created_before {
        if created_utc < *before {
            tracing::debug!(asset_id = %asset.id(), date = %created_utc, "Skipping (before date range)");
            return Some(FilterReason::DateRange);
        }
    }
    if let Some(after) = &config.skip_created_after {
        if created_utc > *after {
            tracing::debug!(asset_id = %asset.id(), date = %created_utc, "Skipping (after date range)");
            return Some(FilterReason::DateRange);
        }
    }
    // Only check filename exclusion when the asset has a real filename.
    // filter_asset_to_tasks separately handles fallback fingerprint filenames.
    if !config.filename_exclude.is_empty() {
        if let Some(filename) = asset.filename() {
            if config
                .filename_exclude
                .iter()
                .any(|p| p.matches_with(filename, GLOB_CASE_INSENSITIVE))
            {
                tracing::debug!(asset_id = %asset.id(), filename, "Skipping (filename_exclude match)");
                return Some(FilterReason::Filename);
            }
        }
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
    config: &DownloadConfig,
) -> SmallVec<[(VersionSizeKey, &'a str); 2]> {
    let is_live_photo = asset.is_live_photo();
    let versions = asset.versions();
    let mut result = SmallVec::new();

    // Primary version (with fallback to Original, same logic as filter_asset_to_tasks)
    // VideoOnly: skip primary image for live photos.
    let skip_primary = config.live_photo_mode == LivePhotoMode::VideoOnly && is_live_photo;
    let get_version = |key: &AssetVersionSize| -> Option<&AssetVersion> {
        versions.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    };
    if !skip_primary {
        let primary = version_with_fallback(
            &get_version,
            config.size,
            AssetVersionSize::Original,
            config.force_size,
        );
        if let Some((v, effective_size)) = primary {
            result.push((VersionSizeKey::from(effective_size), v.checksum.as_ref()));
        }
    }

    // Live photo companion (with fallback to LiveOriginal, mirrors primary logic)
    if matches!(
        config.live_photo_mode,
        LivePhotoMode::Both | LivePhotoMode::VideoOnly
    ) && asset.item_type() == Some(AssetItemType::Image)
    {
        let live = version_with_fallback(
            &get_version,
            config.live_photo_size,
            AssetVersionSize::LiveOriginal,
            config.force_size,
        );
        if let Some((v, effective_live_size)) = live {
            result.push((
                VersionSizeKey::from(effective_live_size),
                v.checksum.as_ref(),
            ));
        }
    }

    result
}

/// Look up a version by key, falling back to `fallback_key` when the requested
/// size is unavailable (unless `force_size` is set). Shared by both
/// `extract_skip_candidates` and `filter_asset_to_tasks`.
fn version_with_fallback<'a>(
    get_version: &dyn Fn(&AssetVersionSize) -> Option<&'a AssetVersion>,
    requested: AssetVersionSize,
    fallback: AssetVersionSize,
    force_size: bool,
) -> Option<(&'a AssetVersion, AssetVersionSize)> {
    match get_version(&requested) {
        Some(v) => Some((v, requested)),
        None if requested != fallback && !force_size => {
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
    let created_local: DateTime<Local> = asset.created().with_timezone(&Local);
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
    /// Compare sizes: same size = skip, different size = generate a dedup path.
    /// When `skip_zero_size` is true, a version with size 0 is treated as
    /// "size unknown" and never matches (always dedup).
    SizeDedup { skip_zero_size: bool },
    /// The file's identity is already encoded in the filename (name-id7).
    /// Any existing file at the path means "already downloaded" -- skip.
    SkipIfExists,
}

/// Shared context for `resolve_download_path` -- groups the mutable/config
/// references that every call needs so the function stays under clippy's
/// argument limit.
#[derive(Debug)]
struct ResolveContext<'a> {
    config: &'a DownloadConfig,
    created_local: &'a DateTime<Local>,
    claimed_paths: &'a FxHashMap<NormalizedPath, u64>,
    dir_cache: &'a mut paths::DirCache,
}

/// Resolve the final download path for a single version, handling on-disk
/// files, AM/PM whitespace variants, and in-flight claimed paths.
///
/// Returns `Some(path)` when the file should be downloaded, or `None` to skip.
///
/// `check_ampm`: when true, also checks AM/PM whitespace variants on disk
/// (relevant for primary photos whose timestamps contain AM/PM).
///
/// `make_dedup_filename`: called when a collision with a different-sized file
/// is detected. Returns the deduplicated filename to try.
fn resolve_download_path(
    download_path: &Path,
    version_size: u64,
    asset_id: &str,
    strategy: CollisionStrategy,
    ctx: &mut ResolveContext<'_>,
    check_ampm: bool,
    make_dedup_filename: impl FnOnce() -> String,
    label: &str,
) -> Option<PathBuf> {
    // Check for the file on disk. For primary photos, also check AM/PM
    // whitespace variants (e.g., "1.40.01 PM.PNG" vs "1.40.01\u{202F}PM.PNG").
    let on_disk_size = ctx.dir_cache.file_size(download_path).or_else(|| {
        if !check_ampm {
            return None;
        }
        let variant = ctx.dir_cache.find_ampm_variant(download_path)?;
        Some(ctx.dir_cache.file_size(&variant).unwrap_or(0))
    });

    // Determine whether the existing size (on disk or in-flight) is a match.
    // `source` is used only for log messages.
    let (existing_size, source) = if let Some(size) = on_disk_size {
        (Some(size), "on-disk")
    } else {
        let normalized = NormalizedPath::normalize(download_path);
        if let Some(&size) = ctx.claimed_paths.get(normalized.as_ref()) {
            (Some(size), "in-flight")
        } else {
            (None, "")
        }
    };

    let Some(existing_size) = existing_size else {
        // Path is unclaimed -- use it directly.
        return Some(download_path.to_path_buf());
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
            None
        }
        CollisionStrategy::SizeDedup { skip_zero_size } => {
            let sizes_match =
                (!skip_zero_size || version_size > 0) && existing_size == version_size;

            if sizes_match {
                if source == "on-disk" {
                    tracing::info!(
                        asset_id,
                        path = %download_path.display(),
                        size = version_size,
                        "Skipping {label}: file exists with same name and size"
                    );
                } else {
                    tracing::info!(
                        asset_id,
                        path = %download_path.display(),
                        size = version_size,
                        "Skipping {label}: {source} download has same name and size"
                    );
                }
                return None;
            }

            // Different size -- deduplicate.
            let dedup_filename = make_dedup_filename();
            let dedup_path = paths::local_download_path(
                &ctx.config.directory,
                &ctx.config.folder_structure,
                ctx.created_local,
                &dedup_filename,
                ctx.config.album_name.as_deref(),
            );
            let dedup_key = NormalizedPath::normalize(&dedup_path);
            if ctx.dir_cache.exists(&dedup_path)
                || ctx.claimed_paths.contains_key(dedup_key.as_ref())
            {
                if source == "on-disk" {
                    tracing::info!(
                        asset_id,
                        path = %dedup_path.display(),
                        "Skipping {label}: dedup path already exists"
                    );
                } else {
                    tracing::info!(
                        asset_id,
                        path = %dedup_path.display(),
                        "Skipping {label}: dedup path already claimed in-flight"
                    );
                }
                None
            } else {
                if source == "on-disk" {
                    tracing::debug!(
                        path = %download_path.display(),
                        on_disk_size = existing_size,
                        expected_size = version_size,
                        dedup_path = %dedup_path.display(),
                        "{label} collision: already exists with different size"
                    );
                } else {
                    tracing::debug!(
                        path = %download_path.display(),
                        claimed_size = existing_size,
                        expected_size = version_size,
                        dedup_path = %dedup_path.display(),
                        "{label} {source} collision: claimed with different size"
                    );
                }
                Some(dedup_path)
            }
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
) -> SmallVec<[DownloadTask; 2]> {
    let is_live_photo = asset.is_live_photo();

    let fallback_filename;
    let raw_filename = if let Some(f) = asset.filename() {
        f
    } else {
        // Generate fallback from asset ID fingerprint, matching Python behavior.
        let asset_type = asset
            .versions()
            .first()
            .map_or("", |(_, v)| v.asset_type.as_ref());
        fallback_filename = paths::generate_fingerprint_filename(asset.id(), asset_type);
        tracing::info!(
            asset_id = %asset.id(),
            filename = %fallback_filename,
            "Using fingerprint fallback filename"
        );
        // is_asset_filtered only checks real filenames; check fallback against
        // exclusion patterns here so fingerprint names are also filtered.
        if config
            .filename_exclude
            .iter()
            .any(|p| p.matches_with(&fallback_filename, GLOB_CASE_INSENSITIVE))
        {
            tracing::debug!(
                asset_id = %asset.id(),
                filename = %fallback_filename,
                "Skipping (filename_exclude match on fallback)"
            );
            return SmallVec::new();
        }
        &fallback_filename
    };

    // Strip non-ASCII characters unless --keep-unicode-in-filenames is set.
    // Matches Python's default behavior of calling remove_unicode_chars() on filenames.
    let base_filename: String = if config.keep_unicode_in_filenames {
        raw_filename.to_string()
    } else {
        paths::remove_unicode_chars(raw_filename).into_owned()
    };

    let created_local: DateTime<Local> = asset.created().with_timezone(&Local);
    let versions = apply_raw_policy(asset.versions(), config.align_raw);
    let mut tasks = SmallVec::new();
    // Live-photo assets emit two DownloadTasks (primary + MOV companion)
    // that share the same metadata; build the payload once and Arc::clone
    // it onto each task.
    let payload = build_payload(asset, config);
    // Track the effective primary filename (including any dedup suffix) so the
    // live photo MOV companion is derived from the same name, keeping them paired.
    let mut effective_primary_filename: Option<String> = None;

    // Helper closure to find a version by key in the SmallVec
    let get_version = |key: &AssetVersionSize| -> Option<&AssetVersion> {
        versions.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    };

    // Select requested version, falling back to Original when the requested size is
    // unavailable (unless --force-size is set). Matches Python's behavior.
    // Track the effective size so we only add "-medium"/"-thumb" suffix when
    // the asset actually has that version (not on fallback to Original).
    let (version, effective_size) = match version_with_fallback(
        &get_version,
        config.size,
        AssetVersionSize::Original,
        config.force_size,
    ) {
        Some((v, s)) => (Some(v), s),
        None => (None, config.size),
    };
    // VideoOnly mode: skip the primary image for live photos, only emit MOV.
    let skip_primary = config.live_photo_mode == LivePhotoMode::VideoOnly && is_live_photo;

    if let Some(version) = version.filter(|_| !skip_primary) {
        // Map the file extension based on the version's UTI asset_type
        let mapped_filename = paths::map_filename_extension(&base_filename, &version.asset_type);

        // Add size suffix for non-Original sizes (e.g., "-medium", "-thumb").
        // Only when actually using that size, not on fallback to Original.
        // Matches Python's VERSION_FILENAME_SUFFIX_LOOKUP.
        let sized_filename = match effective_size {
            AssetVersionSize::Medium => paths::insert_suffix(&mapped_filename, "medium"),
            AssetVersionSize::Thumb => paths::insert_suffix(&mapped_filename, "thumb"),
            _ => mapped_filename,
        };

        // Apply name-id7 policy: bake asset ID suffix into ALL filenames upfront
        let filename = match config.file_match_policy {
            FileMatchPolicy::NameId7 => paths::apply_name_id7(&sized_filename, asset.id()),
            FileMatchPolicy::NameSizeDedupWithSuffix => sized_filename,
        };

        let download_path = paths::local_download_path(
            &config.directory,
            &config.folder_structure,
            &created_local,
            &filename,
            config.album_name.as_deref(),
        );

        let strategy = match config.file_match_policy {
            FileMatchPolicy::NameId7 => CollisionStrategy::SkipIfExists,
            FileMatchPolicy::NameSizeDedupWithSuffix => CollisionStrategy::SizeDedup {
                skip_zero_size: true,
            },
        };

        let final_path = {
            let mut ctx = ResolveContext {
                config,
                created_local: &created_local,
                claimed_paths,
                dir_cache,
            };
            resolve_download_path(
                &download_path,
                version.size,
                asset.id(),
                strategy,
                &mut ctx,
                true, // check AM/PM variants for primary photos
                || paths::add_dedup_suffix(&filename, version.size),
                "asset",
            )
        };

        if let Some(path) = &final_path {
            // Record the effective filename used for the primary download so the
            // MOV companion is derived from it, keeping HEIC/MOV paired after dedup.
            if let Some(stem) = path.file_name().and_then(|f| f.to_str()) {
                effective_primary_filename = Some(stem.to_string());
            }
        }
        if let Some(path) = final_path {
            claimed_paths.insert(NormalizedPath::new(path.clone()), version.size);
            tasks.push(DownloadTask {
                url: version.url.clone(),
                download_path: path,
                checksum: version.checksum.clone(),
                asset_id: asset.id_arc(),
                metadata: Arc::clone(&payload),
                size: version.size,
                created_local,
                version_size: VersionSizeKey::from(effective_size),
            });
        }
    }

    // Live photo MOV companion -- only for images.
    // Falls back from LiveAdjusted -> LiveOriginal when adjusted isn't available
    // (mirrors the primary version fallback logic), unless --force-size is set.
    if matches!(
        config.live_photo_mode,
        LivePhotoMode::Both | LivePhotoMode::VideoOnly
    ) && asset.item_type() == Some(AssetItemType::Image)
    {
        let (live_version_opt, effective_live_size) = match version_with_fallback(
            &get_version,
            config.live_photo_size,
            AssetVersionSize::LiveOriginal,
            config.force_size,
        ) {
            Some((v, s)) => (Some(v), s),
            None => (None, config.live_photo_size),
        };
        if let Some(live_version) = live_version_opt {
            // Derive the MOV filename from the effective primary filename (which
            // includes any dedup suffix) so the HEIC and MOV remain visually paired.
            // Fall back to the base filename when no primary was produced (e.g. skipped).
            let live_base = match config.file_match_policy {
                FileMatchPolicy::NameId7 => paths::apply_name_id7(&base_filename, asset.id()),
                FileMatchPolicy::NameSizeDedupWithSuffix => effective_primary_filename
                    .as_deref()
                    .unwrap_or(&base_filename)
                    .to_string(),
            };
            let mov_filename = match config.live_photo_mov_filename_policy {
                LivePhotoMovFilenamePolicy::Suffix => paths::live_photo_mov_path_suffix(&live_base),
                LivePhotoMovFilenamePolicy::Original => {
                    paths::live_photo_mov_path_original(&live_base)
                }
            };
            let mov_path = paths::local_download_path(
                &config.directory,
                &config.folder_structure,
                &created_local,
                &mov_filename,
                config.album_name.as_deref(),
            );

            let asset_id = asset.id();
            let final_mov_path = {
                let mut ctx = ResolveContext {
                    config,
                    created_local: &created_local,
                    claimed_paths,
                    dir_cache,
                };
                resolve_download_path(
                    &mov_path,
                    live_version.size,
                    asset_id,
                    CollisionStrategy::SizeDedup {
                        skip_zero_size: false,
                    },
                    &mut ctx,
                    false, // no AM/PM variants for MOV companions
                    || paths::insert_suffix(&mov_filename, asset_id),
                    "live photo MOV",
                )
            };

            if let Some(path) = final_mov_path {
                claimed_paths.insert(NormalizedPath::new(path.clone()), live_version.size);
                tasks.push(DownloadTask {
                    url: live_version.url.clone(),
                    download_path: path,
                    checksum: live_version.checksum.clone(),
                    asset_id: asset.id_arc(),
                    metadata: Arc::clone(&payload),
                    size: live_version.size,
                    created_local,
                    version_size: VersionSizeKey::from(effective_live_size),
                });
            }
        }
    }

    tasks
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use rustc_hash::FxHashSet;

    use crate::icloud::photos::PhotoAsset;
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
    fn filter_asset_fresh(
        asset: &PhotoAsset,
        config: &DownloadConfig,
    ) -> SmallVec<[DownloadTask; 2]> {
        let mut claimed_paths = FxHashMap::default();
        let mut dir_cache = paths::DirCache::new();
        filter_asset_to_tasks(asset, config, &mut claimed_paths, &mut dir_cache)
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
        config.skip_videos = true;
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
        config.skip_photos = true;
        assert_eq!(
            is_asset_filtered(&asset, &config),
            Some(FilterReason::MediaType)
        );
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
    fn test_filter_skips_existing_file() {
        let dir = TempDir::new().unwrap();
        let asset = TestPhotoAsset::new("TEST_1").build();
        let mut config = test_config();
        config.directory = std::sync::Arc::from(dir.path());

        // First call should produce a task (file doesn't exist yet)
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 1);

        // Create the file with matching size (1000 bytes), second call should skip
        fs::create_dir_all(tasks[0].download_path.parent().unwrap()).unwrap();
        fs::write(&tasks[0].download_path, vec![0u8; 1000]).unwrap();
        assert!(filter_asset_fresh(&asset, &config).is_empty());
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
        assert!(tasks[1]
            .download_path
            .to_str()
            .unwrap()
            .contains("IMG_0001_HEVC.MOV"));
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
        assert!(tasks[1]
            .download_path
            .to_str()
            .unwrap()
            .contains("IMG_0001.MOV"));
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

        // Second call: only the photo task (MOV already exists with matching size)
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 1);
        assert_eq!(&*tasks[0].url, "https://p01.icloud-content.com/heic_orig");
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
        config.live_photo_size = AssetVersionSize::LiveMedium;
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

    /// Helper to get a version from a SmallVec by key
    fn get_ver(versions: &VersionsMap, key: AssetVersionSize) -> Option<&AssetVersion> {
        versions.iter().find(|(k, _)| *k == key).map(|(_, v)| v)
    }

    /// Helper to check if a version exists in a SmallVec
    fn has_ver(versions: &VersionsMap, key: AssetVersionSize) -> bool {
        versions.iter().any(|(k, _)| *k == key)
    }

    #[test]
    fn test_raw_policy_as_is_no_swap() {
        let asset = photo_asset_with_original_and_alternative("public.jpeg", "com.adobe.raw-image");
        let versions = apply_raw_policy(asset.versions(), RawTreatmentPolicy::Unchanged);
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
    fn test_raw_policy_as_original_swaps_when_alt_is_raw() {
        let asset = photo_asset_with_original_and_alternative("public.jpeg", "com.adobe.raw-image");
        let versions = apply_raw_policy(asset.versions(), RawTreatmentPolicy::PreferOriginal);
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
    fn test_raw_policy_as_alternative_swaps_when_orig_is_raw() {
        let asset = photo_asset_with_original_and_alternative("com.adobe.raw-image", "public.jpeg");
        let versions = apply_raw_policy(asset.versions(), RawTreatmentPolicy::PreferAlternative);
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
    fn test_raw_policy_as_original_no_swap_when_alt_not_raw() {
        let asset = photo_asset_with_original_and_alternative("public.jpeg", "public.jpeg");
        let versions = apply_raw_policy(asset.versions(), RawTreatmentPolicy::PreferOriginal);
        assert_eq!(
            &*get_ver(&versions, AssetVersionSize::Original).unwrap().url,
            "https://p01.icloud-content.com/orig"
        );
    }

    #[test]
    fn test_raw_policy_as_alternative_no_swap_when_orig_not_raw() {
        let asset = photo_asset_with_original_and_alternative("public.jpeg", "public.jpeg");
        let versions = apply_raw_policy(asset.versions(), RawTreatmentPolicy::PreferAlternative);
        assert_eq!(
            &*get_ver(&versions, AssetVersionSize::Original).unwrap().url,
            "https://p01.icloud-content.com/orig"
        );
    }

    #[test]
    fn test_raw_policy_no_alternative_no_swap() {
        let asset = TestPhotoAsset::new("TEST_1").build(); // only has Original
        let versions = apply_raw_policy(asset.versions(), RawTreatmentPolicy::PreferOriginal);
        assert_eq!(
            &*get_ver(&versions, AssetVersionSize::Original).unwrap().url,
            "https://p01.icloud-content.com/orig"
        );
        assert!(!has_ver(&versions, AssetVersionSize::Alternative));
    }

    #[test]
    fn test_filter_asset_uses_raw_policy_swap() {
        let asset = photo_asset_with_original_and_alternative("public.jpeg", "com.adobe.raw-image");
        let mut config = test_config();
        config.align_raw = RawTreatmentPolicy::PreferOriginal;
        // With AsOriginal and RAW alternative, the swap makes Original point to alt URL
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 1);
        assert_eq!(&*tasks[0].url, "https://p01.icloud-content.com/alt");
        assert_eq!(&*tasks[0].checksum, "alt_ck");
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
        let config = test_config(); // align_raw defaults to AsIs
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

    #[test]
    fn test_extract_skip_candidates_photo() {
        let asset = TestPhotoAsset::new("TEST_1").build();
        let config = test_config();
        let candidates = extract_skip_candidates(&asset, &config);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].0, VersionSizeKey::Original);
        assert_eq!(candidates[0].1, "abc123");
    }

    #[test]
    fn test_extract_skip_candidates_live_photo() {
        let asset = test_live_photo_asset();
        let config = test_config();
        let candidates = extract_skip_candidates(&asset, &config);
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].0, VersionSizeKey::Original);
        assert_eq!(candidates[0].1, "heic_ck");
        assert_eq!(candidates[1].0, VersionSizeKey::LiveOriginal);
        assert_eq!(candidates[1].1, "mov_ck");
    }

    #[test]
    fn test_extract_skip_candidates_skip_videos() {
        let asset = TestPhotoAsset::new("VID_1")
            .filename("movie.mov")
            .item_type("com.apple.quicktime-movie")
            .orig_file_type("com.apple.quicktime-movie")
            .orig_size(50000)
            .orig_url("https://p01.icloud-content.com/vid")
            .orig_checksum("vid_ck")
            .build();
        let mut config = test_config();
        config.skip_videos = true;
        assert_eq!(
            is_asset_filtered(&asset, &config),
            Some(FilterReason::MediaType)
        );
    }

    #[test]
    fn test_extract_skip_candidates_skip_photos() {
        let asset = TestPhotoAsset::new("TEST_1").build();
        let mut config = test_config();
        config.skip_photos = true;
        assert_eq!(
            is_asset_filtered(&asset, &config),
            Some(FilterReason::MediaType)
        );
    }

    #[test]
    fn test_extract_skip_candidates_image_only_mode() {
        let asset = test_live_photo_asset();
        let mut config = test_config();
        config.live_photo_mode = LivePhotoMode::ImageOnly;
        let candidates = extract_skip_candidates(&asset, &config);
        // Should still have the primary HEIC version, just not the MOV companion
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].0, VersionSizeKey::Original);
    }

    #[test]
    fn test_extract_skip_candidates_skip_mode() {
        let asset = test_live_photo_asset();
        let mut config = test_config();
        config.live_photo_mode = LivePhotoMode::Skip;
        assert_eq!(
            is_asset_filtered(&asset, &config),
            Some(FilterReason::LivePhoto),
            "Skip mode should exclude live photos entirely"
        );
    }

    #[test]
    fn test_extract_skip_candidates_skip_mode_non_live_passes() {
        let asset = TestPhotoAsset::new("TEST_1").build();
        let mut config = test_config();
        config.live_photo_mode = LivePhotoMode::Skip;
        let candidates = extract_skip_candidates(&asset, &config);
        assert_eq!(
            candidates.len(),
            1,
            "Skip mode should not affect non-live photos"
        );
    }

    #[test]
    fn test_extract_skip_candidates_video_only_mode() {
        let asset = test_live_photo_asset();
        let mut config = test_config();
        config.live_photo_mode = LivePhotoMode::VideoOnly;
        let candidates = extract_skip_candidates(&asset, &config);
        // Should have only the MOV companion, no primary image
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].0, VersionSizeKey::LiveOriginal);
    }

    #[test]
    fn test_extract_skip_candidates_date_before_filter() {
        let asset = TestPhotoAsset::new("TEST_1").build(); // assetDate = 1736899200000 = 2025-01-15
        let mut config = test_config();
        // Set skip_created_before to a date AFTER the asset's creation
        config.skip_created_before = Some(
            DateTime::parse_from_rfc3339("2025-02-01T00:00:00Z")
                .unwrap()
                .into(),
        );
        assert_eq!(
            is_asset_filtered(&asset, &config),
            Some(FilterReason::DateRange)
        );
    }

    #[test]
    fn test_extract_skip_candidates_date_after_filter() {
        let asset = TestPhotoAsset::new("TEST_1").build(); // assetDate = 1736899200000 = 2025-01-15
        let mut config = test_config();
        // Set skip_created_after to a date BEFORE the asset's creation
        config.skip_created_after = Some(
            DateTime::parse_from_rfc3339("2025-01-01T00:00:00Z")
                .unwrap()
                .into(),
        );
        assert_eq!(
            is_asset_filtered(&asset, &config),
            Some(FilterReason::DateRange)
        );
    }

    #[test]
    fn test_extract_skip_candidates_size_fallback_to_original() {
        let asset = TestPhotoAsset::new("TEST_1").build(); // only has resOriginalRes
        let mut config = test_config();
        config.size = AssetVersionSize::Medium; // not available
        config.force_size = false;
        let candidates = extract_skip_candidates(&asset, &config);
        // Should fall back to Original
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].0, VersionSizeKey::Original);
    }

    #[test]
    fn test_extract_skip_candidates_force_size_no_fallback() {
        let asset = TestPhotoAsset::new("TEST_1").build(); // only has resOriginalRes
        let mut config = test_config();
        config.size = AssetVersionSize::Medium; // not available
        config.force_size = true;
        let candidates = extract_skip_candidates(&asset, &config);
        // force_size prevents fallback — no primary version
        assert!(candidates.is_empty());
    }

    #[test]
    fn test_extract_skip_candidates_live_adjusted_falls_back_to_live_original() {
        let asset = test_live_photo_asset(); // has LiveOriginal, no LiveAdjusted
        let mut config = test_config();
        config.live_photo_size = AssetVersionSize::LiveAdjusted;
        config.force_size = false;
        let candidates = extract_skip_candidates(&asset, &config);
        // Primary + live companion (fallback to LiveOriginal)
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[1].0, VersionSizeKey::LiveOriginal);
    }

    #[test]
    fn test_extract_skip_candidates_live_adjusted_force_size_no_fallback() {
        let asset = test_live_photo_asset(); // has LiveOriginal, no LiveAdjusted
        let mut config = test_config();
        config.live_photo_size = AssetVersionSize::LiveAdjusted;
        config.force_size = true;
        let candidates = extract_skip_candidates(&asset, &config);
        // force_size prevents fallback — only primary, no live companion
        assert_eq!(candidates.len(), 1);
    }

    #[test]
    fn test_filter_live_adjusted_falls_back_to_live_original() {
        let asset = test_live_photo_asset(); // has LiveOriginal, no LiveAdjusted
        let mut config = test_config();
        config.live_photo_size = AssetVersionSize::LiveAdjusted;
        config.force_size = false;
        let tasks = filter_asset_fresh(&asset, &config);
        // Should produce 2 tasks: primary + live companion (fallback to LiveOriginal)
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[1].version_size, VersionSizeKey::LiveOriginal);
        assert_eq!(&*tasks[1].url, "https://p01.icloud-content.com/live_mov");
    }

    #[test]
    fn test_filter_live_adjusted_force_size_no_fallback() {
        let asset = test_live_photo_asset(); // has LiveOriginal, no LiveAdjusted
        let mut config = test_config();
        config.live_photo_size = AssetVersionSize::LiveAdjusted;
        config.force_size = true;
        let tasks = filter_asset_fresh(&asset, &config);
        // force_size prevents fallback — only primary, no live companion
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
        config.size = AssetVersionSize::Medium;
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
        config.size = AssetVersionSize::Thumb;
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
            let np = NormalizedPath::new(PathBuf::from("Foo.JPG"));
            assert_eq!(&*np.0, "foo.jpg");
        }
    }

    #[test]
    fn test_normalized_path_case_equality() {
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            let a = NormalizedPath::new(PathBuf::from("/photos/IMG.JPG"));
            let b = NormalizedPath::new(PathBuf::from("/photos/img.jpg"));
            assert_eq!(a, b);
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            let a = NormalizedPath::new(PathBuf::from("/photos/IMG.JPG"));
            let b = NormalizedPath::new(PathBuf::from("/photos/img.jpg"));
            assert_ne!(a, b);
        }
    }

    #[test]
    fn test_normalized_path_borrow_for_hashmap_lookup() {
        use std::collections::HashMap;
        let mut map: HashMap<NormalizedPath, u64> = HashMap::new();
        map.insert(NormalizedPath::new(PathBuf::from("test.jpg")), 42);
        let key = NormalizedPath::normalize(std::path::Path::new("test.jpg"));
        assert_eq!(map.get(key.as_ref()), Some(&42));
    }

    // ── NormalizedPath additional tests ──────────────────────────────────

    #[test]
    fn test_normalized_path_new_stores_normalized_form() {
        let np = NormalizedPath::new(PathBuf::from("/photos/2025/01/IMG_0001.JPG"));
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
        map.insert(NormalizedPath::new(PathBuf::from("IMG_0001.JPG")), 100);
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
        let np = NormalizedPath::new(path.clone());
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
        let upper = NormalizedPath::new(PathBuf::from("PHOTO.HEIC"));
        let lower = NormalizedPath::new(PathBuf::from("photo.heic"));
        let mixed = NormalizedPath::new(PathBuf::from("Photo.Heic"));
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

    /// A path pre-seeded into claimed_paths (as a startup load from the
    /// state DB's downloaded rows would do) must case-insensitively match
    /// an incoming asset's target and dedupe it — otherwise cross-batch
    /// collisions silently overwrite prior downloads on case-insensitive
    /// filesystems.
    #[test]
    fn filter_cross_batch_case_insensitive_collision_is_deduped() {
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
        claimed_paths.insert(NormalizedPath::new(downloaded_path.clone()), 1000);

        let mut dir_cache = paths::DirCache::new();
        let second_tasks =
            filter_asset_to_tasks(&asset, &config, &mut claimed_paths, &mut dir_cache);
        assert!(
            second_tasks.is_empty(),
            "asset whose target path case-insensitively matches a claimed \
             path of the same size must be skipped; got tasks: {second_tasks:?}"
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
    fn filter_asset_narrowing_date_window_includes_asset_inside() {
        // Asset date: 2025-01-15 (epoch ms 1736899200000)
        let asset = TestPhotoAsset::new("TEST_1").build();
        let mut config = test_config();
        // Window: 2025-01-01 .. 2025-02-01 — asset at Jan 15 is inside
        config.skip_created_before = Some(
            DateTime::parse_from_rfc3339("2025-01-01T00:00:00Z")
                .unwrap()
                .into(),
        );
        config.skip_created_after = Some(
            DateTime::parse_from_rfc3339("2025-02-01T00:00:00Z")
                .unwrap()
                .into(),
        );
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(
            tasks.len(),
            1,
            "Asset inside the date window should produce a task"
        );
    }

    #[test]
    fn filter_asset_narrowing_date_window_excludes_asset_before() {
        // Asset date: 2025-01-15
        let asset = TestPhotoAsset::new("TEST_1").build();
        let mut config = test_config();
        // Window: 2025-01-20 .. 2025-02-01 — asset at Jan 15 is before the window
        config.skip_created_before = Some(
            DateTime::parse_from_rfc3339("2025-01-20T00:00:00Z")
                .unwrap()
                .into(),
        );
        config.skip_created_after = Some(
            DateTime::parse_from_rfc3339("2025-02-01T00:00:00Z")
                .unwrap()
                .into(),
        );
        assert_eq!(
            is_asset_filtered(&asset, &config),
            Some(FilterReason::DateRange),
            "Asset before the date window should be skipped"
        );
    }

    #[test]
    fn filter_asset_narrowing_date_window_excludes_asset_after() {
        // Asset date: 2025-01-15
        let asset = TestPhotoAsset::new("TEST_1").build();
        let mut config = test_config();
        // Window: 2024-12-01 .. 2025-01-10 — asset at Jan 15 is after the window
        config.skip_created_before = Some(
            DateTime::parse_from_rfc3339("2024-12-01T00:00:00Z")
                .unwrap()
                .into(),
        );
        config.skip_created_after = Some(
            DateTime::parse_from_rfc3339("2025-01-10T00:00:00Z")
                .unwrap()
                .into(),
        );
        assert_eq!(
            is_asset_filtered(&asset, &config),
            Some(FilterReason::DateRange),
            "Asset after the date window should be skipped"
        );
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
        ctx.known_ids.insert("PREV_SYNCED_001".into());
        ctx.known_ids.insert("PREV_SYNCED_002".into());

        let known_asset = TestPhotoAsset::new("TEST_1").build(); // recordName "TEST_1"
        let config = test_config();
        let tasks = filter_asset_fresh(&known_asset, &config);

        // Simulate the retry_only check from the producer loop
        let retry_filtered: Vec<_> = tasks
            .into_iter()
            .filter(|task| ctx.known_ids.contains(task.asset_id.as_ref()))
            .collect();

        // "TEST_1" is NOT in known_ids, so retry_only would skip it
        assert!(
            retry_filtered.is_empty(),
            "Unknown asset should be filtered out in retry_only mode"
        );

        // Now add "TEST_1" to known_ids and verify it passes
        ctx.known_ids.insert("TEST_1".into());
        let tasks2 = filter_asset_fresh(&known_asset, &config);
        let retry_filtered2: Vec<_> = tasks2
            .into_iter()
            .filter(|task| ctx.known_ids.contains(task.asset_id.as_ref()))
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
            reason: ChangeReason::Created,
            asset: Some(modified_asset),
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

    #[test]
    fn test_filter_skip_created_before_excludes_old_asset() {
        // Asset created 2020-06-15 (epoch ms)
        let asset = TestPhotoAsset::new("OLD_1")
            .asset_date(1592179200000.0) // 2020-06-15T00:00:00Z
            .build();
        let mut config = test_config();
        // skip_created_before = 2024-01-01
        config.skip_created_before = Some(
            chrono::NaiveDate::from_ymd_opt(2024, 1, 1)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap()
                .and_utc(),
        );
        assert_eq!(
            is_asset_filtered(&asset, &config),
            Some(FilterReason::DateRange),
            "Asset created in 2020 should be excluded by skip_created_before=2024"
        );
    }

    #[test]
    fn test_filter_skip_created_after_excludes_new_asset() {
        // Asset created 2025-06-15 (epoch ms)
        let asset = TestPhotoAsset::new("NEW_1")
            .asset_date(1750003200000.0) // 2025-06-15T00:00:00Z
            .build();
        let mut config = test_config();
        // skip_created_after = 2023-01-01
        config.skip_created_after = Some(
            chrono::NaiveDate::from_ymd_opt(2023, 1, 1)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap()
                .and_utc(),
        );
        assert_eq!(
            is_asset_filtered(&asset, &config),
            Some(FilterReason::DateRange),
            "Asset created in 2025 should be excluded by skip_created_after=2023"
        );
    }

    #[test]
    fn test_filter_force_size_missing_version_no_fallback() {
        // Asset only has Original; request Medium with force_size=true
        let asset = TestPhotoAsset::new("FORCE_1").build();
        let mut config = test_config();
        config.size = AssetVersionSize::Medium;
        config.force_size = true;
        assert!(
            filter_asset_fresh(&asset, &config).is_empty(),
            "force_size=true with missing Medium version should not fall back to Original"
        );
    }

    // ── LivePhotoMode + filename_exclude filter tests ─────────────

    #[test]
    fn test_filter_skip_mode_skips_live_photo_entirely() {
        let asset = test_live_photo_asset();
        let mut config = test_config();
        config.live_photo_mode = LivePhotoMode::Skip;
        assert_eq!(
            is_asset_filtered(&asset, &config),
            Some(FilterReason::LivePhoto),
            "Skip mode should produce no tasks for live photos"
        );
    }

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

    #[test]
    fn test_filter_filename_exclude_matches() {
        let asset = TestPhotoAsset::new("EXCL_1")
            .filename("IMG_0001.AAE")
            .build();
        let mut config = test_config();
        config.filename_exclude = std::sync::Arc::from(vec![glob::Pattern::new("*.AAE").unwrap()]);
        assert_eq!(
            is_asset_filtered(&asset, &config),
            Some(FilterReason::Filename),
            "*.AAE pattern should exclude AAE files"
        );
    }

    #[test]
    fn test_filter_filename_exclude_case_insensitive() {
        let asset = TestPhotoAsset::new("EXCL_2").filename("Photo.aae").build();
        let mut config = test_config();
        config.filename_exclude = std::sync::Arc::from(vec![glob::Pattern::new("*.AAE").unwrap()]);
        assert_eq!(
            is_asset_filtered(&asset, &config),
            Some(FilterReason::Filename),
            "Pattern matching should be case-insensitive"
        );
    }

    #[test]
    fn test_filter_filename_exclude_no_match_passes() {
        let asset = TestPhotoAsset::new("EXCL_3")
            .filename("IMG_0001.JPG")
            .build();
        let mut config = test_config();
        config.filename_exclude = std::sync::Arc::from(vec![glob::Pattern::new("*.AAE").unwrap()]);
        let tasks = filter_asset_fresh(&asset, &config);
        assert!(!tasks.is_empty(), "Non-matching files should pass through");
    }

    // ── exclude_asset_ids filter tests ─────────────────────────────

    #[test]
    fn test_filter_exclude_asset_ids_blocks_matching() {
        let asset = TestPhotoAsset::new("EXCLUDED_1")
            .filename("IMG_0001.JPG")
            .build();
        let mut config = test_config();
        let mut ids = FxHashSet::default();
        ids.insert("EXCLUDED_1".to_string());
        config.exclude_asset_ids = Arc::new(ids);
        assert_eq!(
            is_asset_filtered(&asset, &config),
            Some(FilterReason::ExcludedAlbum),
            "Asset in exclude set should be filtered"
        );
    }

    #[test]
    fn test_filter_exclude_asset_ids_passes_non_matching() {
        let asset = TestPhotoAsset::new("KEEP_1")
            .filename("IMG_0002.JPG")
            .build();
        let mut config = test_config();
        let mut ids = FxHashSet::default();
        ids.insert("OTHER_ID".to_string());
        config.exclude_asset_ids = Arc::new(ids);
        let tasks = filter_asset_fresh(&asset, &config);
        assert!(!tasks.is_empty(), "Asset not in exclude set should pass");
    }

    #[test]
    fn test_skip_candidates_exclude_asset_ids() {
        let asset = TestPhotoAsset::new("SKIP_EXCL_1")
            .filename("IMG_0001.JPG")
            .build();
        let mut config = test_config();
        let mut ids = FxHashSet::default();
        ids.insert("SKIP_EXCL_1".to_string());
        config.exclude_asset_ids = Arc::new(ids);
        assert_eq!(
            is_asset_filtered(&asset, &config),
            Some(FilterReason::ExcludedAlbum),
            "is_asset_filtered should return true for excluded assets"
        );
    }

    // ── extract_skip_candidates: filename_exclude ─────────────────

    #[test]
    fn test_extract_skip_candidates_filename_exclude_matches() {
        let asset = TestPhotoAsset::new("TEST_1").filename("photo.AAE").build();
        let mut config = test_config();
        config.filename_exclude = std::sync::Arc::from(vec![glob::Pattern::new("*.AAE").unwrap()]);
        assert_eq!(
            is_asset_filtered(&asset, &config),
            Some(FilterReason::Filename),
            "filename_exclude should filter via is_asset_filtered"
        );
    }

    #[test]
    fn test_extract_skip_candidates_filename_exclude_no_match_passes() {
        let asset = TestPhotoAsset::new("TEST_1").build(); // filename = "test_photo.jpg"
        let mut config = test_config();
        config.filename_exclude = std::sync::Arc::from(vec![glob::Pattern::new("*.AAE").unwrap()]);
        assert!(
            !extract_skip_candidates(&asset, &config).is_empty(),
            "non-matching filename should pass through"
        );
    }

    #[test]
    fn test_extract_skip_candidates_filename_exclude_case_insensitive() {
        let asset = TestPhotoAsset::new("TEST_1").filename("photo.aae").build();
        let mut config = test_config();
        config.filename_exclude = std::sync::Arc::from(vec![glob::Pattern::new("*.AAE").unwrap()]);
        assert_eq!(
            is_asset_filtered(&asset, &config),
            Some(FilterReason::Filename),
            "filename_exclude should be case-insensitive"
        );
    }

    // ── Gap: two assets with same filename, same date, same size ──────
    //
    // When two distinct iCloud assets resolve to the same local path AND have
    // the same file size, the NameSizeDedupWithSuffix policy treats the second
    // as "already present" and silently skips it. This is by design -- but
    // there was no test verifying this exact scenario.

    #[test]
    fn filter_two_assets_same_path_same_size_second_skipped() {
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

        // Assert: first asset gets a task, second is skipped (same size = "match")
        assert_eq!(tasks_a.len(), 1, "first asset should produce a task");
        assert!(
            tasks_b.is_empty(),
            "second asset with same path and same size should be skipped, but got {} tasks",
            tasks_b.len()
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
