//! Download engine — streaming pipeline that starts downloading as soon as
//! the first API page returns, rather than enumerating the entire library
//! upfront. Uses a two-phase approach: (1) stream-and-download with bounded
//! concurrency, then (2) cleanup pass with fresh CDN URLs for any failures.

pub mod error;
pub mod exif;
pub mod file;
pub mod paths;

use std::fs::FileTimes;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use chrono::{DateTime, Local, Utc};
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::Client;
use rustc_hash::{FxHashMap, FxHashSet};
use smallvec::SmallVec;

use std::io::IsTerminal;
use std::path::PathBuf;

use futures_util::stream::{self, StreamExt};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;

use crate::icloud::photos::types::AssetVersion;
use crate::icloud::photos::{AssetItemType, AssetVersionSize, PhotoAlbum, VersionsMap};
use crate::retry::RetryConfig;
use crate::state::{AssetRecord, MediaType, StateDb, SyncRunStats, VersionSizeKey};
use crate::types::{FileMatchPolicy, LivePhotoMovFilenamePolicy, RawTreatmentPolicy};

use error::DownloadError;

/// Determine the media type for an asset based on version size and item type.
pub fn determine_media_type(
    version_size: VersionSizeKey,
    asset: &crate::icloud::photos::PhotoAsset,
) -> MediaType {
    match version_size {
        VersionSizeKey::LiveOriginal | VersionSizeKey::LiveMedium | VersionSizeKey::LiveThumb => {
            if asset.item_type() == Some(AssetItemType::Image) {
                MediaType::LivePhotoVideo
            } else {
                MediaType::Video
            }
        }
        _ => {
            if asset.item_type() == Some(AssetItemType::Movie) {
                MediaType::Video
            } else if asset.item_type() == Some(AssetItemType::Image) {
                // Could be live photo image or regular photo
                // Check if asset has live photo versions
                if asset.contains_version(&AssetVersionSize::LiveOriginal)
                    || asset.contains_version(&AssetVersionSize::LiveMedium)
                    || asset.contains_version(&AssetVersionSize::LiveThumb)
                {
                    MediaType::LivePhotoImage
                } else {
                    MediaType::Photo
                }
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
/// Use `NormalizedPath::normalize()` for temporary lookup keys to avoid PathBuf cloning.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct NormalizedPath(Box<str>);

impl NormalizedPath {
    /// Create a new normalized path from an owned PathBuf.
    /// For lookup operations, prefer `normalize()` to avoid PathBuf cloning.
    fn new(path: PathBuf) -> Self {
        Self(Self::normalize(&path).into_owned().into_boxed_str())
    }

    /// Normalize a path reference for map lookups.
    ///
    /// On case-insensitive systems (macOS, Windows), returns a lowercase copy.
    /// On case-sensitive systems (Linux), returns a borrowed view when possible.
    ///
    /// Use with `claimed_paths.contains_key(NormalizedPath::normalize(&path).as_ref())`
    /// to avoid allocating a PathBuf just for the lookup.
    fn normalize(path: &Path) -> std::borrow::Cow<'_, str> {
        let s = path.to_string_lossy();
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            std::borrow::Cow::Owned(s.to_ascii_lowercase())
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

/// Outcome of a download pass.
#[derive(Debug)]
pub enum DownloadOutcome {
    /// All downloads completed successfully.
    Success,
    /// Session expired mid-sync; caller should re-authenticate and retry.
    SessionExpired { auth_error_count: usize },
    /// Some downloads failed (not session-related).
    PartialFailure { failed_count: usize },
}

/// Subset of application config consumed by the download engine.
/// Decoupled from CLI parsing so the engine can be tested independently.
pub struct DownloadConfig {
    pub(crate) directory: std::path::PathBuf,
    pub(crate) folder_structure: String,
    pub(crate) size: AssetVersionSize,
    pub(crate) skip_videos: bool,
    pub(crate) skip_photos: bool,
    pub(crate) skip_created_before: Option<DateTime<Utc>>,
    pub(crate) skip_created_after: Option<DateTime<Utc>>,
    pub(crate) set_exif_datetime: bool,
    pub(crate) dry_run: bool,
    pub(crate) concurrent_downloads: usize,
    pub(crate) recent: Option<u32>,
    pub(crate) retry: RetryConfig,
    pub(crate) skip_live_photos: bool,
    pub(crate) live_photo_size: AssetVersionSize,
    pub(crate) live_photo_mov_filename_policy: LivePhotoMovFilenamePolicy,
    pub(crate) align_raw: RawTreatmentPolicy,
    pub(crate) no_progress_bar: bool,
    pub(crate) file_match_policy: FileMatchPolicy,
    pub(crate) force_size: bool,
    pub(crate) keep_unicode_in_filenames: bool,
    /// Temp file suffix for partial downloads (e.g. `.icloudpd-tmp`).
    pub(crate) temp_suffix: String,
    /// State database for tracking download progress.
    pub(crate) state_db: Option<Arc<dyn StateDb>>,
}

impl std::fmt::Debug for DownloadConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DownloadConfig")
            .field("directory", &self.directory)
            .field("folder_structure", &self.folder_structure)
            .field("size", &self.size)
            .field("skip_videos", &self.skip_videos)
            .field("skip_photos", &self.skip_photos)
            .field("skip_created_before", &self.skip_created_before)
            .field("skip_created_after", &self.skip_created_after)
            .field("set_exif_datetime", &self.set_exif_datetime)
            .field("dry_run", &self.dry_run)
            .field("concurrent_downloads", &self.concurrent_downloads)
            .field("recent", &self.recent)
            .field("retry", &self.retry)
            .field("skip_live_photos", &self.skip_live_photos)
            .field("live_photo_size", &self.live_photo_size)
            .field(
                "live_photo_mov_filename_policy",
                &self.live_photo_mov_filename_policy,
            )
            .field("align_raw", &self.align_raw)
            .field("no_progress_bar", &self.no_progress_bar)
            .field("file_match_policy", &self.file_match_policy)
            .field("force_size", &self.force_size)
            .field("keep_unicode_in_filenames", &self.keep_unicode_in_filenames)
            .field("temp_suffix", &self.temp_suffix)
            .field("state_db", &self.state_db.is_some())
            .finish()
    }
}

/// A unit of work produced by the filter phase and consumed by the download phase.
///
/// Fields ordered for optimal memory layout:
/// - Heap types first (`Box<str>`, PathBuf)
/// - 8-byte primitives (u64)
/// - DateTime (12-16 bytes)
/// - 1-byte enum last
#[derive(Debug, Clone)]
struct DownloadTask {
    // Heap types first
    url: Box<str>,
    download_path: PathBuf,
    checksum: Box<str>,
    /// iCloud asset ID for state tracking.
    asset_id: Box<str>,
    // 8-byte primitives
    size: u64,
    // DateTime
    created_local: DateTime<Local>,
    // 1-byte enum
    /// Version size key for state tracking.
    version_size: VersionSizeKey,
}

/// Pre-loaded download state for O(1) skip decisions.
///
/// Loaded once at sync start from the state database, this enables fast
/// in-memory lookups instead of per-asset DB queries. For 100K+ asset
/// libraries, this significantly reduces DB roundtrips.
///
/// Uses a two-level map structure (asset_id -> version_sizes) to enable
/// zero-allocation lookups via `&str` keys, avoiding the need to allocate
/// `(String, String)` tuples for each lookup.
#[derive(Debug, Default)]
struct DownloadContext {
    /// Nested map: asset_id -> set of version_sizes that are already downloaded.
    /// Two-level structure enables O(1) borrowed lookups without allocation.
    downloaded_ids: FxHashMap<Box<str>, FxHashSet<Box<str>>>,
    /// Nested map: asset_id -> (version_size -> checksum) for downloaded assets.
    /// Used to detect checksum changes (iCloud asset updated) without DB queries.
    downloaded_checksums: FxHashMap<Box<str>, FxHashMap<Box<str>, Box<str>>>,
}

impl DownloadContext {
    /// Load the download context from the state database.
    async fn load(db: &dyn StateDb) -> Self {
        // Build nested map structure for zero-allocation lookups
        let mut downloaded_ids: FxHashMap<Box<str>, FxHashSet<Box<str>>> = FxHashMap::default();
        for (asset_id, version_size) in db.get_downloaded_ids().await.unwrap_or_default() {
            downloaded_ids
                .entry(asset_id.into_boxed_str())
                .or_default()
                .insert(version_size.into_boxed_str());
        }

        let mut downloaded_checksums: FxHashMap<Box<str>, FxHashMap<Box<str>, Box<str>>> =
            FxHashMap::default();
        for ((asset_id, version_size), checksum) in
            db.get_downloaded_checksums().await.unwrap_or_default()
        {
            downloaded_checksums
                .entry(asset_id.into_boxed_str())
                .or_default()
                .insert(version_size.into_boxed_str(), checksum.into_boxed_str());
        }

        Self {
            downloaded_ids,
            downloaded_checksums,
        }
    }

    /// Check if an asset should be downloaded based on pre-loaded state.
    ///
    /// Returns `Some(true)` if definitely needs download, or `None` if
    /// downloaded with matching checksum (need filesystem check to confirm).
    ///
    /// Uses borrowed `&str` keys for zero-allocation lookups.
    fn should_download_fast(
        &self,
        asset_id: &str,
        version_size: VersionSizeKey,
        checksum: &str,
    ) -> Option<bool> {
        let version_size_str = version_size.as_str();

        // Two-level lookup with borrowed keys — no allocation
        let is_downloaded = self
            .downloaded_ids
            .get(asset_id)
            .is_some_and(|versions| versions.contains(version_size_str));

        if !is_downloaded {
            // Not in downloaded set — needs download
            return Some(true);
        }

        // Check if checksum changed (also zero-allocation lookup)
        if let Some(versions) = self.downloaded_checksums.get(asset_id) {
            if let Some(stored_checksum) = versions.get(version_size_str) {
                if stored_checksum.as_ref() != checksum {
                    // Checksum changed — needs re-download
                    return Some(true);
                }
            }
        }

        // Downloaded with matching checksum — but file might be missing,
        // need filesystem check (return None)
        None
    }
}

/// Eagerly enumerate all albums and build a complete task list.
///
/// Used only by the Phase 2 cleanup pass — re-contacts the API so each call
/// yields fresh CDN URLs that haven't expired during a long download session.
async fn build_download_tasks(
    albums: &[PhotoAlbum],
    config: &DownloadConfig,
    shutdown_token: CancellationToken,
) -> Result<Vec<DownloadTask>> {
    let album_results: Vec<Result<Vec<_>>> = stream::iter(albums)
        .take_while(|_| std::future::ready(!shutdown_token.is_cancelled()))
        .map(|album| async move { album.photos(config.recent).await })
        .buffer_unordered(config.concurrent_downloads)
        .collect()
        .await;

    let mut tasks: Vec<DownloadTask> = Vec::new();
    let mut claimed_paths: FxHashMap<NormalizedPath, u64> = FxHashMap::default();
    let mut dir_cache = std::collections::HashMap::new();
    for album_result in album_results {
        let assets = album_result?;

        for asset in &assets {
            tasks.extend(filter_asset_to_tasks(
                asset,
                config,
                &mut claimed_paths,
                &mut dir_cache,
            ));
        }
    }

    Ok(tasks)
}

/// Apply the RAW alignment policy by swapping Original and Alternative versions
/// when appropriate, matching Python's `apply_raw_policy()`.
fn apply_raw_policy(
    versions: &VersionsMap,
    policy: RawTreatmentPolicy,
) -> std::borrow::Cow<'_, VersionsMap> {
    if policy == RawTreatmentPolicy::Unchanged {
        return std::borrow::Cow::Borrowed(versions);
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

    let alt_idx = match alt_idx {
        Some(idx) => idx,
        None => return std::borrow::Cow::Borrowed(versions),
    };

    let should_swap = match policy {
        RawTreatmentPolicy::PreferOriginal => versions[alt_idx].1.asset_type.contains("raw"),
        RawTreatmentPolicy::PreferAlternative => orig_idx
            .map(|idx| versions[idx].1.asset_type.contains("raw"))
            .unwrap_or(false),
        RawTreatmentPolicy::Unchanged => false,
    };

    if !should_swap {
        return std::borrow::Cow::Borrowed(versions);
    }

    // Swap by cloning and modifying the keys
    let mut swapped = versions.clone();
    if let Some(orig_idx) = orig_idx {
        swapped[orig_idx].0 = AssetVersionSize::Alternative;
        swapped[alt_idx].0 = AssetVersionSize::Original;
    }
    std::borrow::Cow::Owned(swapped)
}

/// Apply content filters (type, date range) and local existence check,
/// producing download tasks for assets that need fetching.
/// Returns up to two tasks: the primary photo/video and an optional live photo MOV.
///
/// The `claimed_paths` map tracks paths that have been claimed by earlier tasks
/// in the same download session, preventing race conditions where two assets
/// with the same filename both see "file doesn't exist" during concurrent downloads.
fn filter_asset_to_tasks(
    asset: &crate::icloud::photos::PhotoAsset,
    config: &DownloadConfig,
    claimed_paths: &mut FxHashMap<NormalizedPath, u64>,
    dir_cache: &mut std::collections::HashMap<PathBuf, Vec<String>>,
) -> SmallVec<[DownloadTask; 2]> {
    if config.skip_videos && asset.item_type() == Some(AssetItemType::Movie) {
        return SmallVec::new();
    }
    if config.skip_photos && asset.item_type() == Some(AssetItemType::Image) {
        return SmallVec::new();
    }

    let created_utc = asset.created();
    if let Some(before) = &config.skip_created_before {
        if created_utc < *before {
            return SmallVec::new();
        }
    }
    if let Some(after) = &config.skip_created_after {
        if created_utc > *after {
            return SmallVec::new();
        }
    }

    let fallback_filename;
    let raw_filename = match asset.filename() {
        Some(f) => f,
        None => {
            // Generate fallback from asset ID fingerprint, matching Python behavior.
            let asset_type = asset
                .versions()
                .first()
                .map(|(_, v)| v.asset_type.as_ref())
                .unwrap_or("");
            fallback_filename = paths::generate_fingerprint_filename(asset.id(), asset_type);
            tracing::info!(
                asset_id = %asset.id(),
                filename = %fallback_filename,
                "Using fingerprint fallback filename"
            );
            &fallback_filename
        }
    };

    // Strip non-ASCII characters unless --keep-unicode-in-filenames is set.
    // Matches Python's default behavior of calling remove_unicode_chars() on filenames.
    let base_filename = if config.keep_unicode_in_filenames {
        raw_filename.to_string()
    } else {
        paths::remove_unicode_chars(raw_filename)
    };

    let created_local: DateTime<Local> = created_utc.with_timezone(&Local);
    let versions = apply_raw_policy(asset.versions(), config.align_raw);
    let mut tasks = SmallVec::new();
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
    let (version, effective_size) = match get_version(&config.size) {
        Some(v) => (Some(v), config.size),
        None if config.size != AssetVersionSize::Original && !config.force_size => {
            match get_version(&AssetVersionSize::Original) {
                Some(v) => (Some(v), AssetVersionSize::Original),
                None => (None, config.size),
            }
        }
        _ => (None, config.size),
    };
    if let Some(version) = version {
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
            _ => sized_filename,
        };

        let download_path = paths::local_download_path(
            &config.directory,
            &config.folder_structure,
            &created_local,
            &filename,
        );
        // Determine the final download path, applying size-based deduplication if needed.
        // Check both on-disk files AND in-flight downloads (claimed_paths) to handle
        // concurrent downloads of assets with the same filename.
        // Check for the file on disk, including AM/PM whitespace variants
        // (e.g., "1.40.01 PM.PNG" vs "1.40.01\u{202F}PM.PNG")
        let existing_path = if download_path.exists() {
            Some(download_path.clone())
        } else {
            paths::find_ampm_variant_cached(&download_path, dir_cache)
        };
        let final_path = if let Some(existing) = existing_path {
            match config.file_match_policy {
                FileMatchPolicy::NameSizeDedupWithSuffix => {
                    // If file exists with different size, download with size suffix
                    let on_disk_size = std::fs::metadata(&existing).map(|m| m.len()).unwrap_or(0);
                    if on_disk_size == version.size {
                        // Same size — likely already downloaded, skip.
                        None
                    } else {
                        // Different size — deduplicate by appending file size to filename.
                        let dedup_filename = paths::add_dedup_suffix(&filename, version.size);
                        let dedup_path = paths::local_download_path(
                            &config.directory,
                            &config.folder_structure,
                            &created_local,
                            &dedup_filename,
                        );
                        // Use normalize() for lookup to avoid PathBuf clone
                        let dedup_key = NormalizedPath::normalize(&dedup_path);
                        if dedup_path.exists() || claimed_paths.contains_key(dedup_key.as_ref()) {
                            None // deduped version already downloaded or claimed
                        } else {
                            tracing::debug!(
                                "File collision: {} already exists with different size (on-disk: {}, expected: {}), using {}",
                                download_path.display(),
                                on_disk_size,
                                version.size,
                                dedup_path.display(),
                            );
                            Some(dedup_path)
                        }
                    }
                }
                FileMatchPolicy::NameId7 => {
                    // name-id7 policy adds asset ID to ALL filenames, not just collisions.
                    // If the file exists, it's already downloaded, skip.
                    None
                }
            }
        } else if let Some(&claimed_size) =
            // Use normalize() for lookup to avoid PathBuf clone
            claimed_paths.get(NormalizedPath::normalize(&download_path).as_ref())
        {
            // Path is claimed by an in-flight download — check for size collision.
            // Use normalized paths for collision detection to handle case-insensitive
            // filesystems (macOS, Windows) where IMG.mov and IMG.MOV are the same file.
            match config.file_match_policy {
                FileMatchPolicy::NameSizeDedupWithSuffix => {
                    if claimed_size == version.size {
                        // Same size — likely duplicate asset, skip.
                        None
                    } else {
                        // Different size — deduplicate by appending file size to filename.
                        let dedup_filename = paths::add_dedup_suffix(&filename, version.size);
                        let dedup_path = paths::local_download_path(
                            &config.directory,
                            &config.folder_structure,
                            &created_local,
                            &dedup_filename,
                        );
                        // Use normalize() for lookup to avoid PathBuf clone
                        let dedup_key = NormalizedPath::normalize(&dedup_path);
                        if dedup_path.exists() || claimed_paths.contains_key(dedup_key.as_ref()) {
                            None // deduped version already downloaded or claimed
                        } else {
                            tracing::debug!(
                                "In-flight collision: {} claimed with different size (claimed: {}, expected: {}), using {}",
                                download_path.display(),
                                claimed_size,
                                version.size,
                                dedup_path.display(),
                            );
                            Some(dedup_path)
                        }
                    }
                }
                FileMatchPolicy::NameId7 => None,
            }
        } else {
            Some(download_path.clone())
        };

        if let Some(ref path) = final_path {
            // Record the effective filename used for the primary download so the
            // MOV companion is derived from it, keeping HEIC/MOV paired after dedup.
            if let Some(stem) = path.file_name().and_then(|f| f.to_str()) {
                effective_primary_filename = Some(stem.to_string());
            }
        }
        if let Some(path) = final_path {
            // Clone for the normalized key, move original into DownloadTask
            claimed_paths.insert(NormalizedPath::new(path.clone()), version.size);
            tasks.push(DownloadTask {
                url: version.url.to_string().into_boxed_str(),
                download_path: path,
                checksum: version.checksum.to_string().into_boxed_str(),
                asset_id: asset.id().to_string().into_boxed_str(),
                size: version.size,
                created_local,
                version_size: VersionSizeKey::from(config.size),
            });
        }
    }

    // Live photo MOV companion — only for images
    if !config.skip_live_photos && asset.item_type() == Some(AssetItemType::Image) {
        if let Some(live_version) = get_version(&config.live_photo_size) {
            // Derive the MOV filename from the effective primary filename (which
            // includes any dedup suffix) so the HEIC and MOV remain visually paired.
            // Fall back to the base filename when no primary was produced (e.g. skipped).
            let live_base = match config.file_match_policy {
                FileMatchPolicy::NameId7 => paths::apply_name_id7(&base_filename, asset.id()),
                _ => effective_primary_filename
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
            );
            // If the path already exists (on disk or claimed), it may be a different
            // file (e.g. a regular video) that collides with the live photo companion
            // name. Detect this by comparing sizes; on mismatch, deduplicate using
            // the asset ID.
            //
            // Use normalized paths for collision detection to handle case-insensitive
            // filesystems (macOS, Windows) where IMG.mov and IMG.MOV are the same file.
            let mov_key = NormalizedPath::normalize(&mov_path);
            let final_mov_path = if mov_path.exists() {
                let on_disk_size = std::fs::metadata(&mov_path).map(|m| m.len()).unwrap_or(0);
                if on_disk_size == live_version.size {
                    // Same size — likely already downloaded, skip.
                    None
                } else {
                    // Collision with a different file — deduplicate.
                    let dedup_filename = paths::insert_suffix(&mov_filename, asset.id());
                    let dedup_path = paths::local_download_path(
                        &config.directory,
                        &config.folder_structure,
                        &created_local,
                        &dedup_filename,
                    );
                    let dedup_key = NormalizedPath::normalize(&dedup_path);
                    if dedup_path.exists() || claimed_paths.contains_key(dedup_key.as_ref()) {
                        None // deduped version already downloaded or claimed
                    } else {
                        tracing::debug!(
                            "Live photo MOV collision: {} already exists with different size, using {}",
                            mov_path.display(),
                            dedup_path.display(),
                        );
                        Some(dedup_path)
                    }
                }
            } else if let Some(&claimed_size) = claimed_paths.get(mov_key.as_ref()) {
                // Path is claimed by an in-flight download
                if claimed_size == live_version.size {
                    None // Same size, likely duplicate
                } else {
                    // Collision with in-flight download — deduplicate.
                    let dedup_filename = paths::insert_suffix(&mov_filename, asset.id());
                    let dedup_path = paths::local_download_path(
                        &config.directory,
                        &config.folder_structure,
                        &created_local,
                        &dedup_filename,
                    );
                    let dedup_key = NormalizedPath::normalize(&dedup_path);
                    if dedup_path.exists() || claimed_paths.contains_key(dedup_key.as_ref()) {
                        None
                    } else {
                        tracing::debug!(
                            "Live photo MOV in-flight collision: {} claimed, using {}",
                            mov_path.display(),
                            dedup_path.display(),
                        );
                        Some(dedup_path)
                    }
                }
            } else {
                Some(mov_path)
            };
            if let Some(path) = final_mov_path {
                // Clone for the normalized key, move original into DownloadTask
                claimed_paths.insert(NormalizedPath::new(path.clone()), live_version.size);
                tasks.push(DownloadTask {
                    url: live_version.url.to_string().into_boxed_str(),
                    download_path: path,
                    checksum: live_version.checksum.to_string().into_boxed_str(),
                    asset_id: asset.id().to_string().into_boxed_str(),
                    size: live_version.size,
                    created_local,
                    version_size: VersionSizeKey::from(config.live_photo_size),
                });
            }
        }
    }

    tasks
}

/// Create a progress bar with a consistent template.
///
/// Returns `ProgressBar::hidden()` when the user passed `--no-progress-bar` or
/// stdout is not a TTY (e.g. piped output, cron jobs) — this prevents output
/// corruption and honours the user's preference.
fn create_progress_bar(no_progress_bar: bool, total: u64) -> ProgressBar {
    if no_progress_bar || !std::io::stdout().is_terminal() {
        return ProgressBar::hidden();
    }
    let pb = ProgressBar::new(total);
    // Template is a compile-time constant; unwrap_or_else handles the impossible case
    if let Ok(style) = ProgressStyle::with_template(
        "[{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({eta}) {msg}",
    ) {
        pb.set_style(style.progress_chars("=> "));
    }
    pb
}

/// Threshold of auth errors before aborting the download pass for re-authentication.
/// Counted cumulatively across both phases (streaming + cleanup).
const AUTH_ERROR_THRESHOLD: usize = 3;

/// Result of the streaming download phase.
#[derive(Debug)]
struct StreamingResult {
    downloaded: usize,
    exif_failures: usize,
    failed: Vec<DownloadTask>,
    auth_errors: usize,
}

/// Streaming download pipeline — merges per-album streams and pipes assets
/// directly into the download loop as they arrive from the API.
///
/// Eliminates the startup delay of full-library enumeration: the first
/// download begins as soon as the first API page returns. Each album's
/// background task prefetches the next page via a channel buffer, so API
/// latency overlaps with download I/O.
///
/// Returns `StreamingResult` containing download counts, failed tasks, and
/// auth error count. When auth errors exceed the threshold, the function
/// returns early to allow re-authentication.
async fn stream_and_download(
    download_client: &Client,
    albums: &[PhotoAlbum],
    config: &Arc<DownloadConfig>,
    shutdown_token: CancellationToken,
) -> Result<StreamingResult> {
    // Lightweight count-only API query (HyperionIndexCountLookup) — separate
    // from the page-by-page photo fetch, used to size the progress bar.
    // When --recent is set, cap to that limit since the stream will stop early.
    //
    // Note: the total reflects *photo count*, but each photo may produce
    // multiple download tasks (e.g. live photo MOV companions, RAW
    // alternates). The bar may therefore overshoot pos > len slightly.
    // This matches Python icloudpd's tqdm behavior and keeps the ETA useful.
    let mut total: u64 = 0;
    for album in albums {
        total += album.len().await.unwrap_or(0);
    }
    if let Some(recent) = config.recent {
        total = total.min(recent as u64);
    }
    let pb = create_progress_bar(config.no_progress_bar, total);

    // select_all interleaves across albums so no single large album
    // starves others; each stream's background task provides prefetch.
    let album_streams: Vec<_> = albums
        .iter()
        .map(|album| album.photo_stream(config.recent))
        .collect();

    let mut combined = stream::select_all(album_streams);

    // Track paths claimed by in-flight downloads to detect collisions between
    // assets with the same filename processed in the same session.
    let mut claimed_paths: FxHashMap<NormalizedPath, u64> = FxHashMap::default();

    if config.dry_run {
        let mut count = 0usize;
        let mut dir_cache = std::collections::HashMap::new();
        while let Some(result) = combined.next().await {
            if shutdown_token.is_cancelled() {
                tracing::info!("Shutdown requested, stopping dry run");
                break;
            }
            let asset = result?;
            let tasks = filter_asset_to_tasks(&asset, config, &mut claimed_paths, &mut dir_cache);
            for task in &tasks {
                tracing::info!("[DRY RUN] Would download {}", task.download_path.display());
            }
            count += tasks.len();
        }
        return Ok(StreamingResult {
            downloaded: count,
            exif_failures: 0,
            failed: Vec::new(),
            auth_errors: 0,
        });
    }

    let download_client = download_client.clone();
    let retry_config = config.retry;
    let set_exif = config.set_exif_datetime;
    let concurrency = config.concurrent_downloads;
    let state_db = config.state_db.clone();

    // Pre-load download context for O(1) skip decisions
    let download_ctx = if let Some(db) = &state_db {
        tracing::debug!("Pre-loading download state from database");
        DownloadContext::load(db.as_ref()).await
    } else {
        DownloadContext::default()
    };
    tracing::debug!(
        downloaded_ids = download_ctx.downloaded_ids.len(),
        "Download context loaded"
    );

    // Start sync run tracking
    let sync_run_id = if let Some(db) = &state_db {
        match db.start_sync_run().await {
            Ok(id) => {
                tracing::debug!(run_id = id, "Started sync run");
                Some(id)
            }
            Err(e) => {
                tracing::warn!("Failed to start sync run tracking: {}", e);
                None
            }
        }
    } else {
        None
    };

    let mut downloaded = 0usize;
    let mut exif_failures = 0usize;
    let mut failed: Vec<DownloadTask> = Vec::new();
    let mut auth_errors = 0usize;

    // Use a bounded channel to stream tasks from the producer to the download loop.
    // This allows downloads to start immediately as assets arrive from the API,
    // rather than waiting for all assets to be enumerated first.
    let (task_tx, task_rx) = mpsc::channel::<DownloadTask>(concurrency * 2);

    // Wrap counters in Arc for sharing with producer task
    let assets_seen = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let assets_seen_producer = Arc::clone(&assets_seen);

    // Spawn producer task that processes assets and sends download tasks
    let producer_config = Arc::clone(config);
    let producer_state_db = state_db.clone();
    let producer_shutdown = shutdown_token.clone();
    let producer_pb = pb.clone();
    let producer = tokio::spawn(async move {
        let config = &producer_config;
        let mut claimed_paths = claimed_paths;
        let mut dir_cache = std::collections::HashMap::new();
        while let Some(result) = combined.next().await {
            if producer_shutdown.is_cancelled() {
                break;
            }
            match result {
                Ok(asset) => {
                    assets_seen_producer.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let tasks =
                        filter_asset_to_tasks(&asset, config, &mut claimed_paths, &mut dir_cache);
                    if tasks.is_empty() {
                        producer_pb.inc(1);
                    } else {
                        for task in tasks {
                            // Record asset in state DB
                            if let Some(db) = &producer_state_db {
                                let media_type = determine_media_type(task.version_size, &asset);
                                let record = AssetRecord::new_pending(
                                    task.asset_id.to_string(),
                                    task.version_size,
                                    task.checksum.to_string(),
                                    task.download_path
                                        .file_name()
                                        .and_then(|f| f.to_str())
                                        .unwrap_or("")
                                        .to_string(),
                                    asset.created(),
                                    Some(asset.added_date()),
                                    task.size,
                                    media_type,
                                );
                                if let Err(e) = db.upsert_seen(&record).await {
                                    tracing::warn!(
                                        "Failed to record asset {}: {}",
                                        task.asset_id,
                                        e
                                    );
                                }

                                // Fast path: check pre-loaded state first
                                match download_ctx.should_download_fast(
                                    &task.asset_id,
                                    task.version_size,
                                    &task.checksum,
                                ) {
                                    Some(true) => {
                                        if task_tx.send(task).await.is_err() {
                                            return; // Receiver dropped
                                        }
                                    }
                                    Some(false) => {
                                        // Defensive: should_download_fast never returns
                                        // Some(false) today, but skip if it ever does.
                                        tracing::debug!(
                                            asset_id = %task.asset_id,
                                            "Skipping (state confirms no download needed)"
                                        );
                                        producer_pb.inc(1);
                                    }
                                    None => {
                                        // Downloaded with matching checksum — check file exists
                                        match tokio::fs::try_exists(&task.download_path).await {
                                            Ok(true) => {
                                                tracing::debug!(
                                                    asset_id = %task.asset_id,
                                                    path = %task.download_path.display(),
                                                    "Skipping (already downloaded)"
                                                );
                                                producer_pb.inc(1);
                                            }
                                            Ok(false) => {
                                                // Check for AM/PM whitespace variant on disk
                                                if paths::find_ampm_variant_cached(
                                                    &task.download_path,
                                                    &mut dir_cache,
                                                )
                                                .is_some()
                                                {
                                                    tracing::debug!(
                                                        asset_id = %task.asset_id,
                                                        path = %task.download_path.display(),
                                                        "Skipping (AM/PM variant exists on disk)"
                                                    );
                                                    producer_pb.inc(1);
                                                } else {
                                                    tracing::debug!(
                                                        asset_id = %task.asset_id,
                                                        path = %task.download_path.display(),
                                                        "File missing, will re-download"
                                                    );
                                                    if task_tx.send(task).await.is_err() {
                                                        return;
                                                    }
                                                }
                                            }
                                            Err(e) => {
                                                tracing::warn!(
                                                    "File existence check failed, downloading anyway: {}",
                                                    e
                                                );
                                                if task_tx.send(task).await.is_err() {
                                                    return;
                                                }
                                            }
                                        }
                                    }
                                }
                            } else {
                                // No state DB — just send for download
                                if task_tx.send(task).await.is_err() {
                                    return;
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    producer_pb.suspend(|| tracing::error!("Error fetching asset: {}", e));
                }
            }
        }
    });

    // Convert channel receiver to stream and feed into buffer_unordered
    let temp_suffix: Arc<str> = config.temp_suffix.clone().into();
    let download_stream = ReceiverStream::new(task_rx)
        .map(|task| {
            let client = download_client.clone();
            let temp_suffix = Arc::clone(&temp_suffix);
            async move {
                let result =
                    download_single_task(&client, &task, &retry_config, set_exif, &temp_suffix)
                        .await;
                (task, result)
            }
        })
        .buffer_unordered(concurrency);

    tokio::pin!(download_stream);

    // Batch DB writes for better throughput — flush every N completions
    const DB_BATCH_SIZE: usize = 50;
    let mut downloaded_batch: Vec<(String, String, PathBuf)> = Vec::with_capacity(DB_BATCH_SIZE);
    let mut failed_batch: Vec<(String, String, String)> = Vec::with_capacity(DB_BATCH_SIZE);

    while let Some((task, result)) = download_stream.next().await {
        if shutdown_token.is_cancelled() {
            pb.suspend(|| tracing::info!("Shutdown requested, stopping new downloads"));
            break;
        }
        let filename = task
            .download_path
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or("")
            .to_string();
        pb.set_message(filename);
        match result {
            Ok(exif_ok) => {
                downloaded += 1;
                if !exif_ok {
                    exif_failures += 1;
                }
                if state_db.is_some() {
                    downloaded_batch.push((
                        task.asset_id.to_string(),
                        task.version_size.as_str().to_string(),
                        task.download_path.clone(),
                    ));
                }
            }
            Err(e) => {
                // Check if this is a session expiry error
                if let Some(download_err) = e.downcast_ref::<DownloadError>() {
                    if download_err.is_session_expired() {
                        auth_errors += 1;
                        pb.suspend(|| {
                            tracing::warn!(
                                "Auth error ({}/{}): {} - {}",
                                auth_errors,
                                AUTH_ERROR_THRESHOLD,
                                task.download_path.display(),
                                e
                            );
                        });
                        if auth_errors >= AUTH_ERROR_THRESHOLD {
                            pb.suspend(|| {
                                tracing::warn!(
                                    "Auth error threshold reached, aborting for re-authentication"
                                );
                            });
                            // Use break instead of return to allow buffered tasks to
                            // complete cleanly, matching graceful shutdown behavior.
                            break;
                        }
                        if state_db.is_some() {
                            failed_batch.push((
                                task.asset_id.to_string(),
                                task.version_size.as_str().to_string(),
                                e.to_string(),
                            ));
                        }
                        failed.push(task);
                        pb.inc(1);
                        continue;
                    }
                }
                pb.suspend(|| {
                    tracing::error!("Download failed: {}: {}", task.download_path.display(), e);
                });
                if state_db.is_some() {
                    failed_batch.push((
                        task.asset_id.to_string(),
                        task.version_size.as_str().to_string(),
                        e.to_string(),
                    ));
                }
                failed.push(task);
            }
        }
        pb.inc(1);

        // Flush batches periodically
        if let Some(db) = &state_db {
            if downloaded_batch.len() >= DB_BATCH_SIZE {
                if let Err(e) = db.mark_downloaded_batch(&downloaded_batch).await {
                    tracing::warn!(
                        "Failed to batch mark {} downloads: {}",
                        downloaded_batch.len(),
                        e
                    );
                }
                downloaded_batch.clear();
            }
            if failed_batch.len() >= DB_BATCH_SIZE {
                if let Err(e) = db.mark_failed_batch(&failed_batch).await {
                    tracing::warn!(
                        "Failed to batch mark {} failures: {}",
                        failed_batch.len(),
                        e
                    );
                }
                failed_batch.clear();
            }
        }
    }

    // Wait for producer to finish (it may still be processing if we broke early)
    if let Err(e) = producer.await {
        if e.is_panic() {
            tracing::error!("Asset producer task panicked: {:?}", e);
        }
    }

    // Load the final count from the atomic counter
    let assets_seen_count = assets_seen.load(std::sync::atomic::Ordering::Relaxed);

    // Flush remaining batches
    if let Some(db) = &state_db {
        if !downloaded_batch.is_empty() {
            if let Err(e) = db.mark_downloaded_batch(&downloaded_batch).await {
                tracing::warn!(
                    "Failed to batch mark {} downloads: {}",
                    downloaded_batch.len(),
                    e
                );
            }
        }
        if !failed_batch.is_empty() {
            if let Err(e) = db.mark_failed_batch(&failed_batch).await {
                tracing::warn!(
                    "Failed to batch mark {} failures: {}",
                    failed_batch.len(),
                    e
                );
            }
        }
    }

    pb.finish_and_clear();

    // Complete sync run tracking
    if let (Some(db), Some(run_id)) = (&state_db, sync_run_id) {
        let stats = SyncRunStats {
            assets_seen: assets_seen_count,
            assets_downloaded: downloaded as u64,
            assets_failed: failed.len() as u64,
            interrupted: shutdown_token.is_cancelled() || auth_errors >= AUTH_ERROR_THRESHOLD,
        };
        if let Err(e) = db.complete_sync_run(run_id, &stats).await {
            tracing::warn!("Failed to complete sync run tracking: {}", e);
        } else {
            tracing::debug!(
                run_id,
                assets_seen = assets_seen_count,
                downloaded,
                failed = failed.len(),
                "Completed sync run"
            );
        }
    }

    Ok(StreamingResult {
        downloaded,
        exif_failures,
        failed,
        auth_errors,
    })
}

/// Entry point for the download engine.
///
/// Phase 1: Stream assets page-by-page and download immediately with bounded
/// concurrency — no upfront enumeration delay.
///
/// Phase 2 (cleanup): Re-fetch from the API to get fresh CDN URLs (the
/// originals may have expired during a long Phase 1) and retry failures at
/// reduced concurrency to give large files full bandwidth.
///
/// Returns `DownloadOutcome` indicating whether all downloads succeeded,
/// the session expired (requiring re-auth), or some downloads failed.
pub async fn download_photos(
    download_client: &Client,
    albums: &[PhotoAlbum],
    config: Arc<DownloadConfig>,
    shutdown_token: CancellationToken,
) -> Result<DownloadOutcome> {
    let started = Instant::now();

    let streaming_result =
        stream_and_download(download_client, albums, &config, shutdown_token.clone()).await?;

    let downloaded = streaming_result.downloaded;
    let mut exif_failures = streaming_result.exif_failures;
    let failed_tasks = streaming_result.failed;
    let auth_errors = streaming_result.auth_errors;

    // If auth errors exceeded threshold, return early for re-authentication
    if auth_errors >= AUTH_ERROR_THRESHOLD {
        return Ok(DownloadOutcome::SessionExpired {
            auth_error_count: auth_errors,
        });
    }

    if downloaded == 0 && failed_tasks.is_empty() {
        if config.dry_run {
            tracing::info!("── Dry Run Summary ──");
            tracing::info!("  0 files would be downloaded");
            tracing::info!("  destination: {}", config.directory.display());
        } else {
            tracing::info!("No new photos to download");
        }
        return Ok(DownloadOutcome::Success);
    }

    if config.dry_run {
        tracing::info!("── Dry Run Summary ──");
        if shutdown_token.is_cancelled() {
            tracing::info!(
                "  Interrupted — scanned {} files before shutdown",
                downloaded
            );
        } else {
            tracing::info!("  {} files would be downloaded", downloaded);
        }
        tracing::info!("  destination: {}", config.directory.display());
        tracing::info!("  concurrency: {}", config.concurrent_downloads);
        return Ok(DownloadOutcome::Success);
    }

    let total = downloaded + failed_tasks.len();

    if failed_tasks.is_empty() {
        tracing::info!("── Summary ──");
        if exif_failures > 0 {
            tracing::info!(
                "  {} downloaded ({} EXIF failures), 0 failed, {} total",
                total,
                exif_failures,
                total
            );
        } else {
            tracing::info!("  {} downloaded, 0 failed, {} total", total, total);
        }
        tracing::info!("  elapsed: {}", format_duration(started.elapsed()));
        return Ok(DownloadOutcome::Success);
    }

    // Phase 2: CDN URLs from Phase 1 may have expired during a long
    // download session. Re-fetch the full task list for fresh URLs and
    // retry with moderate parallelism (balance throughput vs. bandwidth per file).
    let cleanup_concurrency = 5;
    let failure_count = failed_tasks.len();
    tracing::info!(
        "── Cleanup pass: re-fetching URLs and retrying {} failed downloads (concurrency: {}) ──",
        failure_count,
        cleanup_concurrency,
    );

    let fresh_tasks = build_download_tasks(albums, &config, shutdown_token.clone()).await?;
    tracing::info!("  Re-fetched {} tasks with fresh URLs", fresh_tasks.len());

    let phase2_task_count = fresh_tasks.len();
    let pass_config = PassConfig {
        client: download_client,
        retry_config: &config.retry,
        set_exif: config.set_exif_datetime,
        concurrency: cleanup_concurrency,
        no_progress_bar: config.no_progress_bar,
        temp_suffix: config.temp_suffix.clone(),
        shutdown_token,
        state_db: config.state_db.clone(),
    };
    let pass_result = run_download_pass(pass_config, fresh_tasks).await;

    let remaining_failed = pass_result.failed;
    let phase2_auth_errors = pass_result.auth_errors;
    exif_failures += pass_result.exif_failures;
    let total_auth_errors = auth_errors + phase2_auth_errors;

    // If auth errors exceeded threshold during phase 2, return for re-auth
    if total_auth_errors >= AUTH_ERROR_THRESHOLD {
        return Ok(DownloadOutcome::SessionExpired {
            auth_error_count: total_auth_errors,
        });
    }

    let failed = remaining_failed.len();
    let phase2_succeeded = phase2_task_count - failed;
    let succeeded = downloaded + phase2_succeeded;
    let final_total = succeeded + failed;
    tracing::info!("── Summary ──");
    if exif_failures > 0 {
        tracing::info!(
            "  {} downloaded ({} EXIF failures), {} failed, {} total",
            succeeded,
            exif_failures,
            failed,
            final_total
        );
    } else {
        tracing::info!(
            "  {} downloaded, {} failed, {} total",
            succeeded,
            failed,
            final_total
        );
    }
    tracing::info!("  elapsed: {}", format_duration(started.elapsed()));

    if failed > 0 {
        for task in &remaining_failed {
            tracing::error!("Download failed: {}", task.download_path.display());
        }
        return Ok(DownloadOutcome::PartialFailure {
            failed_count: failed,
        });
    }

    Ok(DownloadOutcome::Success)
}

/// Result of a download pass.
#[derive(Debug)]
struct PassResult {
    exif_failures: usize,
    failed: Vec<DownloadTask>,
    auth_errors: usize,
}

/// Configuration for a download pass.
struct PassConfig<'a> {
    client: &'a Client,
    retry_config: &'a RetryConfig,
    set_exif: bool,
    concurrency: usize,
    no_progress_bar: bool,
    temp_suffix: String,
    shutdown_token: CancellationToken,
    state_db: Option<Arc<dyn StateDb>>,
}

/// Execute a download pass over the given tasks, returning any that failed.
async fn run_download_pass(config: PassConfig<'_>, tasks: Vec<DownloadTask>) -> PassResult {
    let pb = create_progress_bar(config.no_progress_bar, tasks.len() as u64);
    let client = config.client.clone();
    let retry_config = config.retry_config;
    let set_exif = config.set_exif;
    let state_db = config.state_db.clone();
    let shutdown_token = config.shutdown_token.clone();
    let concurrency = config.concurrency;
    let temp_suffix: Arc<str> = config.temp_suffix.into();

    let results: Vec<(DownloadTask, Result<bool>)> = stream::iter(tasks)
        .take_while(|_| std::future::ready(!shutdown_token.is_cancelled()))
        .map(|task| {
            let client = client.clone();
            let temp_suffix = Arc::clone(&temp_suffix);
            async move {
                let result =
                    download_single_task(&client, &task, retry_config, set_exif, &temp_suffix)
                        .await;
                (task, result)
            }
        })
        .buffer_unordered(concurrency)
        .collect()
        .await;

    let mut failed: Vec<DownloadTask> = Vec::new();
    let mut auth_errors = 0usize;
    let mut exif_failures = 0usize;

    // Collect DB updates for batch write
    let mut downloaded_batch: Vec<(String, String, PathBuf)> = Vec::new();
    let mut failed_batch: Vec<(String, String, String)> = Vec::new();

    for (task, result) in results {
        match &result {
            Ok(exif_ok) => {
                if !*exif_ok {
                    exif_failures += 1;
                }
                if state_db.is_some() {
                    downloaded_batch.push((
                        task.asset_id.to_string(),
                        task.version_size.as_str().to_string(),
                        task.download_path.clone(),
                    ));
                }
            }
            Err(e) => {
                // Check if this is a session expiry error
                if let Some(download_err) = e.downcast_ref::<DownloadError>() {
                    if download_err.is_session_expired() {
                        auth_errors += 1;
                        pb.suspend(|| {
                            tracing::warn!("Auth error: {} - {}", task.download_path.display(), e);
                        });
                    }
                } else {
                    pb.suspend(|| {
                        tracing::error!("Download failed: {}: {}", task.download_path.display(), e);
                    });
                }
                if state_db.is_some() {
                    failed_batch.push((
                        task.asset_id.to_string(),
                        task.version_size.as_str().to_string(),
                        e.to_string(),
                    ));
                }
                failed.push(task);
            }
        }
        pb.inc(1);
    }

    // Batch write DB updates
    if let Some(db) = &state_db {
        if !downloaded_batch.is_empty() {
            if let Err(e) = db.mark_downloaded_batch(&downloaded_batch).await {
                tracing::warn!(
                    "Failed to batch mark {} downloads: {}",
                    downloaded_batch.len(),
                    e
                );
            }
        }
        if !failed_batch.is_empty() {
            if let Err(e) = db.mark_failed_batch(&failed_batch).await {
                tracing::warn!(
                    "Failed to batch mark {} failures: {}",
                    failed_batch.len(),
                    e
                );
            }
        }
    }

    pb.finish_and_clear();
    PassResult {
        exif_failures,
        failed,
        auth_errors,
    }
}

/// Download a single task, handling mtime and EXIF stamping on success.
///
/// Returns `Ok(true)` on full success, `Ok(false)` if the download succeeded
/// but EXIF stamping failed (the file is usable but lacks EXIF metadata).
async fn download_single_task(
    client: &Client,
    task: &DownloadTask,
    retry_config: &RetryConfig,
    set_exif: bool,
    temp_suffix: &str,
) -> Result<bool> {
    if let Some(parent) = task.download_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    tracing::debug!(
        size_bytes = task.size,
        path = %task.download_path.display(),
        "downloading",
    );

    file::download_file(
        client,
        &task.url,
        &task.download_path,
        &task.checksum,
        false,
        retry_config,
        temp_suffix,
    )
    .await?;

    let mtime_path = task.download_path.clone();
    let ts = task.created_local.timestamp();
    if let Err(e) = tokio::task::spawn_blocking(move || set_file_mtime(&mtime_path, ts)).await? {
        tracing::warn!(
            "Could not set mtime on {}: {}",
            task.download_path.display(),
            e
        );
    }

    tracing::debug!("Downloaded {}", task.download_path.display());

    let mut exif_ok = true;
    if set_exif {
        let ext = task
            .download_path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        if matches!(ext.to_ascii_lowercase().as_str(), "jpg" | "jpeg") {
            let exif_path = task.download_path.clone();
            let date_str = task.created_local.format("%Y:%m:%d %H:%M:%S").to_string();
            let exif_result =
                tokio::task::spawn_blocking(move || match exif::get_photo_exif(&exif_path) {
                    Ok(None) => {
                        if let Err(e) = exif::set_photo_exif(&exif_path, &date_str) {
                            tracing::warn!("Failed to set EXIF on {}: {}", exif_path.display(), e);
                            false
                        } else {
                            true
                        }
                    }
                    Ok(Some(_)) => true,
                    Err(e) => {
                        tracing::warn!("Failed to read EXIF from {}: {}", exif_path.display(), e);
                        false
                    }
                })
                .await;
            match exif_result {
                Ok(ok) => exif_ok = ok,
                Err(e) => {
                    tracing::warn!("EXIF task panicked: {}", e);
                    exif_ok = false;
                }
            }

            // Restore mtime after EXIF modification (EXIF write updates mtime to "now")
            let mtime_path2 = task.download_path.clone();
            let ts2 = task.created_local.timestamp();
            if let Err(e) =
                tokio::task::spawn_blocking(move || set_file_mtime(&mtime_path2, ts2)).await?
            {
                tracing::warn!(
                    "Could not restore mtime on {}: {}",
                    task.download_path.display(),
                    e
                );
            }
        }
    }

    Ok(exif_ok)
}

fn format_duration(d: Duration) -> String {
    let total_secs = d.as_secs();
    let hours = total_secs / 3600;
    let mins = (total_secs % 3600) / 60;
    let secs = total_secs % 60;

    if hours > 0 {
        format!("{}h {:02}m {:02}s", hours, mins, secs)
    } else if mins > 0 {
        format!("{}m {:02}s", mins, secs)
    } else {
        format!("{}s", secs)
    }
}

/// Set the modification and access times of a file to the given Unix
/// timestamp. Uses `std::fs::File::set_times` (stable since Rust 1.75).
///
/// Handles negative timestamps (dates before 1970) gracefully by clamping
/// to the Unix epoch.
fn set_file_mtime(path: &Path, timestamp: i64) -> std::io::Result<()> {
    let time = if timestamp >= 0 {
        UNIX_EPOCH + Duration::from_secs(timestamp as u64)
    } else {
        tracing::warn!(
            path = %path.display(),
            timestamp,
            "Negative timestamp (pre-1970 date), clamping mtime to epoch"
        );
        UNIX_EPOCH
            .checked_sub(Duration::from_secs(timestamp.unsigned_abs()))
            .unwrap_or(SystemTime::UNIX_EPOCH)
    };
    let times = FileTimes::new().set_modified(time).set_accessed(time);
    let file = std::fs::File::options().write(true).open(path)?;
    file.set_times(times)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::icloud::photos::PhotoAsset;
    use serde_json::json;
    use std::fs;

    /// Cross-platform temp directory for tests
    fn test_tmp_dir(subdir: &str) -> PathBuf {
        std::env::temp_dir().join("claude").join(subdir)
    }

    fn tmp_file(name: &str) -> PathBuf {
        let dir = test_tmp_dir("download_tests");
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        fs::write(&p, b"test").unwrap();
        p
    }

    fn test_config() -> DownloadConfig {
        DownloadConfig {
            directory: test_tmp_dir("download_filter_tests"),
            folder_structure: "{:%Y/%m/%d}".to_string(),
            size: AssetVersionSize::Original,
            skip_videos: false,
            skip_photos: false,
            skip_created_before: None,
            skip_created_after: None,
            set_exif_datetime: false,
            dry_run: false,
            concurrent_downloads: 1,
            recent: None,
            retry: RetryConfig::default(),
            skip_live_photos: false,
            live_photo_size: AssetVersionSize::LiveOriginal,
            live_photo_mov_filename_policy: crate::types::LivePhotoMovFilenamePolicy::Suffix,
            align_raw: RawTreatmentPolicy::Unchanged,
            no_progress_bar: true,
            file_match_policy: FileMatchPolicy::NameSizeDedupWithSuffix,
            force_size: false,
            keep_unicode_in_filenames: false,
            temp_suffix: ".icloudpd-tmp".to_string(),
            state_db: None,
        }
    }

    /// Helper that calls filter_asset_to_tasks with a fresh claimed_paths map.
    /// Use this for simple tests that don't need to track paths across calls.
    fn filter_asset_fresh(
        asset: &PhotoAsset,
        config: &DownloadConfig,
    ) -> SmallVec<[DownloadTask; 2]> {
        let mut claimed_paths = FxHashMap::default();
        let mut dir_cache = std::collections::HashMap::new();
        filter_asset_to_tasks(asset, config, &mut claimed_paths, &mut dir_cache)
    }

    fn photo_asset_with_version() -> PhotoAsset {
        PhotoAsset::new(
            json!({"recordName": "TEST_1", "fields": {
                "filenameEnc": {"value": "photo.jpg", "type": "STRING"},
                "itemType": {"value": "public.jpeg"},
                "resOriginalRes": {"value": {
                    "size": 1000,
                    "downloadURL": "https://example.com/orig",
                    "fileChecksum": "abc123"
                }},
                "resOriginalFileType": {"value": "public.jpeg"}
            }}),
            json!({"fields": {"assetDate": {"value": 1736899200000.0}}}),
        )
    }

    #[test]
    fn test_filter_asset_produces_task() {
        let asset = photo_asset_with_version();
        let config = test_config();
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 1);
        assert_eq!(&*tasks[0].url, "https://example.com/orig");
        assert_eq!(&*tasks[0].checksum, "abc123");
        assert_eq!(tasks[0].size, 1000);
    }

    #[test]
    fn test_filter_skips_videos_when_configured() {
        let asset = PhotoAsset::new(
            json!({"recordName": "VID_1", "fields": {
                "filenameEnc": {"value": "movie.mov", "type": "STRING"},
                "itemType": {"value": "com.apple.quicktime-movie"},
                "resOriginalRes": {"value": {
                    "size": 50000,
                    "downloadURL": "https://example.com/vid",
                    "fileChecksum": "vid_ck"
                }},
                "resOriginalFileType": {"value": "com.apple.quicktime-movie"}
            }}),
            json!({"fields": {"assetDate": {"value": 1736899200000.0}}}),
        );
        let mut config = test_config();
        config.skip_videos = true;
        assert!(filter_asset_fresh(&asset, &config).is_empty());
    }

    #[test]
    fn test_filter_video_task_carries_size() {
        let asset = PhotoAsset::new(
            json!({"recordName": "VID_2", "fields": {
                "filenameEnc": {"value": "movie.mov", "type": "STRING"},
                "itemType": {"value": "com.apple.quicktime-movie"},
                "resOriginalRes": {"value": {
                    "size": 500_000_000,
                    "downloadURL": "https://example.com/big_vid",
                    "fileChecksum": "big_ck"
                }},
                "resOriginalFileType": {"value": "com.apple.quicktime-movie"}
            }}),
            json!({"fields": {"assetDate": {"value": 1736899200000.0}}}),
        );
        let config = test_config();
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].size, 500_000_000);
    }

    #[test]
    fn test_filter_skips_photos_when_configured() {
        let asset = photo_asset_with_version();
        let mut config = test_config();
        config.skip_photos = true;
        assert!(filter_asset_fresh(&asset, &config).is_empty());
    }

    #[test]
    fn test_filter_uses_fingerprint_fallback_without_filename() {
        // Asset ID with special chars proves fingerprint sanitization ran:
        // "AB/CD+EF==GH" → "AB_CD_EF__GH" (non-alphanumeric replaced with _)
        let asset = PhotoAsset::new(
            json!({"recordName": "AB/CD+EF==GH", "fields": {
                "itemType": {"value": "public.jpeg"},
                "resOriginalRes": {"value": {
                    "size": 1000,
                    "downloadURL": "https://example.com/orig",
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
                .contains("AB_CD_EF__GH.JPG"),
            "Expected fingerprint fallback filename, got: {:?}",
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
                    "downloadURL": "https://example.com/thumb",
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
        let dir = test_tmp_dir("download_filter_tests");
        fs::create_dir_all(&dir).unwrap();
        let asset = photo_asset_with_version();
        let mut config = test_config();
        config.directory = dir.clone();

        // First call should produce a task (file doesn't exist yet)
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 1);

        // Create the file with matching size (1000 bytes), second call should skip
        fs::create_dir_all(tasks[0].download_path.parent().unwrap()).unwrap();
        fs::write(&tasks[0].download_path, vec![0u8; 1000]).unwrap();
        assert!(filter_asset_fresh(&asset, &config).is_empty());

        // Cleanup
        let _ = fs::remove_file(&tasks[0].download_path);
    }

    #[test]
    fn test_filter_deduplicates_file_with_different_size() {
        let dir = test_tmp_dir("download_filter_tests_dedup");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let asset = photo_asset_with_version(); // version.size = 1000
        let mut config = test_config();
        config.directory = dir.clone();

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

        // Cleanup
        let _ = fs::remove_dir_all(&dir);
    }

    fn photo_asset_with_live_photo() -> PhotoAsset {
        PhotoAsset::new(
            json!({"recordName": "LIVE_1", "fields": {
                "filenameEnc": {"value": "IMG_0001.HEIC", "type": "STRING"},
                "itemType": {"value": "public.heic"},
                "resOriginalRes": {"value": {
                    "size": 2000,
                    "downloadURL": "https://example.com/heic_orig",
                    "fileChecksum": "heic_ck"
                }},
                "resOriginalFileType": {"value": "public.heic"},
                "resOriginalVidComplRes": {"value": {
                    "size": 3000,
                    "downloadURL": "https://example.com/live_mov",
                    "fileChecksum": "mov_ck"
                }},
                "resOriginalVidComplFileType": {"value": "com.apple.quicktime-movie"}
            }}),
            json!({"fields": {"assetDate": {"value": 1736899200000.0}}}),
        )
    }

    #[test]
    fn test_filter_produces_live_photo_mov_task() {
        let asset = photo_asset_with_live_photo();
        let config = test_config();
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 2);
        assert_eq!(&*tasks[0].url, "https://example.com/heic_orig");
        assert_eq!(tasks[0].size, 2000);
        assert_eq!(&*tasks[1].url, "https://example.com/live_mov");
        assert_eq!(tasks[1].size, 3000);
        assert!(tasks[1]
            .download_path
            .to_str()
            .unwrap()
            .contains("IMG_0001_HEVC.MOV"));
    }

    #[test]
    fn test_filter_skips_live_photo_when_configured() {
        let asset = photo_asset_with_live_photo();
        let mut config = test_config();
        config.skip_live_photos = true;
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 1);
        assert_eq!(&*tasks[0].url, "https://example.com/heic_orig");
    }

    #[test]
    fn test_filter_live_photo_original_policy() {
        let asset = photo_asset_with_live_photo();
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
        let dir = test_tmp_dir("download_filter_tests_live");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let asset = photo_asset_with_live_photo();
        let mut config = test_config();
        config.directory = dir.clone();

        // First call: both photo and MOV
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 2);

        // Create the MOV file on disk with matching size (3000 bytes)
        fs::create_dir_all(tasks[1].download_path.parent().unwrap()).unwrap();
        fs::write(&tasks[1].download_path, vec![0u8; 3000]).unwrap();

        // Second call: only the photo task (MOV already exists with matching size)
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 1);
        assert_eq!(&*tasks[0].url, "https://example.com/heic_orig");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_filter_deduplicates_live_photo_mov_collision() {
        let dir = test_tmp_dir("download_filter_tests_live_dedup");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let asset = photo_asset_with_live_photo();
        let mut config = test_config();
        config.directory = dir.clone();

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
        assert_eq!(&*tasks[1].url, "https://example.com/live_mov");
        let dedup_path = tasks[1].download_path.to_str().unwrap();
        assert!(
            dedup_path.contains("LIVE_1"),
            "Expected asset ID 'LIVE_1' in deduped path, got: {}",
            dedup_path,
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_filter_live_photo_dedup_suffix_consistent_with_mov() {
        // Regression test for #102: when two live photos share the same base
        // filename but have different sizes (triggering dedup), the MOV companion
        // must derive from the deduped HEIC name so they remain visually paired.
        let dir = test_tmp_dir("download_filter_tests_live_dedup_consistency");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let asset1 = PhotoAsset::new(
            json!({"recordName": "LIVE_A", "fields": {
                "filenameEnc": {"value": "IMG_0001.HEIC", "type": "STRING"},
                "itemType": {"value": "public.heic"},
                "resOriginalRes": {"value": {
                    "size": 2000,
                    "downloadURL": "https://example.com/heic_a",
                    "fileChecksum": "ck_a"
                }},
                "resOriginalFileType": {"value": "public.heic"},
                "resOriginalVidComplRes": {"value": {
                    "size": 3000,
                    "downloadURL": "https://example.com/mov_a",
                    "fileChecksum": "mov_ck_a"
                }},
                "resOriginalVidComplFileType": {"value": "com.apple.quicktime-movie"}
            }}),
            json!({"fields": {"assetDate": {"value": 1736899200000.0}}}),
        );

        let asset2 = PhotoAsset::new(
            json!({"recordName": "LIVE_B", "fields": {
                "filenameEnc": {"value": "IMG_0001.HEIC", "type": "STRING"},
                "itemType": {"value": "public.heic"},
                "resOriginalRes": {"value": {
                    "size": 4000,
                    "downloadURL": "https://example.com/heic_b",
                    "fileChecksum": "ck_b"
                }},
                "resOriginalFileType": {"value": "public.heic"},
                "resOriginalVidComplRes": {"value": {
                    "size": 5000,
                    "downloadURL": "https://example.com/mov_b",
                    "fileChecksum": "mov_ck_b"
                }},
                "resOriginalVidComplFileType": {"value": "com.apple.quicktime-movie"}
            }}),
            json!({"fields": {"assetDate": {"value": 1736899200000.0}}}),
        );

        let mut config = test_config();
        config.directory = dir.clone();

        // Process asset1: creates IMG_0001.HEIC (2000 bytes) and its MOV
        let mut claimed_paths = FxHashMap::default();
        let mut dir_cache = std::collections::HashMap::new();
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

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_filter_live_photo_medium_size() {
        let asset = PhotoAsset::new(
            json!({"recordName": "LIVE_MED", "fields": {
                "filenameEnc": {"value": "IMG_0002.HEIC", "type": "STRING"},
                "itemType": {"value": "public.heic"},
                "resOriginalRes": {"value": {
                    "size": 2000,
                    "downloadURL": "https://example.com/heic_orig",
                    "fileChecksum": "heic_ck"
                }},
                "resOriginalFileType": {"value": "public.heic"},
                "resVidMedRes": {"value": {
                    "size": 1500,
                    "downloadURL": "https://example.com/live_med",
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
        assert_eq!(&*tasks[1].url, "https://example.com/live_med");
    }

    #[test]
    fn test_filter_no_live_photo_for_videos() {
        let asset = PhotoAsset::new(
            json!({"recordName": "VID_1", "fields": {
                "filenameEnc": {"value": "movie.mov", "type": "STRING"},
                "itemType": {"value": "com.apple.quicktime-movie"},
                "resOriginalRes": {"value": {
                    "size": 50000,
                    "downloadURL": "https://example.com/vid",
                    "fileChecksum": "vid_ck"
                }},
                "resOriginalFileType": {"value": "com.apple.quicktime-movie"},
                "resOriginalVidComplRes": {"value": {
                    "size": 3000,
                    "downloadURL": "https://example.com/live_mov",
                    "fileChecksum": "mov_ck"
                }},
                "resOriginalVidComplFileType": {"value": "com.apple.quicktime-movie"}
            }}),
            json!({"fields": {"assetDate": {"value": 1736899200000.0}}}),
        );
        let config = test_config();
        let tasks = filter_asset_fresh(&asset, &config);
        // Videos should get 1 task (the video itself), not a live photo MOV
        assert_eq!(tasks.len(), 1);
    }

    #[test]
    fn test_set_file_mtime_positive_timestamp() {
        let p = tmp_file("pos.txt");
        set_file_mtime(&p, 1_700_000_000).unwrap();
        let meta = fs::metadata(&p).unwrap();
        let mtime = meta.modified().unwrap();
        assert_eq!(mtime, UNIX_EPOCH + Duration::from_secs(1_700_000_000));
    }

    #[test]
    fn test_set_file_mtime_zero_timestamp() {
        let p = tmp_file("zero.txt");
        set_file_mtime(&p, 0).unwrap();
        let meta = fs::metadata(&p).unwrap();
        let mtime = meta.modified().unwrap();
        assert_eq!(mtime, UNIX_EPOCH);
    }

    #[test]
    fn test_set_file_mtime_negative_timestamp() {
        let p = tmp_file("neg.txt");
        // Should not panic — clamps or uses pre-epoch time
        set_file_mtime(&p, -86400).unwrap();
    }

    #[test]
    fn test_set_file_mtime_nonexistent_file() {
        let p = test_tmp_dir("download_tests").join("nonexistent_file.txt");
        let _ = fs::remove_file(&p); // ensure absent
        assert!(set_file_mtime(&p, 0).is_err());
    }

    fn photo_asset_with_original_and_alternative(orig_type: &str, alt_type: &str) -> PhotoAsset {
        PhotoAsset::new(
            json!({"recordName": "RAW_TEST", "fields": {
                "filenameEnc": {"value": "photo.jpg", "type": "STRING"},
                "itemType": {"value": "public.jpeg"},
                "resOriginalRes": {"value": {
                    "size": 1000,
                    "downloadURL": "https://example.com/orig",
                    "fileChecksum": "orig_ck"
                }},
                "resOriginalFileType": {"value": orig_type},
                "resOriginalAltRes": {"value": {
                    "size": 2000,
                    "downloadURL": "https://example.com/alt",
                    "fileChecksum": "alt_ck"
                }},
                "resOriginalAltFileType": {"value": alt_type}
            }}),
            json!({"fields": {"assetDate": {"value": 1736899200000.0}}}),
        )
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
            "https://example.com/orig"
        );
        assert_eq!(
            &*get_ver(&versions, AssetVersionSize::Alternative)
                .unwrap()
                .url,
            "https://example.com/alt"
        );
    }

    #[test]
    fn test_raw_policy_as_original_swaps_when_alt_is_raw() {
        let asset = photo_asset_with_original_and_alternative("public.jpeg", "com.adobe.raw-image");
        let versions = apply_raw_policy(asset.versions(), RawTreatmentPolicy::PreferOriginal);
        // Alternative was RAW → swap: Original now has alt URL
        assert_eq!(
            &*get_ver(&versions, AssetVersionSize::Original).unwrap().url,
            "https://example.com/alt"
        );
        assert_eq!(
            &*get_ver(&versions, AssetVersionSize::Alternative)
                .unwrap()
                .url,
            "https://example.com/orig"
        );
    }

    #[test]
    fn test_raw_policy_as_alternative_swaps_when_orig_is_raw() {
        let asset = photo_asset_with_original_and_alternative("com.adobe.raw-image", "public.jpeg");
        let versions = apply_raw_policy(asset.versions(), RawTreatmentPolicy::PreferAlternative);
        // Original was RAW → swap: Alternative now has orig URL
        assert_eq!(
            &*get_ver(&versions, AssetVersionSize::Original).unwrap().url,
            "https://example.com/alt"
        );
        assert_eq!(
            &*get_ver(&versions, AssetVersionSize::Alternative)
                .unwrap()
                .url,
            "https://example.com/orig"
        );
    }

    #[test]
    fn test_raw_policy_as_original_no_swap_when_alt_not_raw() {
        let asset = photo_asset_with_original_and_alternative("public.jpeg", "public.jpeg");
        let versions = apply_raw_policy(asset.versions(), RawTreatmentPolicy::PreferOriginal);
        assert_eq!(
            &*get_ver(&versions, AssetVersionSize::Original).unwrap().url,
            "https://example.com/orig"
        );
    }

    #[test]
    fn test_raw_policy_as_alternative_no_swap_when_orig_not_raw() {
        let asset = photo_asset_with_original_and_alternative("public.jpeg", "public.jpeg");
        let versions = apply_raw_policy(asset.versions(), RawTreatmentPolicy::PreferAlternative);
        assert_eq!(
            &*get_ver(&versions, AssetVersionSize::Original).unwrap().url,
            "https://example.com/orig"
        );
    }

    #[test]
    fn test_raw_policy_no_alternative_no_swap() {
        let asset = photo_asset_with_version(); // only has Original
        let versions = apply_raw_policy(asset.versions(), RawTreatmentPolicy::PreferOriginal);
        assert_eq!(
            &*get_ver(&versions, AssetVersionSize::Original).unwrap().url,
            "https://example.com/orig"
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
        assert_eq!(&*tasks[0].url, "https://example.com/alt");
        assert_eq!(&*tasks[0].checksum, "alt_ck");
    }

    #[test]
    fn test_format_duration_seconds_only() {
        assert_eq!(format_duration(Duration::from_secs(0)), "0s");
        assert_eq!(format_duration(Duration::from_secs(1)), "1s");
        assert_eq!(format_duration(Duration::from_secs(42)), "42s");
        assert_eq!(format_duration(Duration::from_secs(59)), "59s");
    }

    #[test]
    fn test_format_duration_minutes_and_seconds() {
        assert_eq!(format_duration(Duration::from_secs(60)), "1m 00s");
        assert_eq!(format_duration(Duration::from_secs(61)), "1m 01s");
        assert_eq!(format_duration(Duration::from_secs(754)), "12m 34s");
        assert_eq!(format_duration(Duration::from_secs(3599)), "59m 59s");
    }

    #[test]
    fn test_format_duration_hours() {
        assert_eq!(format_duration(Duration::from_secs(3600)), "1h 00m 00s");
        assert_eq!(format_duration(Duration::from_secs(5025)), "1h 23m 45s");
        assert_eq!(format_duration(Duration::from_secs(86399)), "23h 59m 59s");
    }

    #[test]
    fn test_create_progress_bar_hidden_when_disabled() {
        let pb = create_progress_bar(true, 100);
        assert!(pb.is_hidden());
    }

    #[test]
    fn test_filter_detects_case_insensitive_collision() {
        // On case-insensitive filesystems (macOS, Windows), IMG_0996.mov and IMG_0996.MOV
        // are the same file. Test that claimed_paths detects this collision.
        let dir = test_tmp_dir("download_filter_tests_case");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        // First asset: regular video IMG_0996.mov
        let video_asset = PhotoAsset::new(
            json!({"recordName": "VID_0996", "fields": {
                "filenameEnc": {"value": "IMG_0996.mov", "type": "STRING"},
                "itemType": {"value": "com.apple.quicktime-movie"},
                "resOriginalRes": {"value": {
                    "size": 258592890,
                    "downloadURL": "https://example.com/vid",
                    "fileChecksum": "vid_ck"
                }},
                "resOriginalFileType": {"value": "com.apple.quicktime-movie"}
            }}),
            json!({"fields": {"assetDate": {"value": 1713657600000.0}}}), // 2025/04/21
        );

        // Second asset: live photo IMG_0996.JPG whose MOV companion would be IMG_0996.MOV
        let photo_asset = PhotoAsset::new(
            json!({"recordName": "IMG_0996", "fields": {
                "filenameEnc": {"value": "IMG_0996.JPG", "type": "STRING"},
                "itemType": {"value": "public.jpeg"},
                "resOriginalRes": {"value": {
                    "size": 5000,
                    "downloadURL": "https://example.com/jpg",
                    "fileChecksum": "jpg_ck"
                }},
                "resOriginalFileType": {"value": "public.jpeg"},
                "resOriginalVidComplRes": {"value": {
                    "size": 124037918,
                    "downloadURL": "https://example.com/live_mov",
                    "fileChecksum": "mov_ck"
                }},
                "resOriginalVidComplFileType": {"value": "com.apple.quicktime-movie"}
            }}),
            json!({"fields": {"assetDate": {"value": 1713657600000.0}}}), // Same date
        );

        let mut config = test_config();
        config.directory = dir.clone();

        // Process both assets through claimed_paths
        let mut claimed_paths = FxHashMap::default();
        let mut dir_cache = std::collections::HashMap::new();
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

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_create_progress_bar_with_total() {
        // When not disabled, the bar should have the correct length.
        // In CI/test environments stdout may not be a TTY, so the bar
        // may be hidden — we test both branches.
        let pb = create_progress_bar(false, 42);
        if std::io::stdout().is_terminal() {
            assert!(!pb.is_hidden());
            assert_eq!(pb.length(), Some(42));
        } else {
            // Non-TTY: bar is hidden regardless of the flag
            assert!(pb.is_hidden());
        }
    }

    #[test]
    fn test_filter_asset_as_is_downloads_original() {
        let asset = photo_asset_with_original_and_alternative("public.jpeg", "com.adobe.raw-image");
        let config = test_config(); // align_raw defaults to AsIs
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 1);
        assert_eq!(&*tasks[0].url, "https://example.com/orig");
        assert_eq!(&*tasks[0].checksum, "orig_ck");
    }

    // These tests overflow the stack in debug builds due to large async futures
    // from reqwest and stream combinators. Run with: cargo test --release
    #[tokio::test]
    #[ignore = "stack overflow in debug builds; run with --release"]
    async fn test_run_download_pass_skips_all_tasks_when_cancelled() {
        let token = CancellationToken::new();
        token.cancel();

        let tasks = vec![
            DownloadTask {
                url: "https://example.com/a".into(),
                download_path: test_tmp_dir("shutdown_test").join("a.jpg"),
                checksum: "aaa".into(),
                created_local: chrono::Local::now(),
                size: 1000,
                asset_id: "ASSET_A".into(),
                version_size: VersionSizeKey::Original,
            },
            DownloadTask {
                url: "https://example.com/b".into(),
                download_path: test_tmp_dir("shutdown_test").join("b.jpg"),
                checksum: "bbb".into(),
                created_local: chrono::Local::now(),
                size: 2000,
                asset_id: "ASSET_B".into(),
                version_size: VersionSizeKey::Original,
            },
        ];

        let client = Client::new();
        let retry = RetryConfig::default();

        // Pre-cancelled token: take_while stops immediately, no downloads attempted.
        let pass_config = PassConfig {
            client: &client,
            retry_config: &retry,
            set_exif: false,
            concurrency: 1,
            no_progress_bar: true,
            temp_suffix: ".icloudpd-tmp".to_string(),
            shutdown_token: token,
            state_db: None,
        };
        let result = run_download_pass(pass_config, tasks).await;
        assert!(result.failed.is_empty());
    }

    #[tokio::test]
    #[ignore = "stack overflow in debug builds; run with --release"]
    async fn test_run_download_pass_processes_tasks_when_not_cancelled() {
        let token = CancellationToken::new();

        let tasks = vec![DownloadTask {
            url: "https://0.0.0.0:1/nonexistent".into(),
            download_path: test_tmp_dir("shutdown_test").join("c.jpg"),
            checksum: "ccc".into(),
            created_local: chrono::Local::now(),
            size: 500,
            asset_id: "ASSET_C".into(),
            version_size: VersionSizeKey::Original,
        }];

        let client = Client::new();
        let retry = RetryConfig {
            max_retries: 0,
            base_delay_secs: 0,
            max_delay_secs: 0,
        };

        // Non-cancelled token: task is attempted (and fails since URL is bogus).
        let pass_config = PassConfig {
            client: &client,
            retry_config: &retry,
            set_exif: false,
            concurrency: 1,
            no_progress_bar: true,
            temp_suffix: ".icloudpd-tmp".to_string(),
            shutdown_token: token,
            state_db: None,
        };
        let result = run_download_pass(pass_config, tasks).await;
        assert_eq!(result.failed.len(), 1);
    }

    #[test]
    fn test_download_task_size() {
        use std::mem::size_of;
        // 144 bytes accommodates platform differences (Windows has larger PathBuf)
        assert!(
            size_of::<DownloadTask>() <= 144,
            "DownloadTask size {} exceeds 144 bytes",
            size_of::<DownloadTask>()
        );
    }
}
