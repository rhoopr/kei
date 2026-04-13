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

use anyhow::{Context, Result};
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
use crate::icloud::photos::{PhotoAlbum, PhotoAsset, SyncTokenError, VersionsMap};
use crate::retry::RetryConfig;
use crate::state::{AssetRecord, MediaType, StateDb, SyncRunStats, VersionSizeKey};
use crate::types::{
    AssetItemType, AssetVersionSize, ChangeReason, FileMatchPolicy, LivePhotoMode,
    LivePhotoMovFilenamePolicy, RawTreatmentPolicy,
};

use error::DownloadError;

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
struct NormalizedPath(Box<str>);

impl NormalizedPath {
    /// Create a new normalized path from an owned `PathBuf`.
    /// For lookup operations, prefer `normalize()` to avoid `PathBuf` cloning.
    fn new(path: PathBuf) -> Self {
        Self(Self::normalize(&path).into_owned().into_boxed_str())
    }

    /// Normalize a path reference for map lookups.
    ///
    /// On case-insensitive systems (macOS, Windows), returns a lowercase copy.
    /// On case-sensitive systems (Linux), returns a borrowed view when possible.
    ///
    /// Use with `claimed_paths.contains_key(NormalizedPath::normalize(&path).as_ref())`
    /// to avoid allocating a `PathBuf` just for the lookup.
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

/// How the sync should enumerate photos from iCloud.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncMode {
    /// Full enumeration via records/query (existing behavior).
    /// On completion, captures the syncToken for future incremental syncs.
    Full,
    /// Incremental delta sync via changes/zone with a stored syncToken.
    /// Falls back to Full if the token is invalid/expired.
    Incremental {
        /// The stored syncToken for the zone being synced.
        zone_sync_token: String,
    },
}

/// Result of a sync cycle, including the optional new syncToken.
#[derive(Debug)]
pub struct SyncResult {
    /// The outcome of the download pass (success, session expired, partial failure).
    pub outcome: DownloadOutcome,
    /// The new zone-level syncToken, if one was captured during this sync.
    /// Store this for the next incremental sync.
    pub sync_token: Option<String>,
}

/// Truncate a `DateTime<Utc>` to midnight so that relative date intervals
/// (e.g. `20d` → `now - 20 days`) produce a stable hash within the same
/// calendar day.
fn truncate_date_to_day(dt: Option<DateTime<Utc>>) -> Option<chrono::NaiveDate> {
    dt.map(|d| d.date_naive())
}

/// Compute a deterministic hash of the config fields that affect path resolution.
///
/// When this hash changes between runs, we can't trust the state DB's download
/// records (the resolved paths may differ), so we fall back to the full pipeline
/// with filesystem existence checks.
///
/// Also called from `main.rs` (via [`compute_config_hash`]) to clear sync tokens
/// before the incremental-vs-full decision when the download config changes.
pub(crate) fn hash_download_config(config: &DownloadConfig) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write;

    let mut hasher = Sha256::new();
    hasher.update(config.directory.as_os_str().as_encoded_bytes());
    hasher.update(b"\0");
    hasher.update(config.folder_structure.as_bytes());
    hasher.update(b"\0");
    hasher.update(format!("{:?}", config.size).as_bytes());
    hasher.update(format!("{:?}", config.live_photo_size).as_bytes());
    hasher.update(format!("{:?}", config.file_match_policy).as_bytes());
    hasher.update(format!("{:?}", config.live_photo_mov_filename_policy).as_bytes());
    hasher.update(format!("{:?}", config.align_raw).as_bytes());
    hasher.update([u8::from(config.keep_unicode_in_filenames)]);
    // Filter fields: changing these affects which assets are eligible, so we
    // must invalidate the trust-state cache (and stored sync tokens) to avoid
    // skipping newly-eligible assets on incremental syncs.
    //
    // Dates are truncated to day precision before hashing so that relative
    // intervals like "20d" (resolved to now-minus-20-days at parse time)
    // produce a stable hash across consecutive runs on the same day.
    hasher.update(format!("{:?}", truncate_date_to_day(config.skip_created_before)).as_bytes());
    hasher.update(format!("{:?}", truncate_date_to_day(config.skip_created_after)).as_bytes());
    hasher.update(format!("{:?}", config.recent).as_bytes());
    hasher.update([u8::from(config.force_size)]);
    hasher.update([u8::from(config.skip_videos)]);
    hasher.update([u8::from(config.skip_photos)]);
    hasher.update([config.live_photo_mode as u8]);
    // filename_exclude patterns affect which assets are eligible
    let mut sorted_excludes: Vec<&str> = config
        .filename_exclude
        .iter()
        .map(glob::Pattern::as_str)
        .collect();
    sorted_excludes.sort_unstable();
    for pattern in &sorted_excludes {
        hasher.update(pattern.as_bytes());
        hasher.update(b"\0");
    }
    let hash = hasher.finalize();
    let mut hex = String::with_capacity(16);
    // First 8 bytes is plenty for collision avoidance in this context
    for &b in &hash[..8] {
        let _ = Write::write_fmt(&mut hex, format_args!("{b:02x}"));
    }
    hex
}

/// Compute the config hash from the app-level `Config`.
///
/// Called from `main.rs` before the sync-mode decision so that stale sync
/// tokens are cleared when the download config changes.
///
/// This hash is a SUPERSET of [`hash_download_config`]: it includes all
/// the fields that affect download paths (shared with hash_download_config)
/// plus enumeration-filter fields (albums, library, live_photo_mode) that
/// affect WHICH assets are eligible. Changing these filters must invalidate
/// sync tokens so the next run does a full enumeration.
pub(crate) fn compute_config_hash(config: &crate::config::Config) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write;

    let size: AssetVersionSize = config.size.into();
    let live_photo_size = config.live_photo_size.to_asset_version_size();
    let skip_created_before = config
        .skip_created_before
        .map(|d| d.with_timezone(&chrono::Utc));
    let skip_created_after = config
        .skip_created_after
        .map(|d| d.with_timezone(&chrono::Utc));

    let mut hasher = Sha256::new();
    hasher.update(config.directory.as_os_str().as_encoded_bytes());
    hasher.update(b"\0");
    hasher.update(config.folder_structure.as_bytes());
    hasher.update(b"\0");
    hasher.update(format!("{size:?}").as_bytes());
    hasher.update(format!("{live_photo_size:?}").as_bytes());
    hasher.update(format!("{:?}", config.file_match_policy).as_bytes());
    hasher.update(format!("{:?}", config.live_photo_mov_filename_policy).as_bytes());
    hasher.update(format!("{:?}", config.align_raw).as_bytes());
    hasher.update([u8::from(config.keep_unicode_in_filenames)]);
    hasher.update(format!("{:?}", truncate_date_to_day(skip_created_before)).as_bytes());
    hasher.update(format!("{:?}", truncate_date_to_day(skip_created_after)).as_bytes());
    // Note: `recent` is intentionally excluded from this enum hash.
    // Changing --recent should not invalidate sync tokens because the
    // incremental path already applies the recent cap post-fetch.
    // `recent` IS included in hash_download_config (trust-state) so
    // changing it still triggers filesystem re-verification.
    hasher.update([u8::from(config.force_size)]);
    hasher.update([u8::from(config.skip_videos)]);
    hasher.update([u8::from(config.skip_photos)]);
    // Enumeration-filter fields: changing these affects WHICH assets are
    // fetched from iCloud, so sync tokens must be invalidated to avoid
    // missing assets that are newly eligible under the changed filters.
    hasher.update([config.live_photo_mode as u8]);
    for album in &config.albums {
        hasher.update(album.as_bytes());
        hasher.update(b"\0");
    }
    let mut sorted_excludes: Vec<&str> = config
        .exclude_albums
        .iter()
        .map(std::string::String::as_str)
        .collect();
    sorted_excludes.sort_unstable();
    for name in &sorted_excludes {
        hasher.update(b"exclude:");
        hasher.update(name.as_bytes());
        hasher.update(b"\0");
    }
    let mut sorted_fn_excludes: Vec<&str> = config
        .filename_exclude
        .iter()
        .map(glob::Pattern::as_str)
        .collect();
    sorted_fn_excludes.sort_unstable();
    for pattern in &sorted_fn_excludes {
        hasher.update(b"fnexclude:");
        hasher.update(pattern.as_bytes());
        hasher.update(b"\0");
    }
    hasher.update(format!("{:?}", config.library).as_bytes());
    let hash = hasher.finalize();
    let mut hex = String::with_capacity(16);
    for &b in &hash[..8] {
        let _ = Write::write_fmt(&mut hex, format_args!("{b:02x}"));
    }
    hex
}

/// Subset of application config consumed by the download engine.
/// Decoupled from CLI parsing so the engine can be tested independently.
pub(crate) struct DownloadConfig {
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
    pub(crate) live_photo_mode: LivePhotoMode,
    pub(crate) live_photo_size: AssetVersionSize,
    pub(crate) live_photo_mov_filename_policy: LivePhotoMovFilenamePolicy,
    pub(crate) align_raw: RawTreatmentPolicy,
    pub(crate) no_progress_bar: bool,
    pub(crate) only_print_filenames: bool,
    pub(crate) file_match_policy: FileMatchPolicy,
    pub(crate) force_size: bool,
    pub(crate) keep_unicode_in_filenames: bool,
    /// Compiled glob patterns for filename exclusion.
    pub(crate) filename_exclude: Vec<glob::Pattern>,
    /// Temp file suffix for partial downloads (e.g. `.kei-tmp`).
    pub(crate) temp_suffix: String,
    /// State database for tracking download progress.
    pub(crate) state_db: Option<Arc<dyn StateDb>>,
    /// When true (retry-failed mode), only download assets already known to the
    /// state DB. Skip new assets discovered from iCloud that were never synced.
    pub(crate) retry_only: bool,
    /// Sync mode: full enumeration or incremental delta via syncToken.
    pub(crate) sync_mode: SyncMode,
    /// Album name for `{album}` token in folder_structure. Set per-album when
    /// processing albums individually.
    pub(crate) album_name: Option<Arc<str>>,
    /// Asset IDs to exclude (from `--exclude-album` without `--album`).
    pub(crate) exclude_asset_ids: Arc<FxHashSet<String>>,
    /// Maximum download attempts per asset before giving up (0 = unlimited).
    pub(crate) max_download_attempts: u32,
}

impl DownloadConfig {
    /// Clone this config with a different `album_name`, for per-album processing
    /// when `{album}` is in `folder_structure`. Pre-expands the `{album}` token
    /// in `folder_structure` so `local_download_dir` avoids per-asset
    /// sanitize/escape/replace allocations.
    fn with_album_name(&self, name: Arc<str>) -> Self {
        let album_ref = Some(name.as_ref()).filter(|n: &&str| !n.is_empty());
        let folder_structure = paths::expand_album_token(&self.folder_structure, album_ref);
        Self {
            album_name: Some(name),
            directory: self.directory.clone(),
            folder_structure,
            filename_exclude: self.filename_exclude.clone(),
            temp_suffix: self.temp_suffix.clone(),
            state_db: self.state_db.clone(),
            sync_mode: self.sync_mode.clone(),
            exclude_asset_ids: Arc::clone(&self.exclude_asset_ids),
            ..*self
        }
    }
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
            .field("live_photo_mode", &self.live_photo_mode)
            .field("live_photo_size", &self.live_photo_size)
            .field(
                "live_photo_mov_filename_policy",
                &self.live_photo_mov_filename_policy,
            )
            .field("align_raw", &self.align_raw)
            .field("no_progress_bar", &self.no_progress_bar)
            .field("only_print_filenames", &self.only_print_filenames)
            .field("file_match_policy", &self.file_match_policy)
            .field("force_size", &self.force_size)
            .field("keep_unicode_in_filenames", &self.keep_unicode_in_filenames)
            .field("filename_exclude", &self.filename_exclude)
            .field("temp_suffix", &self.temp_suffix)
            .field("state_db", &self.state_db.is_some())
            .field("retry_only", &self.retry_only)
            .field("sync_mode", &self.sync_mode)
            .field("album_name", &self.album_name)
            .field("exclude_asset_ids_count", &self.exclude_asset_ids.len())
            .field("max_download_attempts", &self.max_download_attempts)
            .finish()
    }
}

/// A unit of work produced by the filter phase and consumed by the download phase.
///
/// Fields ordered for optimal memory layout:
/// - Heap types first (`Box<str>`, `PathBuf`)
/// - 8-byte primitives (u64)
/// - `DateTime` (12-16 bytes)
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
/// Uses a two-level map structure (`asset_id` -> `version_sizes`) to enable
/// zero-allocation lookups via `&str` keys, avoiding the need to allocate
/// `(String, String)` tuples for each lookup.
#[derive(Debug, Default)]
struct DownloadContext {
    /// Nested map: `asset_id` -> set of `version_sizes` that are already downloaded.
    /// Two-level structure enables O(1) borrowed lookups without allocation.
    downloaded_ids: FxHashMap<Box<str>, FxHashSet<Box<str>>>,
    /// Nested map: `asset_id` -> (`version_size` -> checksum) for downloaded assets.
    /// Used to detect checksum changes (iCloud asset updated) without DB queries.
    downloaded_checksums: FxHashMap<Box<str>, FxHashMap<Box<str>, Box<str>>>,
    /// All asset IDs known to the state DB (any status). Used in retry-only mode
    /// to skip new assets that were never synced.
    known_ids: FxHashSet<Box<str>>,
    /// Per-asset maximum download attempt count (from failed assets).
    /// Used to skip assets that have exceeded `max_download_attempts`.
    attempt_counts: FxHashMap<Box<str>, u32>,
}

impl DownloadContext {
    /// Load the download context from the state database.
    async fn load(db: &dyn StateDb, retry_only: bool) -> Self {
        // Build nested map structure for zero-allocation lookups
        let mut downloaded_ids: FxHashMap<Box<str>, FxHashSet<Box<str>>> = FxHashMap::default();
        for (asset_id, version_size) in db.get_downloaded_ids().await.unwrap_or_else(|e| {
            tracing::warn!(error = %e, "Failed to load downloaded IDs from state DB");
            Default::default()
        }) {
            downloaded_ids
                .entry(asset_id.into_boxed_str())
                .or_default()
                .insert(version_size.into_boxed_str());
        }

        let mut downloaded_checksums: FxHashMap<Box<str>, FxHashMap<Box<str>, Box<str>>> =
            FxHashMap::default();
        for ((asset_id, version_size), checksum) in
            db.get_downloaded_checksums().await.unwrap_or_else(|e| {
                tracing::warn!(error = %e, "Failed to load checksums from state DB");
                Default::default()
            })
        {
            downloaded_checksums
                .entry(asset_id.into_boxed_str())
                .or_default()
                .insert(version_size.into_boxed_str(), checksum.into_boxed_str());
        }

        // In retry-only mode, load all known asset IDs so we can skip new
        // assets that were never synced before.
        let known_ids = if retry_only {
            db.get_all_known_ids()
                .await
                .unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "Failed to load known IDs from state DB");
                    Default::default()
                })
                .into_iter()
                .map(String::into_boxed_str)
                .collect()
        } else {
            FxHashSet::default()
        };

        let attempt_counts: FxHashMap<Box<str>, u32> = db
            .get_attempt_counts()
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, "Failed to load attempt counts from state DB");
                Default::default()
            })
            .into_iter()
            .map(|(id, count)| (id.into_boxed_str(), count))
            .collect();

        Self {
            downloaded_ids,
            downloaded_checksums,
            known_ids,
            attempt_counts,
        }
    }

    /// Check if an asset should be downloaded based on pre-loaded state.
    ///
    /// Returns:
    /// - `Some(true)` — definitely needs download (not in DB or checksum changed)
    /// - `Some(false)` — hard skip, DB confirms downloaded with matching checksum
    ///   (only when `trust_state` is true)
    /// - `None` — downloaded with matching checksum but needs filesystem check
    ///   to confirm file is still on disk (when `trust_state` is false)
    ///
    /// Uses borrowed `&str` keys for zero-allocation lookups.
    fn should_download_fast(
        &self,
        asset_id: &str,
        version_size: VersionSizeKey,
        checksum: &str,
        trust_state: bool,
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

        // Downloaded with matching checksum
        if trust_state {
            // Trust the DB — hard skip without filesystem check
            Some(false)
        } else {
            // Need filesystem check to confirm file is still on disk
            None
        }
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
    let mut dir_cache = paths::DirCache::new();
    for album_result in album_results {
        let assets = album_result?;

        for asset in &assets {
            pre_ensure_asset_dir(&mut dir_cache, asset, config).await;
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

    let Some(alt_idx) = alt_idx else {
        return std::borrow::Cow::Borrowed(versions);
    };

    let should_swap = match policy {
        RawTreatmentPolicy::PreferOriginal => versions[alt_idx].1.asset_type.contains("raw"),
        RawTreatmentPolicy::PreferAlternative => {
            orig_idx.is_some_and(|idx| versions[idx].1.asset_type.contains("raw"))
        }
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

/// Lightweight pre-check: extract (`version_size`, checksum) pairs for an asset
/// after applying content/date filters but WITHOUT path resolution or disk I/O.
///
/// Returns the candidate versions that would be downloaded. Used by the early
/// skip gate to check the state DB before the expensive `filter_asset_to_tasks`.
fn extract_skip_candidates<'a>(
    asset: &'a crate::icloud::photos::PhotoAsset,
    config: &DownloadConfig,
) -> SmallVec<[(VersionSizeKey, &'a str); 2]> {
    // Excluded album assets -- same as filter_asset_to_tasks
    if config.exclude_asset_ids.contains(asset.id()) {
        return SmallVec::new();
    }

    // Content type filters -- same as filter_asset_to_tasks
    if config.skip_videos && asset.item_type() == Some(AssetItemType::Movie) {
        return SmallVec::new();
    }
    if config.skip_photos && asset.item_type() == Some(AssetItemType::Image) {
        return SmallVec::new();
    }

    let is_live_photo = asset.is_live_photo();
    if config.live_photo_mode == LivePhotoMode::Skip && is_live_photo {
        return SmallVec::new();
    }

    // Date filters
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

    // Filename exclusion -- mirrors filter_asset_to_tasks
    if !config.filename_exclude.is_empty() {
        if let Some(filename) = asset.filename() {
            if config
                .filename_exclude
                .iter()
                .any(|p| p.matches_with(filename, GLOB_CASE_INSENSITIVE))
            {
                return SmallVec::new();
            }
        }
    }

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
async fn pre_ensure_asset_dir(
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
    dir_cache: &mut paths::DirCache,
) -> SmallVec<[DownloadTask; 2]> {
    if config.exclude_asset_ids.contains(asset.id()) {
        tracing::debug!(asset_id = %asset.id(), "Skipping (excluded album asset)");
        return SmallVec::new();
    }
    if config.skip_videos && asset.item_type() == Some(AssetItemType::Movie) {
        tracing::debug!(asset_id = %asset.id(), "Skipping video (skip_videos enabled)");
        return SmallVec::new();
    }
    if config.skip_photos && asset.item_type() == Some(AssetItemType::Image) {
        tracing::debug!(asset_id = %asset.id(), "Skipping photo (skip_photos enabled)");
        return SmallVec::new();
    }

    // LivePhotoMode::Skip: skip live photo assets entirely (both image and MOV)
    let is_live_photo = asset.is_live_photo();
    if config.live_photo_mode == LivePhotoMode::Skip && is_live_photo {
        tracing::debug!(asset_id = %asset.id(), "Skipping live photo (live_photo_mode=skip)");
        return SmallVec::new();
    }

    let created_utc = asset.created();
    if let Some(before) = &config.skip_created_before {
        if created_utc < *before {
            tracing::debug!(asset_id = %asset.id(), date = %created_utc, "Skipping (before date range)");
            return SmallVec::new();
        }
    }
    if let Some(after) = &config.skip_created_after {
        if created_utc > *after {
            tracing::debug!(asset_id = %asset.id(), date = %created_utc, "Skipping (after date range)");
            return SmallVec::new();
        }
    }

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
        &fallback_filename
    };

    // Filename exclusion: match raw filename against glob patterns before any
    // cleaning or unicode stripping, so patterns match the original iCloud name.
    if config
        .filename_exclude
        .iter()
        .any(|p| p.matches_with(raw_filename, GLOB_CASE_INSENSITIVE))
    {
        tracing::debug!(asset_id = %asset.id(), filename = raw_filename, "Skipping (filename_exclude match)");
        return SmallVec::new();
    }

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
        // Determine the final download path, applying size-based deduplication if needed.
        // Check both on-disk files AND in-flight downloads (claimed_paths) to handle
        // concurrent downloads of assets with the same filename.
        // Check for the file on disk, including AM/PM whitespace variants
        // (e.g., "1.40.01 PM.PNG" vs "1.40.01\u{202F}PM.PNG")
        let existing_with_size = dir_cache
            .file_size(&download_path)
            .map(|size| (download_path.clone(), size))
            .or_else(|| {
                let variant = dir_cache.find_ampm_variant(&download_path)?;
                let size = dir_cache.file_size(&variant).unwrap_or(0);
                Some((variant, size))
            });
        let final_path = if let Some((_, on_disk_size)) = existing_with_size {
            match config.file_match_policy {
                FileMatchPolicy::NameSizeDedupWithSuffix => {
                    if version.size > 0 && on_disk_size == version.size {
                        // Same size — likely already downloaded, skip.
                        tracing::info!(
                            asset_id = asset.id(),
                            path = %download_path.display(),
                            size = version.size,
                            "Skipping asset: file exists with same name and size"
                        );
                        None
                    } else {
                        // Different size — deduplicate by appending file size to filename.
                        let dedup_filename = paths::add_dedup_suffix(&filename, version.size);
                        let dedup_path = paths::local_download_path(
                            &config.directory,
                            &config.folder_structure,
                            &created_local,
                            &dedup_filename,
                            config.album_name.as_deref(),
                        );
                        // Use normalize() for lookup to avoid PathBuf clone
                        let dedup_key = NormalizedPath::normalize(&dedup_path);
                        if dir_cache.exists(&dedup_path)
                            || claimed_paths.contains_key(dedup_key.as_ref())
                        {
                            tracing::info!(
                                asset_id = asset.id(),
                                path = %dedup_path.display(),
                                "Skipping asset: dedup path already exists"
                            );
                            None
                        } else {
                            tracing::debug!(
                                path = %download_path.display(),
                                on_disk_size,
                                expected_size = version.size,
                                dedup_path = %dedup_path.display(),
                                "File collision: already exists with different size"
                            );
                            Some(dedup_path)
                        }
                    }
                }
                FileMatchPolicy::NameId7 => {
                    // name-id7 policy adds asset ID to ALL filenames, not just collisions.
                    // If the file exists, it's already downloaded, skip.
                    tracing::info!(
                        asset_id = asset.id(),
                        path = %download_path.display(),
                        "Skipping asset: file exists (name-id7)"
                    );
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
                    if version.size > 0 && claimed_size == version.size {
                        // Same size — likely duplicate asset, skip.
                        tracing::info!(
                            asset_id = asset.id(),
                            path = %download_path.display(),
                            size = version.size,
                            "Skipping asset: in-flight download has same name and size"
                        );
                        None
                    } else {
                        // Different size — deduplicate by appending file size to filename.
                        let dedup_filename = paths::add_dedup_suffix(&filename, version.size);
                        let dedup_path = paths::local_download_path(
                            &config.directory,
                            &config.folder_structure,
                            &created_local,
                            &dedup_filename,
                            config.album_name.as_deref(),
                        );
                        // Use normalize() for lookup to avoid PathBuf clone
                        let dedup_key = NormalizedPath::normalize(&dedup_path);
                        if dir_cache.exists(&dedup_path)
                            || claimed_paths.contains_key(dedup_key.as_ref())
                        {
                            tracing::info!(
                                asset_id = asset.id(),
                                path = %dedup_path.display(),
                                "Skipping asset: dedup path already claimed in-flight"
                            );
                            None
                        } else {
                            tracing::debug!(
                                path = %download_path.display(),
                                claimed_size,
                                expected_size = version.size,
                                dedup_path = %dedup_path.display(),
                                "In-flight collision: claimed with different size"
                            );
                            Some(dedup_path)
                        }
                    }
                }
                FileMatchPolicy::NameId7 => {
                    tracing::info!(
                        asset_id = asset.id(),
                        path = %download_path.display(),
                        "Skipping asset: path claimed in-flight (name-id7)"
                    );
                    None
                }
            }
        } else {
            Some(download_path.clone())
        };

        if let Some(path) = &final_path {
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
                url: version.url.clone(),
                download_path: path,
                checksum: version.checksum.clone(),
                asset_id: asset.id().into(),
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
            // If the path already exists (on disk or claimed), it may be a different
            // file (e.g. a regular video) that collides with the live photo companion
            // name. Detect this by comparing sizes; on mismatch, deduplicate using
            // the asset ID.
            //
            // Use normalized paths for collision detection to handle case-insensitive
            // filesystems (macOS, Windows) where IMG.mov and IMG.MOV are the same file.
            let mov_key = NormalizedPath::normalize(&mov_path);
            let final_mov_path = if let Some(on_disk_size) = dir_cache.file_size(&mov_path) {
                if on_disk_size == live_version.size {
                    // Same size — likely already downloaded, skip.
                    tracing::info!(
                        asset_id = asset.id(),
                        path = %mov_path.display(),
                        "Skipping live photo MOV: already exists with same size"
                    );
                    None
                } else {
                    // Collision with a different file — deduplicate.
                    let dedup_filename = paths::insert_suffix(&mov_filename, asset.id());
                    let dedup_path = paths::local_download_path(
                        &config.directory,
                        &config.folder_structure,
                        &created_local,
                        &dedup_filename,
                        config.album_name.as_deref(),
                    );
                    let dedup_key = NormalizedPath::normalize(&dedup_path);
                    if dir_cache.exists(&dedup_path)
                        || claimed_paths.contains_key(dedup_key.as_ref())
                    {
                        tracing::info!(
                            asset_id = asset.id(),
                            path = %dedup_path.display(),
                            "Skipping live photo MOV: dedup path already exists"
                        );
                        None
                    } else {
                        tracing::debug!(
                            path = %mov_path.display(),
                            dedup_path = %dedup_path.display(),
                            "Live photo MOV collision: already exists with different size"
                        );
                        Some(dedup_path)
                    }
                }
            } else if let Some(&claimed_size) = claimed_paths.get(mov_key.as_ref()) {
                // Path is claimed by an in-flight download
                if claimed_size == live_version.size {
                    tracing::info!(
                        asset_id = asset.id(),
                        path = %mov_path.display(),
                        "Skipping live photo MOV: in-flight with same size"
                    );
                    None
                } else {
                    // Collision with in-flight download — deduplicate.
                    let dedup_filename = paths::insert_suffix(&mov_filename, asset.id());
                    let dedup_path = paths::local_download_path(
                        &config.directory,
                        &config.folder_structure,
                        &created_local,
                        &dedup_filename,
                        config.album_name.as_deref(),
                    );
                    let dedup_key = NormalizedPath::normalize(&dedup_path);
                    if dir_cache.exists(&dedup_path)
                        || claimed_paths.contains_key(dedup_key.as_ref())
                    {
                        tracing::info!(
                            asset_id = asset.id(),
                            path = %dedup_path.display(),
                            "Skipping live photo MOV: dedup path already claimed in-flight"
                        );
                        None
                    } else {
                        tracing::debug!(
                            path = %mov_path.display(),
                            dedup_path = %dedup_path.display(),
                            "Live photo MOV in-flight collision"
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
                    url: live_version.url.clone(),
                    download_path: path,
                    checksum: live_version.checksum.clone(),
                    asset_id: asset.id().into(),
                    size: live_version.size,
                    created_local,
                    version_size: VersionSizeKey::from(effective_live_size),
                });
            }
        }
    }

    tasks
}

/// Create a progress bar with a consistent template.
///
/// Returns `ProgressBar::hidden()` when the user passed `--no-progress-bar`,
/// `--only-print-filenames`, or stdout is not a TTY (e.g. piped output, cron
/// jobs) — this prevents output corruption and honours the user's preference.
fn create_progress_bar(
    no_progress_bar: bool,
    only_print_filenames: bool,
    total: u64,
) -> ProgressBar {
    if no_progress_bar || only_print_filenames || !std::io::stdout().is_terminal() {
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

/// A successful download whose state write to SQLite failed on first attempt.
/// Accumulated during the download loop and retried in a final flush.
#[derive(Debug)]
struct PendingStateWrite {
    asset_id: Box<str>,
    version_size: VersionSizeKey,
    download_path: PathBuf,
    local_checksum: String,
    download_checksum: Option<String>,
}

/// Maximum retry attempts for deferred state writes.
const STATE_WRITE_MAX_RETRIES: u32 = 6;
const _: () = assert!(STATE_WRITE_MAX_RETRIES <= 32, "shift overflow in backoff");

/// Retry all pending state writes that failed during the download loop.
///
/// Each write is attempted up to [`STATE_WRITE_MAX_RETRIES`] times with
/// exponential backoff (200ms, 400ms, 800ms, 1.6s, 3.2s between attempts
/// 1–5; attempt 6 fails immediately). SQLite lock contention is transient,
/// so generous retries prevent files from ending up on disk but untracked
/// in the state DB.
/// Returns the number of writes that still failed after all retries.
async fn flush_pending_state_writes(db: &dyn StateDb, pending: &[PendingStateWrite]) -> usize {
    if pending.is_empty() {
        return 0;
    }
    tracing::info!(count = pending.len(), "Retrying deferred state writes");
    let mut failures = 0;
    for write in pending {
        let mut succeeded = false;
        for attempt in 1..=STATE_WRITE_MAX_RETRIES {
            match db
                .mark_downloaded(
                    &write.asset_id,
                    write.version_size.as_str(),
                    &write.download_path,
                    &write.local_checksum,
                    write.download_checksum.as_deref(),
                )
                .await
            {
                Ok(()) => {
                    succeeded = true;
                    break;
                }
                Err(e) => {
                    if attempt < STATE_WRITE_MAX_RETRIES {
                        tracing::debug!(
                            asset_id = %write.asset_id,
                            attempt,
                            error = %e,
                            "State write retry failed, will retry"
                        );
                        tokio::time::sleep(Duration::from_millis(
                            200 * u64::from(1u32 << (attempt - 1)),
                        ))
                        .await;
                    } else {
                        tracing::error!(
                            asset_id = %write.asset_id,
                            path = %write.download_path.display(),
                            error = %e,
                            "State write failed after {STATE_WRITE_MAX_RETRIES} attempts — \
                             file on disk but untracked; next sync will detect it via \
                             filesystem check and skip re-download"
                        );
                    }
                }
            }
        }
        if !succeeded {
            failures += 1;
        }
    }
    if failures > 0 {
        tracing::warn!(
            failures,
            total = pending.len(),
            "Some state writes could not be saved"
        );
    } else {
        tracing::info!(count = pending.len(), "All deferred state writes recovered");
    }
    failures
}

/// Result of the streaming download phase.
#[derive(Debug)]
struct StreamingResult {
    downloaded: usize,
    exif_failures: usize,
    failed: Vec<DownloadTask>,
    auth_errors: usize,
    state_write_failures: usize,
    enumeration_errors: usize,
    assets_seen: u64,
}

/// Download photos with syncToken support.
///
/// In `SyncMode::Full`: runs the existing full enumeration via
/// `photo_stream_with_token`, captures the syncToken after the stream is
/// consumed, and delegates download logic to the existing pipeline.
///
/// In `SyncMode::Incremental`: uses `changes_stream` for delta sync,
/// filters `ChangeEvent`s to downloadable assets, and feeds them through
/// the existing download pipeline. Falls back to `SyncMode::Full` if the
/// token is invalid or expired.
/// Remove orphaned `.part` files from the download directory.
///
/// Scans the download directory for files ending with `temp_suffix` that are
/// older than the last completed sync. These are leftovers from interrupted
/// downloads that will never be resumed (new downloads produce fresh .part files).
async fn cleanup_orphan_part_files(config: &DownloadConfig) {
    let Some(db) = &config.state_db else { return };
    let cutoff = match db.get_summary().await {
        Ok(summary) => match summary.last_sync_completed {
            Some(ts) => ts,
            None => return, // No prior sync — nothing is orphaned
        },
        Err(e) => {
            tracing::debug!(error = %e, "Could not query last sync time for .part cleanup");
            return;
        }
    };

    let dir = &config.directory;
    if !dir.exists() {
        return;
    }

    let suffix = config.temp_suffix.clone();
    let dir = dir.clone();
    let cutoff_secs = cutoff.timestamp();

    let cleaned = tokio::task::spawn_blocking(move || {
        let mut cleaned = 0usize;
        let mut stack = vec![dir];
        while let Some(current) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&current) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    if name.ends_with(&suffix) {
                        if let Ok(meta) = path.metadata() {
                            if let Ok(mtime) = meta.modified() {
                                let mtime_secs = mtime
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map(|d| d.as_secs() as i64)
                                    .unwrap_or(0);
                                if mtime_secs < cutoff_secs && std::fs::remove_file(&path).is_ok() {
                                    cleaned += 1;
                                }
                            }
                        }
                    }
                }
            }
        }
        cleaned
    })
    .await
    .unwrap_or(0);

    if cleaned > 0 {
        tracing::info!(count = cleaned, "Cleaned up orphaned .part files");
    }
}

pub async fn download_photos_with_sync(
    download_client: &Client,
    albums: &[PhotoAlbum],
    config: Arc<DownloadConfig>,
    shutdown_token: CancellationToken,
) -> Result<SyncResult> {
    cleanup_orphan_part_files(&config).await;

    match &config.sync_mode {
        SyncMode::Full => {
            download_photos_full_with_token(download_client, albums, &config, shutdown_token).await
        }
        SyncMode::Incremental { zone_sync_token } => {
            let token = zone_sync_token.clone();
            match download_photos_incremental(
                download_client,
                albums,
                &config,
                &token,
                shutdown_token.clone(),
            )
            .await
            {
                Ok(result) => Ok(result),
                Err(e) => {
                    // Determine whether this error warrants a fallback to full
                    // enumeration. Token-level errors (invalid, zone not found)
                    // always trigger fallback. Transient errors (503, network
                    // timeouts) should NOT — they'd fail again on full enum too.
                    // Deserialization errors (e.g. Apple returning a different
                    // JSON shape for an invalid token) are not transient, so
                    // fall back for those too.
                    let is_token_error = e
                        .downcast_ref::<SyncTokenError>()
                        .is_some_and(SyncTokenError::should_fallback_to_full);
                    let is_transient = e.downcast_ref::<crate::auth::error::AuthError>().is_some()
                        || e.downcast_ref::<reqwest::Error>().is_some_and(|r| {
                            r.status().is_some_and(|s| s == 429 || s.as_u16() >= 500)
                                || r.is_timeout()
                                || r.is_connect()
                        });

                    if is_token_error || !is_transient {
                        tracing::warn!(
                            error = %e,
                            "Incremental sync failed, falling back to full enumeration"
                        );
                        download_photos_full_with_token(
                            download_client,
                            albums,
                            &config,
                            shutdown_token,
                        )
                        .await
                    } else {
                        Err(e)
                    }
                }
            }
        }
    }
}

/// Full enumeration with syncToken capture.
///
/// Uses `photo_stream_with_token` to capture the zone-level syncToken
/// while running the standard streaming download pipeline. The token
/// is returned alongside the download outcome.
async fn download_photos_full_with_token(
    download_client: &Client,
    albums: &[PhotoAlbum],
    config: &Arc<DownloadConfig>,
    shutdown_token: CancellationToken,
) -> Result<SyncResult> {
    let started = Instant::now();
    let uses_album_token = config.folder_structure.contains("{album}");

    // Build token-aware streams for each album
    let mut album_counts: Vec<u64> = Vec::with_capacity(albums.len());
    for album in albums {
        album_counts.push(album.len().await.unwrap_or(0));
    }
    let mut total: u64 = album_counts.iter().sum();
    if let Some(recent) = config.recent {
        total = total.min(u64::from(recent));
    }

    // When {album} is in folder_structure, process each album separately so that
    // the album name can be threaded into path expansion. Otherwise, merge all
    // album streams for maximum download concurrency across albums.
    let (streaming_result, token_receivers) = if uses_album_token {
        let mut combined_result = StreamingResult {
            downloaded: 0,
            exif_failures: 0,
            failed: Vec::new(),
            auth_errors: 0,
            state_write_failures: 0,
            enumeration_errors: 0,
            assets_seen: 0,
        };
        let mut token_receivers = Vec::with_capacity(albums.len());

        // When {album} is in folder_structure, albums are processed sequentially
        // so each gets its own per-album path expansion. Cross-album download
        // concurrency is intentionally sacrificed for correct path placement.
        // Assets appearing in multiple albums are downloaded once per album,
        // each to its respective album directory.
        for (album, &count) in albums.iter().zip(&album_counts) {
            if shutdown_token.is_cancelled() {
                break;
            }
            let album_config = Arc::new(config.with_album_name(album.name.clone()));

            let (stream, token_rx) = album.photo_stream_with_token(
                config.recent,
                Some(count),
                config.concurrent_downloads,
            );
            token_receivers.push(token_rx);

            let result = stream_and_download_from_stream(
                download_client,
                stream,
                &album_config,
                total,
                shutdown_token.clone(),
            )
            .await?;

            combined_result.downloaded += result.downloaded;
            combined_result.exif_failures += result.exif_failures;
            combined_result.failed.extend(result.failed);
            combined_result.auth_errors += result.auth_errors;
            combined_result.state_write_failures += result.state_write_failures;
            combined_result.enumeration_errors += result.enumeration_errors;
            combined_result.assets_seen += result.assets_seen;
        }

        (combined_result, token_receivers)
    } else {
        let mut token_receivers = Vec::with_capacity(albums.len());
        let streams: Vec<_> = albums
            .iter()
            .zip(&album_counts)
            .map(|(album, &count)| {
                let (stream, token_rx) = album.photo_stream_with_token(
                    config.recent,
                    Some(count),
                    config.concurrent_downloads,
                );
                token_receivers.push(token_rx);
                stream
            })
            .collect();

        let combined = stream::select_all(streams);
        let result = stream_and_download_from_stream(
            download_client,
            combined,
            config,
            total,
            shutdown_token.clone(),
        )
        .await?;

        (result, token_receivers)
    };

    // Warn if enumeration saw significantly fewer assets than the API reported.
    // This catches silent pagination truncation, dropped pages, or API hiccups
    // that would otherwise go unnoticed.
    if total > 0 && !config.only_print_filenames && !config.dry_run {
        let seen = streaming_result.assets_seen;
        let threshold = total * 95 / 100; // 5% tolerance
        if seen < threshold {
            tracing::warn!(
                expected = total,
                seen,
                "Enumeration saw fewer assets than expected — consider running a full re-sync"
            );
        }
    }

    // Collect the sync token from any album's token receiver.
    // In practice, all albums share the same zone so any token suffices.
    // Don't advance the token for read-only operations like --only-print-filenames.
    let mut sync_token = None;
    if !config.only_print_filenames {
        for rx in token_receivers {
            if let Ok(Some(token)) = rx.await {
                sync_token = Some(token);
                break;
            }
        }
    }

    // Build the outcome using the same logic as download_photos
    let outcome = build_download_outcome(
        download_client,
        albums,
        config,
        streaming_result,
        started,
        shutdown_token,
    )
    .await?;

    Ok(SyncResult {
        outcome,
        sync_token,
    })
}

/// Incremental delta sync via `changes_stream`.
///
/// Fetches `ChangeEvent`s since the given sync token, filters to
/// downloadable assets, and feeds them through the download pipeline.
async fn download_photos_incremental(
    download_client: &Client,
    albums: &[PhotoAlbum],
    config: &Arc<DownloadConfig>,
    zone_sync_token: &str,
    shutdown_token: CancellationToken,
) -> Result<SyncResult> {
    let started = Instant::now();
    let uses_album_token = config.folder_structure.contains("{album}");

    // Collect change events from all albums, counting and filtering in a single pass.
    // Each asset is paired with its source album name so that {album} token
    // expansion works correctly in the incremental path.
    let mut downloadable_assets: Vec<(PhotoAsset, Arc<str>)> = Vec::new();
    let mut sync_token: Option<String> = None;
    let mut created_count = 0u64;
    let mut soft_deleted_count = 0u64;
    let mut hard_deleted_count = 0u64;
    let mut hidden_count = 0u64;
    let mut total_events = 0u64;

    for album in albums {
        let (change_stream, token_rx) = album.changes_stream(zone_sync_token);
        tokio::pin!(change_stream);

        while let Some(result) = change_stream.next().await {
            if shutdown_token.is_cancelled() {
                break;
            }
            let event = result?;
            total_events += 1;
            match event.reason {
                ChangeReason::Created => {
                    created_count += 1;
                    if let Some(asset) = event.asset {
                        downloadable_assets.push((asset, Arc::clone(&album.name)));
                    }
                }
                ChangeReason::SoftDeleted => {
                    soft_deleted_count += 1;
                    tracing::debug!(record_name = %event.record_name, record_type = ?event.record_type, "Skipping soft-deleted record");
                }
                ChangeReason::HardDeleted => {
                    hard_deleted_count += 1;
                    tracing::debug!(record_name = %event.record_name, record_type = ?event.record_type, "Skipping hard-deleted record");
                }
                ChangeReason::Hidden => {
                    hidden_count += 1;
                    tracing::debug!(record_name = %event.record_name, record_type = ?event.record_type, "Skipping hidden record");
                }
            }
        }

        // Capture the sync token from this album
        if let Ok(token) = token_rx.await {
            sync_token = Some(token);
        }
    }

    tracing::info!(
        created = created_count,
        soft_deleted = soft_deleted_count,
        hard_deleted = hard_deleted_count,
        hidden = hidden_count,
        "Incremental sync: {total_events} change events",
    );

    if downloadable_assets.is_empty() {
        tracing::info!("No new photos to download from incremental sync");
        tracing::info!(elapsed = %format_duration(started.elapsed()), "  completed");
        return Ok(SyncResult {
            outcome: DownloadOutcome::Success,
            sync_token,
        });
    }

    // Respect --recent: cap the number of assets to download
    if let Some(recent) = config.recent {
        let limit = recent as usize;
        if downloadable_assets.len() > limit {
            tracing::info!(
                total = downloadable_assets.len(),
                limit,
                "Capping incremental assets to --recent limit"
            );
            downloadable_assets.truncate(limit);
        }
    }

    tracing::info!(
        count = downloadable_assets.len(),
        "Assets to download from incremental sync"
    );

    // Pre-load download context for O(1) state DB skip decisions
    let download_ctx = if let Some(db) = &config.state_db {
        DownloadContext::load(db.as_ref(), false).await
    } else {
        DownloadContext::default()
    };

    // Convert assets to download tasks, using state DB fast-skip where possible.
    // When {album} is in folder_structure, create per-album configs so the album
    // name is threaded into path expansion (mirrors full sync behaviour).
    let mut tasks: Vec<DownloadTask> = Vec::new();
    let mut claimed_paths: FxHashMap<NormalizedPath, u64> = FxHashMap::default();
    let mut dir_cache = paths::DirCache::new();
    let mut skipped_by_state = 0usize;
    let mut album_configs: FxHashMap<Arc<str>, Arc<DownloadConfig>> = FxHashMap::default();

    // In {album} mode, assets in multiple albums are processed once per album,
    // each downloading to the album-specific directory. Configs are cached per
    // album name to avoid redundant allocations.
    for (asset, album_name) in &downloadable_assets {
        let effective_config: &Arc<DownloadConfig> = if uses_album_token {
            album_configs
                .entry(Arc::clone(album_name))
                .or_insert_with(|| Arc::new(config.with_album_name(Arc::clone(album_name))))
        } else {
            config
        };

        // Fast-skip: if state DB confirms all versions are already downloaded
        // with matching checksums, skip the filesystem check entirely.
        let candidates = extract_skip_candidates(asset, effective_config);
        if !candidates.is_empty()
            && candidates.iter().all(|&(vs, cs)| {
                matches!(
                    download_ctx.should_download_fast(asset.id(), vs, cs, true),
                    Some(false)
                )
            })
        {
            skipped_by_state += 1;
            continue;
        }

        pre_ensure_asset_dir(&mut dir_cache, asset, effective_config).await;
        let asset_tasks =
            filter_asset_to_tasks(asset, effective_config, &mut claimed_paths, &mut dir_cache);

        // Upsert state records so mark_downloaded/mark_failed can find them.
        // Without this, the UPDATE in mark_downloaded matches 0 rows and the
        // file ends up on disk but untracked in the state DB.
        if let Some(db) = &config.state_db {
            for task in &asset_tasks {
                let media_type = determine_media_type(task.version_size, asset);
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
                        asset_id = %task.asset_id,
                        error = %e,
                        "Failed to record asset in state DB"
                    );
                }
            }
        }

        tasks.extend(asset_tasks);
    }

    if skipped_by_state > 0 {
        tracing::info!(
            skipped = skipped_by_state,
            "Skipped already-downloaded assets (state DB)"
        );
    }

    if tasks.is_empty() {
        tracing::info!("All incremental assets already downloaded or filtered");
        tracing::info!(elapsed = %format_duration(started.elapsed()), "  completed");
        return Ok(SyncResult {
            outcome: DownloadOutcome::Success,
            sync_token,
        });
    }

    if config.only_print_filenames {
        for task in &tasks {
            println!("{}", task.download_path.display());
        }
        // Don't advance the sync token — this is a read-only operation.
        return Ok(SyncResult {
            outcome: DownloadOutcome::Success,
            sync_token: None,
        });
    }

    let task_count = tasks.len();
    tracing::info!(
        count = task_count,
        "Downloading files from incremental sync"
    );

    // Run the download pass on the collected tasks
    let pass_config = PassConfig {
        client: download_client,
        retry_config: &config.retry,
        set_exif: config.set_exif_datetime,
        concurrency: config.concurrent_downloads,
        no_progress_bar: config.no_progress_bar,
        temp_suffix: config.temp_suffix.clone(),
        shutdown_token,
        state_db: config.state_db.clone(),
    };
    let pass_result = run_download_pass(pass_config, tasks).await;

    let failed = pass_result.failed.len();
    let succeeded = task_count - failed;

    tracing::info!("── Incremental Sync Summary ──");
    if pass_result.exif_failures > 0 || pass_result.state_write_failures > 0 {
        tracing::info!(
            downloaded = succeeded,
            exif_failures = pass_result.exif_failures,
            state_write_failures = pass_result.state_write_failures,
            failed,
            total = task_count,
            "  sync results"
        );
    } else {
        tracing::info!(
            downloaded = succeeded,
            failed,
            total = task_count,
            "  sync results"
        );
    }
    tracing::info!(elapsed = %format_duration(started.elapsed()), "  completed");

    if pass_result.auth_errors >= AUTH_ERROR_THRESHOLD {
        return Ok(SyncResult {
            outcome: DownloadOutcome::SessionExpired {
                auth_error_count: pass_result.auth_errors,
            },
            sync_token,
        });
    }

    let outcome = if failed > 0
        || pass_result.exif_failures > 0
        || pass_result.state_write_failures > 0
    {
        for task in &pass_result.failed {
            tracing::error!(asset_id = %task.asset_id, path = %task.download_path.display(), "Download failed");
        }
        DownloadOutcome::PartialFailure {
            failed_count: failed + pass_result.exif_failures + pass_result.state_write_failures,
        }
    } else {
        DownloadOutcome::Success
    };

    Ok(SyncResult {
        outcome,
        sync_token,
    })
}

/// Streaming download pipeline that consumes a pre-built combined stream.
///
/// This is the core producer/consumer download logic from `stream_and_download`,
/// factored out so that `download_photos_full_with_token` can supply a
/// token-aware combined stream while reusing the same download machinery.
async fn stream_and_download_from_stream<S>(
    download_client: &Client,
    combined: S,
    config: &Arc<DownloadConfig>,
    total: u64,
    shutdown_token: CancellationToken,
) -> Result<StreamingResult>
where
    S: futures_util::Stream<Item = anyhow::Result<crate::icloud::photos::PhotoAsset>>
        + Send
        + 'static,
{
    let pb = create_progress_bar(config.no_progress_bar, config.only_print_filenames, total);

    if config.only_print_filenames {
        // Load state DB context so we skip already-downloaded assets,
        // matching the incremental path's behavior.
        let download_ctx = if let Some(db) = &config.state_db {
            DownloadContext::load(db.as_ref(), false).await
        } else {
            DownloadContext::default()
        };

        tokio::pin!(combined);
        let mut claimed_paths: FxHashMap<NormalizedPath, u64> = FxHashMap::default();
        let mut dir_cache = paths::DirCache::new();
        while let Some(result) = combined.next().await {
            if shutdown_token.is_cancelled() {
                break;
            }
            match result {
                Ok(asset) => {
                    let candidates = extract_skip_candidates(&asset, config);
                    if !candidates.is_empty()
                        && candidates.iter().all(|&(vs, cs)| {
                            matches!(
                                download_ctx.should_download_fast(asset.id(), vs, cs, true),
                                Some(false)
                            )
                        })
                    {
                        continue;
                    }

                    pre_ensure_asset_dir(&mut dir_cache, &asset, config).await;
                    let tasks =
                        filter_asset_to_tasks(&asset, config, &mut claimed_paths, &mut dir_cache);
                    for task in &tasks {
                        println!("{}", task.download_path.display());
                    }
                }
                Err(e) => {
                    tracing::error!(error = %e, "Error fetching asset");
                }
            }
        }
        return Ok(StreamingResult {
            downloaded: 0,
            exif_failures: 0,
            failed: Vec::new(),
            auth_errors: 0,
            state_write_failures: 0,
            enumeration_errors: 0,
            assets_seen: 0,
        });
    }

    if config.dry_run {
        tokio::pin!(combined);
        let mut count = 0usize;
        let mut claimed_paths: FxHashMap<NormalizedPath, u64> = FxHashMap::default();
        let mut dir_cache = paths::DirCache::new();
        while let Some(result) = combined.next().await {
            if shutdown_token.is_cancelled() {
                tracing::info!("Shutdown requested, stopping dry run");
                break;
            }
            let asset = result?;
            pre_ensure_asset_dir(&mut dir_cache, &asset, config).await;
            let tasks = filter_asset_to_tasks(&asset, config, &mut claimed_paths, &mut dir_cache);
            for task in &tasks {
                tracing::info!(path = %task.download_path.display(), "[DRY RUN] Would download");
            }
            count += tasks.len();
        }
        return Ok(StreamingResult {
            downloaded: count,
            exif_failures: 0,
            failed: Vec::new(),
            auth_errors: 0,
            state_write_failures: 0,
            enumeration_errors: 0,
            assets_seen: 0,
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
        DownloadContext::load(db.as_ref(), config.retry_only).await
    } else {
        DownloadContext::default()
    };
    tracing::debug!(
        downloaded_ids = download_ctx.downloaded_ids.len(),
        "Download context loaded"
    );

    // Determine if we can trust the state DB for early skips
    let trust_state = if let Some(db) = &state_db {
        let config_hash = hash_download_config(config);
        let stored_hash = db.get_metadata("config_hash").await.unwrap_or(None);
        let mut trust = stored_hash.as_deref() == Some(&config_hash);
        if !trust {
            if stored_hash.is_some() {
                tracing::info!("Download config changed since last sync, verifying all files");
                // Clear stored sync tokens so the next cycle/run falls back to
                // full enumeration, picking up assets that the old incremental
                // token would have missed under the new filter settings.
                match db.delete_metadata_by_prefix("sync_token:").await {
                    Ok(n) if n > 0 => {
                        tracing::info!(cleared = n, "Cleared stale sync tokens");
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "Failed to clear sync tokens");
                    }
                    _ => {}
                }
            }
            if let Err(e) = db.set_metadata("config_hash", &config_hash).await {
                tracing::warn!(error = %e, "Failed to persist config_hash");
            }
        }
        trust = trust && !download_ctx.downloaded_ids.is_empty();

        // Sample-check that "downloaded" files still exist on disk
        if trust {
            let sample_count = download_ctx
                .downloaded_ids
                .len()
                .div_ceil(20) // ~5% sample
                .clamp(5, 500);
            match db.sample_downloaded_paths(sample_count).await {
                Ok(paths) => {
                    let missing: Vec<_> = paths.iter().filter(|p| !p.exists()).collect();
                    if !missing.is_empty() {
                        tracing::warn!(
                            sampled = paths.len(),
                            missing = missing.len(),
                            "Sample check found missing files, disabling trust-state"
                        );
                        for p in &missing {
                            tracing::debug!(path = %p.display(), "Missing downloaded file");
                        }
                        trust = false;
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to sample downloaded paths, disabling trust-state");
                    trust = false;
                }
            }
        }
        trust
    } else {
        false
    };
    if trust_state {
        tracing::debug!(
            "Trust-state mode active: skipping filesystem checks for DB-confirmed assets"
        );
    }

    // Start sync run tracking
    let sync_run_id = if let Some(db) = &state_db {
        match db.start_sync_run().await {
            Ok(id) => {
                tracing::debug!(run_id = id, "Started sync run");
                Some(id)
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to start sync run tracking");
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
    let mut pending_state_writes: Vec<PendingStateWrite> = Vec::new();

    let (task_tx, task_rx) = mpsc::channel::<DownloadTask>(concurrency * 2);

    let assets_seen = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let assets_seen_producer = Arc::clone(&assets_seen);
    let enum_errors = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let enum_errors_producer = Arc::clone(&enum_errors);

    let producer_config = Arc::clone(config);
    let producer_state_db = state_db.clone();
    let producer_shutdown = shutdown_token.clone();
    let producer_pb = pb.clone();
    let producer = tokio::spawn(async move {
        let config = &producer_config;
        let mut claimed_paths: FxHashMap<NormalizedPath, u64> = FxHashMap::default();
        let mut dir_cache = paths::DirCache::new();
        let mut seen_ids: FxHashSet<Box<str>> = FxHashSet::default();
        let mut skipped_by_state = 0usize;
        let mut skipped_on_disk = 0usize;
        let mut skipped_ampm = 0usize;
        let mut skipped_by_filter = 0usize;
        tokio::pin!(combined);
        while let Some(result) = combined.next().await {
            if producer_shutdown.is_cancelled() {
                break;
            }
            match result {
                Ok(asset) => {
                    if !seen_ids.insert(asset.id().into()) {
                        tracing::warn!(
                            asset_id = %asset.id(),
                            "Duplicate asset ID from API, skipping"
                        );
                        producer_pb.inc(1);
                        continue;
                    }

                    assets_seen_producer.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

                    if trust_state {
                        let candidates = extract_skip_candidates(&asset, config);
                        if !candidates.is_empty()
                            && candidates.iter().all(|&(vs, cs)| {
                                matches!(
                                    download_ctx.should_download_fast(asset.id(), vs, cs, true),
                                    Some(false)
                                )
                            })
                        {
                            if let Some(db) = &producer_state_db {
                                if let Err(e) = db.touch_last_seen(asset.id()).await {
                                    tracing::debug!(error = %e, asset_id = asset.id(), "Failed to update last-seen timestamp");
                                }
                            }
                            skipped_by_state += 1;
                            producer_pb.inc(1);
                            continue;
                        }
                    }

                    pre_ensure_asset_dir(&mut dir_cache, &asset, config).await;

                    let tasks =
                        filter_asset_to_tasks(&asset, config, &mut claimed_paths, &mut dir_cache);
                    if tasks.is_empty() {
                        skipped_by_filter += 1;
                        producer_pb.inc(1);
                    } else {
                        for task in tasks {
                            // Skip assets that have exceeded the retry limit.
                            if let Some(&attempts) =
                                download_ctx.attempt_counts.get(task.asset_id.as_ref())
                            {
                                if config.max_download_attempts > 0
                                    && attempts >= config.max_download_attempts
                                {
                                    tracing::warn!(
                                        asset_id = %task.asset_id,
                                        attempts,
                                        max = config.max_download_attempts,
                                        "Skipping asset: exceeded max download attempts"
                                    );
                                    continue;
                                }
                            }

                            if config.retry_only
                                && !download_ctx.known_ids.contains(task.asset_id.as_ref())
                            {
                                tracing::debug!(
                                    asset_id = %task.asset_id,
                                    "Skipping new asset in retry-only mode"
                                );
                                continue;
                            }

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
                                        asset_id = %task.asset_id,
                                        error = %e,
                                        "Failed to record asset"
                                    );
                                }

                                match download_ctx.should_download_fast(
                                    &task.asset_id,
                                    task.version_size,
                                    &task.checksum,
                                    false,
                                ) {
                                    Some(true) => {
                                        if task_tx.send(task).await.is_err() {
                                            return;
                                        }
                                    }
                                    Some(false) => {
                                        skipped_by_state += 1;
                                        tracing::debug!(
                                            asset_id = %task.asset_id,
                                            "Skipping (state confirms no download needed)"
                                        );
                                    }
                                    None => {
                                        // Directory was pre-populated above, so these
                                        // are cache-hits — no blocking I/O.
                                        if dir_cache.exists(&task.download_path) {
                                            skipped_on_disk += 1;
                                            tracing::debug!(
                                                asset_id = %task.asset_id,
                                                path = %task.download_path.display(),
                                                "Skipping (already downloaded)"
                                            );
                                        } else if dir_cache
                                            .find_ampm_variant(&task.download_path)
                                            .is_some()
                                        {
                                            skipped_ampm += 1;
                                            tracing::debug!(
                                                asset_id = %task.asset_id,
                                                path = %task.download_path.display(),
                                                "Skipping (AM/PM variant exists on disk)"
                                            );
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
                                }
                            } else if task_tx.send(task).await.is_err() {
                                return;
                            }
                        }
                        producer_pb.inc(1);
                    }
                }
                Err(e) => {
                    enum_errors_producer.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    producer_pb.suspend(|| tracing::error!(error = %e, "Error fetching asset"));
                }
            }
        }

        let total_skipped = skipped_by_state + skipped_on_disk + skipped_ampm + skipped_by_filter;
        if total_skipped > 0 {
            producer_pb.suspend(|| {
                tracing::info!(
                    state = skipped_by_state,
                    on_disk = skipped_on_disk,
                    ampm_variant = skipped_ampm,
                    filtered = skipped_by_filter,
                    total = total_skipped,
                    "Skipped assets"
                );
            });
        }
    });

    // Convert channel receiver to stream and feed into buffer_unordered
    let temp_suffix: Arc<str> = config.temp_suffix.clone().into();
    let download_stream = ReceiverStream::new(task_rx)
        .map(|task| {
            let client = download_client.clone();
            let temp_suffix = Arc::clone(&temp_suffix);
            async move {
                let result = Box::pin(download_single_task(
                    &client,
                    &task,
                    &retry_config,
                    set_exif,
                    &temp_suffix,
                ))
                .await;
                (task, result)
            }
        })
        .buffer_unordered(concurrency);

    tokio::pin!(download_stream);

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
            Ok((exif_ok, local_checksum, download_checksum)) => {
                downloaded += 1;
                if !exif_ok {
                    exif_failures += 1;
                }
                if let Some(db) = &state_db {
                    if let Err(e) = db
                        .mark_downloaded(
                            &task.asset_id,
                            task.version_size.as_str(),
                            &task.download_path,
                            &local_checksum,
                            download_checksum.as_deref(),
                        )
                        .await
                    {
                        pb.suspend(|| {
                            tracing::warn!(
                                asset_id = %task.asset_id,
                                error = %e,
                                "State write failed, deferring for retry"
                            );
                        });
                        pending_state_writes.push(PendingStateWrite {
                            asset_id: task.asset_id.clone(),
                            version_size: task.version_size,
                            download_path: task.download_path.clone(),
                            local_checksum,
                            download_checksum,
                        });
                    }
                }
            }
            Err(e) => {
                if let Some(download_err) = e.downcast_ref::<DownloadError>() {
                    if download_err.is_session_expired() {
                        auth_errors += 1;
                        pb.suspend(|| {
                            tracing::warn!(
                                auth_errors,
                                threshold = AUTH_ERROR_THRESHOLD,
                                path = %task.download_path.display(),
                                error = %e,
                                "Auth error"
                            );
                        });
                        if auth_errors >= AUTH_ERROR_THRESHOLD {
                            pb.suspend(|| {
                                tracing::warn!(
                                    "Auth error threshold reached, aborting for re-authentication"
                                );
                            });
                            break;
                        }
                    } else {
                        pb.suspend(|| {
                            tracing::error!(asset_id = %task.asset_id, path = %task.download_path.display(), error = %e, "Download failed");
                        });
                    }
                } else {
                    pb.suspend(|| {
                        tracing::error!(asset_id = %task.asset_id, path = %task.download_path.display(), error = %e, "Download failed");
                    });
                }
                if let Some(db) = &state_db {
                    if let Err(e) = db
                        .mark_failed(&task.asset_id, task.version_size.as_str(), &e.to_string())
                        .await
                    {
                        tracing::warn!(
                            asset_id = %task.asset_id,
                            error = %e,
                            "Failed to mark failure"
                        );
                    }
                }
                failed.push(task);
            }
        }
    }

    let producer_panicked = match producer.await {
        Err(e) if e.is_panic() => {
            tracing::error!(error = ?e, "Asset producer task panicked");
            true
        }
        _ => false,
    };

    let assets_seen_count = assets_seen.load(std::sync::atomic::Ordering::Relaxed);

    pb.finish_and_clear();

    if let (Some(db), Some(run_id)) = (&state_db, sync_run_id) {
        let stats = SyncRunStats {
            assets_seen: assets_seen_count,
            assets_downloaded: downloaded as u64,
            assets_failed: failed.len() as u64,
            interrupted: shutdown_token.is_cancelled()
                || auth_errors >= AUTH_ERROR_THRESHOLD
                || producer_panicked,
        };
        if let Err(e) = db.complete_sync_run(run_id, &stats).await {
            tracing::warn!(error = %e, "Failed to complete sync run tracking");
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

    if producer_panicked {
        return Err(anyhow::anyhow!(
            "Asset producer panicked — sync may be incomplete"
        ));
    }

    // Retry any state writes that failed during the streaming loop
    let state_write_failures = if let Some(db) = &state_db {
        flush_pending_state_writes(db.as_ref(), &pending_state_writes).await
    } else {
        0
    };

    Ok(StreamingResult {
        downloaded,
        exif_failures,
        failed,
        auth_errors,
        state_write_failures,
        enumeration_errors: enum_errors.load(std::sync::atomic::Ordering::Relaxed),
        assets_seen: assets_seen_count,
    })
}

/// Build a `DownloadOutcome` from a `StreamingResult`, running a cleanup
/// pass if there were failures. Shared between `download_photos` and
/// `download_photos_full_with_token`.
async fn build_download_outcome(
    download_client: &Client,
    albums: &[PhotoAlbum],
    config: &Arc<DownloadConfig>,
    streaming_result: StreamingResult,
    started: Instant,
    shutdown_token: CancellationToken,
) -> Result<DownloadOutcome> {
    let downloaded = streaming_result.downloaded;
    let mut exif_failures = streaming_result.exif_failures;
    let failed_tasks = streaming_result.failed;
    let auth_errors = streaming_result.auth_errors;
    let mut state_write_failures = streaming_result.state_write_failures;
    let enumeration_errors = streaming_result.enumeration_errors;

    if auth_errors >= AUTH_ERROR_THRESHOLD {
        return Ok(DownloadOutcome::SessionExpired {
            auth_error_count: auth_errors,
        });
    }

    if downloaded == 0 && failed_tasks.is_empty() {
        if config.dry_run {
            tracing::info!("── Dry Run Summary ──");
            tracing::info!("  0 files would be downloaded");
            tracing::info!(destination = %config.directory.display(), "  destination");
        } else {
            tracing::info!("No new photos to download");
        }
        return Ok(DownloadOutcome::Success);
    }

    if config.dry_run {
        tracing::info!("── Dry Run Summary ──");
        if shutdown_token.is_cancelled() {
            tracing::info!(scanned = downloaded, "  Interrupted before shutdown");
        } else {
            tracing::info!(count = downloaded, "  files would be downloaded");
        }
        tracing::info!(destination = %config.directory.display(), "  destination");
        tracing::info!(concurrency = config.concurrent_downloads, "  concurrency");
        return Ok(DownloadOutcome::Success);
    }

    let total = downloaded + failed_tasks.len();

    if failed_tasks.is_empty() {
        tracing::info!("── Summary ──");
        if exif_failures > 0 || state_write_failures > 0 || enumeration_errors > 0 {
            tracing::info!(
                downloaded = total,
                exif_failures,
                state_write_failures,
                enumeration_errors,
                failed = 0,
                total,
                "  sync results"
            );
        } else {
            tracing::info!(downloaded = total, failed = 0, total, "  sync results");
        }
        tracing::info!(elapsed = %format_duration(started.elapsed()), "  completed");
        if state_write_failures > 0 || enumeration_errors > 0 || exif_failures > 0 {
            return Ok(DownloadOutcome::PartialFailure {
                failed_count: state_write_failures + enumeration_errors + exif_failures,
            });
        }
        return Ok(DownloadOutcome::Success);
    }

    // Phase 2: cleanup pass with fresh CDN URLs
    let cleanup_concurrency = 5;
    let failure_count = failed_tasks.len();
    tracing::info!(
        failure_count,
        concurrency = cleanup_concurrency,
        "── Cleanup pass: re-fetching URLs and retrying failed downloads ──"
    );

    let fresh_tasks = build_download_tasks(albums, config, shutdown_token.clone()).await?;
    tracing::info!(
        count = fresh_tasks.len(),
        "  Re-fetched tasks with fresh URLs"
    );

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
    state_write_failures += pass_result.state_write_failures;
    let total_auth_errors = auth_errors + phase2_auth_errors;

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
    if exif_failures > 0 || state_write_failures > 0 {
        tracing::info!(
            downloaded = succeeded,
            exif_failures,
            state_write_failures,
            failed,
            total = final_total,
            "  sync results"
        );
    } else {
        tracing::info!(
            downloaded = succeeded,
            failed,
            total = final_total,
            "  sync results"
        );
    }
    tracing::info!(elapsed = %format_duration(started.elapsed()), "  completed");

    let total_failures = failed + state_write_failures + exif_failures;
    if total_failures > 0 {
        for task in &remaining_failed {
            tracing::error!(asset_id = %task.asset_id, path = %task.download_path.display(), "Download failed");
        }
        return Ok(DownloadOutcome::PartialFailure {
            failed_count: total_failures,
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
    state_write_failures: usize,
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

impl std::fmt::Debug for PassConfig<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PassConfig")
            .field("set_exif", &self.set_exif)
            .field("concurrency", &self.concurrency)
            .field("no_progress_bar", &self.no_progress_bar)
            .field("temp_suffix", &self.temp_suffix)
            .field("state_db", &self.state_db.as_ref().map(|_| ".."))
            .finish_non_exhaustive()
    }
}

/// Execute a download pass over the given tasks, returning any that failed.
async fn run_download_pass(config: PassConfig<'_>, tasks: Vec<DownloadTask>) -> PassResult {
    let pb = create_progress_bar(config.no_progress_bar, false, tasks.len() as u64);
    let client = config.client.clone();
    let retry_config = config.retry_config;
    let set_exif = config.set_exif;
    let state_db = config.state_db.clone();
    let shutdown_token = config.shutdown_token.clone();
    let concurrency = config.concurrency;
    let temp_suffix: Arc<str> = config.temp_suffix.into();

    type DownloadResult = (DownloadTask, Result<(bool, String, Option<String>)>);
    let results: Vec<DownloadResult> = stream::iter(tasks)
        .take_while(|_| std::future::ready(!shutdown_token.is_cancelled()))
        .map(|task| {
            let client = client.clone();
            let temp_suffix = Arc::clone(&temp_suffix);
            async move {
                let result = Box::pin(download_single_task(
                    &client,
                    &task,
                    retry_config,
                    set_exif,
                    &temp_suffix,
                ))
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
    let mut pending_state_writes: Vec<PendingStateWrite> = Vec::new();

    for (task, result) in results {
        match &result {
            Ok((exif_ok, local_checksum, download_checksum)) => {
                if !*exif_ok {
                    exif_failures += 1;
                }
                if let Some(db) = &state_db {
                    if let Err(e) = db
                        .mark_downloaded(
                            &task.asset_id,
                            task.version_size.as_str(),
                            &task.download_path,
                            local_checksum,
                            download_checksum.as_deref(),
                        )
                        .await
                    {
                        pb.suspend(|| {
                            tracing::warn!(
                                asset_id = %task.asset_id,
                                error = %e,
                                "State write failed, deferring for retry"
                            );
                        });
                        pending_state_writes.push(PendingStateWrite {
                            asset_id: task.asset_id.clone(),
                            version_size: task.version_size,
                            download_path: task.download_path.clone(),
                            local_checksum: local_checksum.clone(),
                            download_checksum: download_checksum.clone(),
                        });
                    }
                }
            }
            Err(e) => {
                if let Some(download_err) = e.downcast_ref::<DownloadError>() {
                    if download_err.is_session_expired() {
                        auth_errors += 1;
                        pb.suspend(|| {
                            tracing::warn!(path = %task.download_path.display(), error = %e, "Auth error");
                        });
                    }
                } else {
                    pb.suspend(|| {
                        tracing::error!(asset_id = %task.asset_id, path = %task.download_path.display(), error = %e, "Download failed");
                    });
                }
                if let Some(db) = &state_db {
                    if let Err(e) = db
                        .mark_failed(&task.asset_id, task.version_size.as_str(), &e.to_string())
                        .await
                    {
                        tracing::warn!(
                            asset_id = %task.asset_id,
                            error = %e,
                            "Failed to mark failure"
                        );
                    }
                }
                failed.push(task);
            }
        }
        pb.inc(1);
    }

    // Retry any state writes that failed during the pass
    let state_write_failures = if let Some(db) = &state_db {
        flush_pending_state_writes(db.as_ref(), &pending_state_writes).await
    } else {
        0
    };

    pb.finish_and_clear();
    PassResult {
        exif_failures,
        failed,
        auth_errors,
        state_write_failures,
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
) -> Result<(bool, String, Option<String>)> {
    if let Some(parent) = task.download_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("failed to create directory {}", parent.display()))?;
    }

    tracing::debug!(
        size_bytes = task.size,
        path = %task.download_path.display(),
        "downloading",
    );

    // Determine if EXIF modification is needed so we can keep the .part file
    // around for modification before the atomic rename to the final path.
    let needs_exif = set_exif && {
        let ext = task
            .download_path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        matches!(ext.to_ascii_lowercase().as_str(), "jpg" | "jpeg")
    };

    Box::pin(file::download_file(
        client,
        &task.url,
        &task.download_path,
        &task.checksum,
        retry_config,
        temp_suffix,
        file::DownloadOpts {
            skip_rename: needs_exif,
            expected_size: if task.size > 0 { Some(task.size) } else { None },
        },
    ))
    .await?;

    // When EXIF is needed, modifications happen on the .part file before
    // the atomic rename, preventing silent corruption on power loss / SIGKILL.
    let part_path = if needs_exif {
        Some(
            file::temp_download_path(&task.download_path, &task.checksum, temp_suffix)
                .context("failed to compute part path")?,
        )
    } else {
        None
    };

    // Compute SHA-256 of the downloaded content before EXIF modification
    // so we store a hash that reflects the original download bytes.
    let download_checksum = if let Some(path) = &part_path {
        Some(file::compute_sha256(path).await?)
    } else {
        None
    };

    let mut exif_ok = true;
    if let Some(part) = &part_path {
        let exif_path = part.clone();
        let date_str = task.created_local.format("%Y:%m:%d %H:%M:%S").to_string();
        let exif_result =
            tokio::task::spawn_blocking(move || match exif::get_photo_exif(&exif_path) {
                Ok(None) => {
                    if let Err(e) = exif::set_photo_exif(&exif_path, &date_str) {
                        tracing::warn!(path = %exif_path.display(), error = %e, "Failed to set EXIF");
                        false
                    } else {
                        true
                    }
                }
                Ok(Some(_)) => true,
                Err(e) => {
                    tracing::warn!(path = %exif_path.display(), error = %e, "Failed to read EXIF");
                    false
                }
            })
            .await;
        match exif_result {
            Ok(ok) => exif_ok = ok,
            Err(e) => {
                tracing::warn!(error = %e, "EXIF task panicked");
                exif_ok = false;
            }
        }
    }

    // Set mtime on .part (before rename) or final path directly.
    // rename() preserves mtime so this works in both cases.
    let mtime_target = part_path
        .as_deref()
        .unwrap_or(&task.download_path)
        .to_path_buf();
    let ts = task.created_local.timestamp();
    if let Err(e) = tokio::task::spawn_blocking(move || set_file_mtime(&mtime_target, ts)).await? {
        tracing::warn!(
            path = %task.download_path.display(),
            error = %e,
            "Could not set mtime"
        );
    }

    // Atomic rename: .part → final (only when EXIF path was used)
    if let Some(part) = &part_path {
        file::rename_part_to_final(part, &task.download_path).await?;
    }

    tracing::debug!(path = %task.download_path.display(), "Downloaded");

    // Compute SHA-256 of the final file for local storage and verification.
    let local_checksum = file::compute_sha256(&task.download_path).await?;

    // Note: Apple's `fileChecksum` is an MMCS (MobileMe Chunked Storage)
    // compound signature, not a SHA-1/SHA-256 content hash. It cannot be
    // compared against a hash of the downloaded bytes.  Content integrity
    // is verified by size matching (Content-Length + API size field) and
    // magic-byte validation during download instead.

    Ok((exif_ok, local_checksum, download_checksum))
}

fn format_duration(d: Duration) -> String {
    let total_secs = d.as_secs();
    let hours = total_secs / 3600;
    let mins = (total_secs % 3600) / 60;
    let secs = total_secs % 60;

    if hours > 0 {
        format!("{hours}h {mins:02}m {secs:02}s")
    } else if mins > 0 {
        format!("{mins}m {secs:02}s")
    } else {
        format!("{secs}s")
    }
}

/// Set the modification and access times of a file to the given Unix
/// timestamp. Uses `std::fs::File::set_times` (stable since Rust 1.75).
///
/// Handles negative timestamps (dates before 1970) gracefully by clamping
/// to the Unix epoch.
fn set_file_mtime(path: &Path, timestamp: i64) -> std::io::Result<()> {
    let time = if timestamp >= 0 {
        UNIX_EPOCH + Duration::from_secs(timestamp.unsigned_abs())
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
    use crate::icloud::photos::asset::ChangeEvent;
    use crate::icloud::photos::PhotoAsset;
    use crate::test_helpers::TestPhotoAsset;
    use serde_json::json;
    use std::fs;
    use tempfile::TempDir;

    fn test_config() -> DownloadConfig {
        DownloadConfig {
            directory: PathBuf::from("/nonexistent/download_filter_tests"),
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
            live_photo_mode: LivePhotoMode::Both,
            live_photo_size: AssetVersionSize::LiveOriginal,
            live_photo_mov_filename_policy: crate::types::LivePhotoMovFilenamePolicy::Suffix,
            align_raw: RawTreatmentPolicy::Unchanged,
            no_progress_bar: true,
            only_print_filenames: false,
            file_match_policy: FileMatchPolicy::NameSizeDedupWithSuffix,
            force_size: false,
            keep_unicode_in_filenames: false,
            filename_exclude: Vec::new(),
            temp_suffix: ".kei-tmp".to_string(),
            state_db: None,
            retry_only: false,
            max_download_attempts: 10,
            sync_mode: SyncMode::Full,
            album_name: None,
            exclude_asset_ids: Arc::new(FxHashSet::default()),
        }
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
        assert_eq!(&*tasks[0].url, "https://example.com/orig");
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
            .orig_url("https://example.com/vid")
            .orig_checksum("vid_ck")
            .build();
        let mut config = test_config();
        config.skip_videos = true;
        assert!(filter_asset_fresh(&asset, &config).is_empty());
    }

    #[test]
    fn test_filter_video_task_carries_size() {
        let asset = TestPhotoAsset::new("VID_2")
            .filename("movie.mov")
            .item_type("com.apple.quicktime-movie")
            .orig_file_type("com.apple.quicktime-movie")
            .orig_size(500_000_000)
            .orig_url("https://example.com/big_vid")
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
        assert!(filter_asset_fresh(&asset, &config).is_empty());
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
        let dir = TempDir::new().unwrap();
        let asset = TestPhotoAsset::new("TEST_1").build();
        let mut config = test_config();
        config.directory = dir.path().to_path_buf();

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
        config.directory = dir.path().to_path_buf();

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
            .orig_url("https://example.com/heic_orig")
            .orig_checksum("heic_ck")
            .live_photo("https://example.com/live_mov", "mov_ck", 3000)
            .build()
    }

    #[test]
    fn test_filter_produces_live_photo_mov_task() {
        let asset = test_live_photo_asset();
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
    fn test_filter_skips_live_photo_mov_when_image_only() {
        let asset = test_live_photo_asset();
        let mut config = test_config();
        config.live_photo_mode = LivePhotoMode::ImageOnly;
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 1);
        assert_eq!(&*tasks[0].url, "https://example.com/heic_orig");
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
        config.directory = dir.path().to_path_buf();

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
    }

    #[test]
    fn test_filter_deduplicates_live_photo_mov_collision() {
        let dir = TempDir::new().unwrap();

        let asset = test_live_photo_asset();
        let mut config = test_config();
        config.directory = dir.path().to_path_buf();

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
            .orig_url("https://example.com/heic_a")
            .orig_checksum("ck_a")
            .live_photo("https://example.com/mov_a", "mov_ck_a", 3000)
            .build();

        let asset2 = TestPhotoAsset::new("LIVE_B")
            .filename("IMG_0001.HEIC")
            .item_type("public.heic")
            .orig_file_type("public.heic")
            .orig_size(4000)
            .orig_url("https://example.com/heic_b")
            .orig_checksum("ck_b")
            .live_photo("https://example.com/mov_b", "mov_ck_b", 5000)
            .build();

        let mut config = test_config();
        config.directory = dir.path().to_path_buf();

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
        let asset = TestPhotoAsset::new("VID_1")
            .filename("movie.mov")
            .item_type("com.apple.quicktime-movie")
            .orig_file_type("com.apple.quicktime-movie")
            .orig_size(50000)
            .orig_url("https://example.com/vid")
            .orig_checksum("vid_ck")
            .live_photo("https://example.com/live_mov", "mov_ck", 3000)
            .build();
        let config = test_config();
        let tasks = filter_asset_fresh(&asset, &config);
        // Videos should get 1 task (the video itself), not a live photo MOV
        assert_eq!(tasks.len(), 1);
    }

    #[test]
    fn test_set_file_mtime_positive_timestamp() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("pos.txt");
        fs::write(&p, b"test").unwrap();
        set_file_mtime(&p, 1_700_000_000).unwrap();
        let meta = fs::metadata(&p).unwrap();
        let mtime = meta.modified().unwrap();
        assert_eq!(mtime, UNIX_EPOCH + Duration::from_secs(1_700_000_000));
    }

    #[test]
    fn test_set_file_mtime_zero_timestamp() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("zero.txt");
        fs::write(&p, b"test").unwrap();
        set_file_mtime(&p, 0).unwrap();
        let meta = fs::metadata(&p).unwrap();
        let mtime = meta.modified().unwrap();
        assert_eq!(mtime, UNIX_EPOCH);
    }

    #[test]
    fn test_set_file_mtime_negative_timestamp() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("neg.txt");
        fs::write(&p, b"test").unwrap();
        // Should not panic — clamps or uses pre-epoch time
        set_file_mtime(&p, -86400).unwrap();
    }

    #[test]
    fn test_set_file_mtime_nonexistent_file() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("nonexistent_file.txt");
        assert!(set_file_mtime(&p, 0).is_err());
    }

    fn photo_asset_with_original_and_alternative(orig_type: &str, alt_type: &str) -> PhotoAsset {
        TestPhotoAsset::new("RAW_TEST")
            .orig_checksum("orig_ck")
            .orig_file_type(orig_type)
            .alt_version("https://example.com/alt", "alt_ck", 2000, alt_type)
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
        let asset = TestPhotoAsset::new("TEST_1").build(); // only has Original
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
        let pb = create_progress_bar(true, false, 100);
        assert!(pb.is_hidden());
    }

    #[test]
    fn test_create_progress_bar_hidden_when_only_print_filenames() {
        let pb = create_progress_bar(false, true, 100);
        assert!(pb.is_hidden());
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
            .orig_url("https://example.com/vid")
            .orig_checksum("vid_ck")
            .asset_date(1713657600000.0)
            .build();

        // Second asset: live photo IMG_0996.JPG whose MOV companion would be IMG_0996.MOV
        let photo_asset = TestPhotoAsset::new("IMG_0996")
            .filename("IMG_0996.JPG")
            .orig_size(5000)
            .orig_url("https://example.com/jpg")
            .orig_checksum("jpg_ck")
            .live_photo("https://example.com/live_mov", "mov_ck", 124037918)
            .asset_date(1713657600000.0)
            .build();

        let mut config = test_config();
        config.directory = dir.path().to_path_buf();

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
    fn test_create_progress_bar_with_total() {
        // When not disabled, the bar should have the correct length.
        // In CI/test environments stdout may not be a TTY, so the bar
        // may be hidden — we test both branches.
        let pb = create_progress_bar(false, false, 42);
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

    // These tests need a larger stack due to large async futures from reqwest
    // and stream combinators. We spawn them on a thread with 8 MiB stack.
    #[test]
    fn test_run_download_pass_skips_all_tasks_when_cancelled() {
        std::thread::Builder::new()
            .stack_size(8 * 1024 * 1024)
            .spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(async {
                        let dir = TempDir::new().unwrap();
                        let token = CancellationToken::new();
                        token.cancel();

                        let tasks = vec![
                            DownloadTask {
                                url: "https://example.com/a".into(),
                                download_path: dir.path().join("a.jpg"),
                                checksum: "aaa".into(),
                                created_local: chrono::Local::now(),
                                size: 1000,
                                asset_id: "ASSET_A".into(),
                                version_size: VersionSizeKey::Original,
                            },
                            DownloadTask {
                                url: "https://example.com/b".into(),
                                download_path: dir.path().join("b.jpg"),
                                checksum: "bbb".into(),
                                created_local: chrono::Local::now(),
                                size: 2000,
                                asset_id: "ASSET_B".into(),
                                version_size: VersionSizeKey::Original,
                            },
                        ];

                        let client = Client::new();
                        let retry = RetryConfig::default();

                        let pass_config = PassConfig {
                            client: &client,
                            retry_config: &retry,
                            set_exif: false,
                            concurrency: 1,
                            no_progress_bar: true,
                            temp_suffix: ".kei-tmp".to_string(),
                            shutdown_token: token,
                            state_db: None,
                        };
                        let result = run_download_pass(pass_config, tasks).await;
                        assert!(result.failed.is_empty());
                    });
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn test_run_download_pass_processes_tasks_when_not_cancelled() {
        std::thread::Builder::new()
            .stack_size(8 * 1024 * 1024)
            .spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(async {
                        let dir = TempDir::new().unwrap();
                        let token = CancellationToken::new();

                        let tasks = vec![DownloadTask {
                            url: "https://0.0.0.0:1/nonexistent".into(),
                            download_path: dir.path().join("c.jpg"),
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

                        let pass_config = PassConfig {
                            client: &client,
                            retry_config: &retry,
                            set_exif: false,
                            concurrency: 1,
                            no_progress_bar: true,
                            temp_suffix: ".kei-tmp".to_string(),
                            shutdown_token: token,
                            state_db: None,
                        };
                        let result = run_download_pass(pass_config, tasks).await;
                        assert_eq!(result.failed.len(), 1);
                    });
            })
            .unwrap()
            .join()
            .unwrap();
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

    #[test]
    fn test_hash_download_config_deterministic() {
        let config = test_config();
        let hash1 = hash_download_config(&config);
        let hash2 = hash_download_config(&config);
        assert_eq!(hash1, hash2);
        assert_eq!(hash1.len(), 16); // 8 bytes hex-encoded
    }

    #[test]
    fn test_hash_download_config_changes_on_directory() {
        let mut config1 = test_config();
        config1.directory = PathBuf::from("/photos/a");
        let mut config2 = test_config();
        config2.directory = PathBuf::from("/photos/b");
        assert_ne!(
            hash_download_config(&config1),
            hash_download_config(&config2)
        );
    }

    #[test]
    fn test_hash_download_config_changes_on_folder_structure() {
        let mut config1 = test_config();
        config1.folder_structure = "{:%Y/%m/%d}".to_string();
        let mut config2 = test_config();
        config2.folder_structure = "{:%Y/%m}".to_string();
        assert_ne!(
            hash_download_config(&config1),
            hash_download_config(&config2)
        );
    }

    #[test]
    fn test_should_download_fast_trust_state_returns_false() {
        let mut ctx = DownloadContext::default();
        ctx.downloaded_ids
            .entry("asset1".into())
            .or_default()
            .insert("original".into());
        ctx.downloaded_checksums
            .entry("asset1".into())
            .or_default()
            .insert("original".into(), "checksum_a".into());

        // trust_state=true: returns Some(false) for matching asset
        assert_eq!(
            ctx.should_download_fast("asset1", VersionSizeKey::Original, "checksum_a", true),
            Some(false)
        );

        // trust_state=false: returns None (needs filesystem check)
        assert_eq!(
            ctx.should_download_fast("asset1", VersionSizeKey::Original, "checksum_a", false),
            None
        );

        // Changed checksum: returns Some(true) regardless of trust_state
        assert_eq!(
            ctx.should_download_fast("asset1", VersionSizeKey::Original, "checksum_b", true),
            Some(true)
        );

        // Unknown asset: returns Some(true)
        assert_eq!(
            ctx.should_download_fast("unknown", VersionSizeKey::Original, "x", true),
            Some(true)
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
            .orig_url("https://example.com/vid")
            .orig_checksum("vid_ck")
            .build();
        let mut config = test_config();
        config.skip_videos = true;
        assert!(extract_skip_candidates(&asset, &config).is_empty());
    }

    #[test]
    fn test_extract_skip_candidates_skip_photos() {
        let asset = TestPhotoAsset::new("TEST_1").build();
        let mut config = test_config();
        config.skip_photos = true;
        assert!(extract_skip_candidates(&asset, &config).is_empty());
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
        let candidates = extract_skip_candidates(&asset, &config);
        assert!(
            candidates.is_empty(),
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
        assert!(extract_skip_candidates(&asset, &config).is_empty());
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
        assert!(extract_skip_candidates(&asset, &config).is_empty());
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
        assert_eq!(&*tasks[1].url, "https://example.com/live_mov");
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

    // ── hash_download_config additional sensitivity tests ──────────

    #[test]
    fn test_hash_download_config_changes_on_file_match_policy() {
        let mut config1 = test_config();
        config1.file_match_policy = FileMatchPolicy::NameSizeDedupWithSuffix;
        let mut config2 = test_config();
        config2.file_match_policy = FileMatchPolicy::NameId7;
        assert_ne!(
            hash_download_config(&config1),
            hash_download_config(&config2)
        );
    }

    #[test]
    fn test_hash_download_config_changes_on_keep_unicode() {
        let mut config1 = test_config();
        config1.keep_unicode_in_filenames = false;
        let mut config2 = test_config();
        config2.keep_unicode_in_filenames = true;
        assert_ne!(
            hash_download_config(&config1),
            hash_download_config(&config2)
        );
    }

    #[test]
    fn test_hash_download_config_ignores_unrelated_fields() {
        let mut config1 = test_config();
        config1.concurrent_downloads = 1;
        config1.dry_run = false;
        let mut config2 = test_config();
        config2.concurrent_downloads = 16;
        config2.dry_run = true;
        // These fields don't affect download paths, so hash should be the same
        assert_eq!(
            hash_download_config(&config1),
            hash_download_config(&config2)
        );
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
            .orig_url("https://example.com/vid")
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
            .orig_url("https://example.com/vid")
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
    fn test_name_id7_skips_existing_file() {
        let asset = TestPhotoAsset::new("TEST_1").build();
        let mut config = test_config();
        config.file_match_policy = FileMatchPolicy::NameId7;
        let dir = TempDir::new().unwrap();
        config.directory = dir.path().to_path_buf();

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
            .filename("Café_photo.jpg")
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
            filename.contains("Café"),
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
            !filename.contains("Café"),
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
                    "downloadURL": "https://example.com/orig",
                    "fileChecksum": "orig_ck"
                }},
                "resOriginalFileType": {"value": "public.jpeg"},
                "resJPEGMedRes": {"value": {
                    "size": 2000,
                    "downloadURL": "https://example.com/med",
                    "fileChecksum": "med_ck"
                }},
                "resJPEGMedFileType": {"value": "public.jpeg"},
                "resJPEGThumbRes": {"value": {
                    "size": 500,
                    "downloadURL": "https://example.com/thumb",
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

    // ---------- SyncMode / SyncResult tests ----------

    #[test]
    fn test_sync_result_partial_failure() {
        let result = SyncResult {
            outcome: DownloadOutcome::PartialFailure { failed_count: 3 },
            sync_token: Some("tok".to_string()),
        };
        match result.outcome {
            DownloadOutcome::PartialFailure { failed_count } => {
                assert_eq!(failed_count, 3);
            }
            _ => panic!("Expected PartialFailure"),
        }
    }

    #[test]
    fn test_sync_result_session_expired() {
        let result = SyncResult {
            outcome: DownloadOutcome::SessionExpired {
                auth_error_count: 5,
            },
            sync_token: None,
        };
        match result.outcome {
            DownloadOutcome::SessionExpired { auth_error_count } => {
                assert_eq!(auth_error_count, 5);
            }
            _ => panic!("Expected SessionExpired"),
        }
    }

    #[test]
    fn test_change_event_filtering_downloadable_reasons() {
        // Verify that the filtering logic in download_photos_incremental
        // correctly identifies which ChangeReasons are downloadable
        let downloadable = [ChangeReason::Created];
        let skippable = [
            ChangeReason::SoftDeleted,
            ChangeReason::HardDeleted,
            ChangeReason::Hidden,
        ];

        for reason in &downloadable {
            assert!(
                matches!(reason, ChangeReason::Created),
                "{:?} should be downloadable",
                reason
            );
        }
        for reason in &skippable {
            assert!(
                !matches!(reason, ChangeReason::Created),
                "{:?} should be skippable",
                reason
            );
        }
    }

    #[test]
    fn test_change_event_asset_extraction() {
        // Verify that events with None assets are filtered out
        let event_with_asset = ChangeEvent {
            record_name: "REC_1".to_string(),
            record_type: Some("CPLAsset".to_string()),
            reason: ChangeReason::Created,
            asset: Some(TestPhotoAsset::new("TEST_1").build()),
        };
        let event_without_asset = ChangeEvent {
            record_name: "REC_2".to_string(),
            record_type: Some("CPLAsset".to_string()),
            reason: ChangeReason::Created,
            asset: None,
        };

        let events = vec![event_with_asset, event_without_asset];
        let downloadable: Vec<_> = events
            .into_iter()
            .filter(|e| matches!(e.reason, ChangeReason::Created))
            .filter_map(|e| e.asset)
            .collect();

        assert_eq!(downloadable.len(), 1);
        assert_eq!(downloadable[0].id(), "TEST_1");
    }

    #[test]
    fn test_incremental_filters_skip_deletions() {
        let events = vec![
            ChangeEvent {
                record_name: "REC_1".to_string(),
                record_type: Some("CPLAsset".to_string()),
                reason: ChangeReason::Created,
                asset: Some(TestPhotoAsset::new("TEST_1").build()),
            },
            ChangeEvent {
                record_name: "REC_2".to_string(),
                record_type: None,
                reason: ChangeReason::HardDeleted,
                asset: None,
            },
            ChangeEvent {
                record_name: "REC_3".to_string(),
                record_type: Some("CPLAsset".to_string()),
                reason: ChangeReason::SoftDeleted,
                asset: None,
            },
            ChangeEvent {
                record_name: "REC_4".to_string(),
                record_type: Some("CPLAsset".to_string()),
                reason: ChangeReason::Hidden,
                asset: None,
            },
        ];

        let downloadable: Vec<_> = events
            .into_iter()
            .filter(|e| matches!(e.reason, ChangeReason::Created))
            .filter_map(|e| e.asset)
            .collect();

        assert_eq!(downloadable.len(), 1);
        assert_eq!(downloadable[0].id(), "TEST_1");
    }

    #[test]
    fn test_incremental_modified_events_are_downloadable() {
        let events = vec![ChangeEvent {
            record_name: "MOD_1".to_string(),
            record_type: Some("CPLAsset".to_string()),
            reason: ChangeReason::Created,
            asset: Some(TestPhotoAsset::new("TEST_1").build()),
        }];

        let downloadable: Vec<_> = events
            .into_iter()
            .filter(|e| matches!(e.reason, ChangeReason::Created))
            .filter_map(|e| e.asset)
            .collect();

        assert_eq!(downloadable.len(), 1);
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

    // ── format_duration additional edge cases ────────────────────────────

    #[test]
    fn test_format_duration_125_seconds() {
        assert_eq!(format_duration(Duration::from_secs(125)), "2m 05s");
    }

    #[test]
    fn test_format_duration_3661_seconds() {
        assert_eq!(format_duration(Duration::from_secs(3661)), "1h 01m 01s");
    }

    #[test]
    fn test_format_duration_ignores_sub_second() {
        // Duration with millis should only show whole seconds
        assert_eq!(format_duration(Duration::from_millis(1999)), "1s");
        assert_eq!(format_duration(Duration::from_millis(500)), "0s");
    }

    // ── hash_download_config additional sensitivity ─────────────────────

    #[test]
    fn test_hash_download_config_changes_on_size() {
        let mut config1 = test_config();
        config1.size = AssetVersionSize::Original;
        let mut config2 = test_config();
        config2.size = AssetVersionSize::Medium;
        assert_ne!(
            hash_download_config(&config1),
            hash_download_config(&config2)
        );
    }

    #[test]
    fn test_hash_download_config_changes_on_live_photo_size() {
        let mut config1 = test_config();
        config1.live_photo_size = AssetVersionSize::LiveOriginal;
        let mut config2 = test_config();
        config2.live_photo_size = AssetVersionSize::LiveMedium;
        assert_ne!(
            hash_download_config(&config1),
            hash_download_config(&config2)
        );
    }

    #[test]
    fn test_hash_download_config_changes_on_live_photo_mov_filename_policy() {
        let mut config1 = test_config();
        config1.live_photo_mov_filename_policy = crate::types::LivePhotoMovFilenamePolicy::Suffix;
        let mut config2 = test_config();
        config2.live_photo_mov_filename_policy = crate::types::LivePhotoMovFilenamePolicy::Original;
        assert_ne!(
            hash_download_config(&config1),
            hash_download_config(&config2)
        );
    }

    #[test]
    fn test_hash_download_config_changes_on_align_raw() {
        let mut config1 = test_config();
        config1.align_raw = RawTreatmentPolicy::Unchanged;
        let mut config2 = test_config();
        config2.align_raw = RawTreatmentPolicy::PreferOriginal;
        assert_ne!(
            hash_download_config(&config1),
            hash_download_config(&config2)
        );
    }

    #[test]
    fn test_hash_download_config_changes_on_skip_created_before() {
        let mut config1 = test_config();
        config1.skip_created_before = None;
        let mut config2 = test_config();
        config2.skip_created_before = Some(
            DateTime::parse_from_rfc3339("2020-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        );
        assert_ne!(
            hash_download_config(&config1),
            hash_download_config(&config2)
        );
    }

    #[test]
    fn test_hash_download_config_changes_on_skip_created_after() {
        let mut config1 = test_config();
        config1.skip_created_after = None;
        let mut config2 = test_config();
        config2.skip_created_after = Some(
            DateTime::parse_from_rfc3339("2024-12-31T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        );
        assert_ne!(
            hash_download_config(&config1),
            hash_download_config(&config2)
        );
    }

    #[test]
    fn test_hash_download_config_changes_on_recent() {
        let mut config1 = test_config();
        config1.recent = None;
        let mut config2 = test_config();
        config2.recent = Some(100);
        assert_ne!(
            hash_download_config(&config1),
            hash_download_config(&config2)
        );
    }

    #[test]
    fn test_hash_download_config_changes_on_force_size() {
        let mut config1 = test_config();
        config1.force_size = false;
        let mut config2 = test_config();
        config2.force_size = true;
        assert_ne!(
            hash_download_config(&config1),
            hash_download_config(&config2)
        );
    }

    #[test]
    fn test_hash_download_config_changes_on_skip_videos() {
        let mut config1 = test_config();
        config1.skip_videos = false;
        let mut config2 = test_config();
        config2.skip_videos = true;
        assert_ne!(
            hash_download_config(&config1),
            hash_download_config(&config2)
        );
    }

    #[test]
    fn test_hash_download_config_changes_on_skip_photos() {
        let mut config1 = test_config();
        config1.skip_photos = false;
        let mut config2 = test_config();
        config2.skip_photos = true;
        assert_ne!(
            hash_download_config(&config1),
            hash_download_config(&config2)
        );
    }

    #[test]
    fn test_hash_download_config_is_16_hex_chars() {
        let config = test_config();
        let hash = hash_download_config(&config);
        assert_eq!(hash.len(), 16);
        assert!(
            hash.chars().all(|c| c.is_ascii_hexdigit()),
            "Hash should be hex chars only, got: {hash}"
        );
    }

    // ── compute_config_hash equivalence ────────────────────────────────

    /// `compute_config_hash` includes enumeration-filter fields (albums,
    /// library, live_photo_mode) that `hash_download_config` doesn't.
    /// Verify it produces a valid hex hash and is deterministic.
    #[test]
    fn test_compute_config_hash_matches_hash_download_config() {
        use crate::config::Config;
        use crate::types::{
            Domain, FileMatchPolicy, LivePhotoMode, LivePhotoMovFilenamePolicy, LivePhotoSize,
            RawTreatmentPolicy, VersionSize,
        };
        use secrecy::SecretString;

        let dl_config = test_config();
        let app_config = Config {
            username: String::new(),
            password: Some(SecretString::from("x")),
            password_file: None,
            password_command: None,
            directory: dl_config.directory.clone(),
            cookie_directory: std::path::PathBuf::from("/tmp"),
            folder_structure: dl_config.folder_structure.clone(),
            albums: vec![],
            exclude_albums: vec![],
            filename_exclude: vec![],
            library: crate::config::LibrarySelection::Single("PrimarySync".into()),
            temp_suffix: dl_config.temp_suffix.clone(),
            skip_created_before: None,
            skip_created_after: None,
            pid_file: None,
            notification_script: None,
            watch_with_interval: None,
            retry_delay_secs: 5,
            recent: dl_config.recent,
            max_retries: 3,
            threads_num: 1,
            size: VersionSize::Original,
            live_photo_size: LivePhotoSize::Original,
            domain: Domain::Com,
            live_photo_mode: LivePhotoMode::Both,
            live_photo_mov_filename_policy: LivePhotoMovFilenamePolicy::Suffix,
            align_raw: RawTreatmentPolicy::Unchanged,
            file_match_policy: FileMatchPolicy::NameSizeDedupWithSuffix,
            skip_videos: false,
            skip_photos: false,
            force_size: false,
            set_exif_datetime: false,
            dry_run: false,
            no_progress_bar: true,
            keep_unicode_in_filenames: false,
            only_print_filenames: false,
            no_incremental: false,
            notify_systemd: false,
            save_password: false,
        };

        // compute_config_hash is a superset (includes albums, library, live_photo_mode)
        // so it won't match hash_download_config. Verify it's deterministic and valid hex.
        let hash1 = compute_config_hash(&app_config);
        let hash2 = compute_config_hash(&app_config);
        assert_eq!(hash1, hash2, "compute_config_hash must be deterministic");
        assert_eq!(hash1.len(), 16);
        assert!(hash1.chars().all(|c| c.is_ascii_hexdigit()));

        // Verify album changes produce a different hash
        let mut config_with_album = app_config;
        config_with_album.albums = vec!["Favorites".to_string()];
        let hash3 = compute_config_hash(&config_with_album);
        assert_ne!(hash1, hash3, "adding an album must change the hash");
    }

    // ── should_download_fast additional tests ───────────────────────────

    #[test]
    fn test_should_download_fast_unknown_asset_returns_true() {
        let ctx = DownloadContext::default();
        assert_eq!(
            ctx.should_download_fast("never_seen", VersionSizeKey::Original, "any_ck", true),
            Some(true)
        );
        assert_eq!(
            ctx.should_download_fast("never_seen", VersionSizeKey::Original, "any_ck", false),
            Some(true)
        );
    }

    #[test]
    fn test_should_download_fast_downloaded_matching_checksum() {
        let mut ctx = DownloadContext::default();
        ctx.downloaded_ids
            .entry("asset_x".into())
            .or_default()
            .insert("original".into());
        ctx.downloaded_checksums
            .entry("asset_x".into())
            .or_default()
            .insert("original".into(), "ck_match".into());

        // trust_state=true => hard skip
        assert_eq!(
            ctx.should_download_fast("asset_x", VersionSizeKey::Original, "ck_match", true),
            Some(false)
        );
        // trust_state=false => needs filesystem check
        assert_eq!(
            ctx.should_download_fast("asset_x", VersionSizeKey::Original, "ck_match", false),
            None
        );
    }

    #[test]
    fn test_should_download_fast_downloaded_changed_checksum() {
        let mut ctx = DownloadContext::default();
        ctx.downloaded_ids
            .entry("asset_y".into())
            .or_default()
            .insert("original".into());
        ctx.downloaded_checksums
            .entry("asset_y".into())
            .or_default()
            .insert("original".into(), "old_ck".into());

        // Changed checksum => needs re-download regardless of trust_state
        assert_eq!(
            ctx.should_download_fast("asset_y", VersionSizeKey::Original, "new_ck", true),
            Some(true)
        );
        assert_eq!(
            ctx.should_download_fast("asset_y", VersionSizeKey::Original, "new_ck", false),
            Some(true)
        );
    }

    #[test]
    fn test_should_download_fast_different_version_size() {
        let mut ctx = DownloadContext::default();
        ctx.downloaded_ids
            .entry("asset_z".into())
            .or_default()
            .insert("original".into());

        // Medium version not downloaded
        assert_eq!(
            ctx.should_download_fast("asset_z", VersionSizeKey::Medium, "any_ck", true),
            Some(true)
        );
    }

    #[test]
    fn test_download_context_known_ids_populated_for_retry_only() {
        // Simulate retry-only mode: known_ids is populated
        let mut ctx = DownloadContext::default();
        ctx.known_ids.insert("known_asset".into());

        // A known asset that's not in downloaded_ids needs download
        assert_eq!(
            ctx.should_download_fast("known_asset", VersionSizeKey::Original, "ck", true),
            Some(true)
        );
        // The known_ids set is used externally to decide whether to skip new assets;
        // verify the set membership works
        assert!(ctx.known_ids.contains("known_asset"));
        assert!(!ctx.known_ids.contains("new_asset"));
    }

    // ── Change event classification tests ───────────────────────────────

    #[test]
    fn test_change_event_filtering_counts_and_extraction() {
        // Simulate the inline filtering loop from download_photos_incremental
        let events = vec![
            ChangeEvent {
                record_name: "A".to_string(),
                record_type: Some("CPLAsset".to_string()),
                reason: ChangeReason::Created,
                asset: Some(TestPhotoAsset::new("TEST_1").build()),
            },
            ChangeEvent {
                record_name: "B".to_string(),
                record_type: Some("CPLAsset".to_string()),
                reason: ChangeReason::Created,
                asset: None, // Unpaired record
            },
            ChangeEvent {
                record_name: "C".to_string(),
                record_type: None,
                reason: ChangeReason::HardDeleted,
                asset: None,
            },
            ChangeEvent {
                record_name: "D".to_string(),
                record_type: Some("CPLAsset".to_string()),
                reason: ChangeReason::SoftDeleted,
                asset: None,
            },
            ChangeEvent {
                record_name: "E".to_string(),
                record_type: Some("CPLAsset".to_string()),
                reason: ChangeReason::Hidden,
                asset: None,
            },
        ];

        let mut created_count = 0u32;
        let mut soft_deleted_count = 0u32;
        let mut hard_deleted_count = 0u32;
        let mut hidden_count = 0u32;
        let mut downloadable_assets = Vec::new();

        for event in events {
            match event.reason {
                ChangeReason::Created => {
                    created_count += 1;
                    if let Some(asset) = event.asset {
                        downloadable_assets.push(asset);
                    }
                }
                ChangeReason::SoftDeleted => soft_deleted_count += 1,
                ChangeReason::HardDeleted => hard_deleted_count += 1,
                ChangeReason::Hidden => hidden_count += 1,
            }
        }

        assert_eq!(created_count, 2);
        assert_eq!(soft_deleted_count, 1);
        assert_eq!(hard_deleted_count, 1);
        assert_eq!(hidden_count, 1);
        assert_eq!(downloadable_assets.len(), 1);
        assert_eq!(downloadable_assets[0].id(), "TEST_1");
    }

    #[tokio::test]
    async fn test_producer_panic_propagates_as_error() {
        let config = Arc::new(test_config());
        let client = reqwest::Client::new();
        let shutdown_token = CancellationToken::new();

        // Stream that panics on first poll — simulates a producer task panic
        let panicking_stream = futures_util::stream::poll_fn(
            |_cx| -> std::task::Poll<Option<anyhow::Result<PhotoAsset>>> {
                panic!("simulated producer panic");
            },
        );

        let err =
            stream_and_download_from_stream(&client, panicking_stream, &config, 0, shutdown_token)
                .await
                .expect_err("should propagate producer panic");
        assert!(
            err.to_string().contains("producer panicked"),
            "Expected producer panic error, got: {err}"
        );
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
            .orig_url("https://cdn.icloud.com/photos/orig/abc")
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

    #[test]
    fn filter_asset_missing_filename_uses_fingerprint_fallback() {
        // Asset whose filenameEnc field is absent (null) should trigger the
        // fingerprint fallback path, generating a filename from the asset ID.
        let asset = PhotoAsset::new(
            json!({"recordName": "NOFN_ASSET1", "fields": {
                "itemType": {"value": "public.jpeg"},
                "resOriginalRes": {"value": {
                    "size": 2048,
                    "downloadURL": "https://cdn.icloud.com/photos/orig/nofn",
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

    // ── Gap coverage: should_download_fast with empty checksum ──────────

    #[test]
    fn should_download_fast_empty_checksum_string() {
        // When the stored checksum is empty and the incoming checksum is also
        // empty, they match — should behave like a normal matching checksum.
        let mut ctx = DownloadContext::default();
        ctx.downloaded_ids
            .entry("asset_empty_ck".into())
            .or_default()
            .insert("original".into());
        ctx.downloaded_checksums
            .entry("asset_empty_ck".into())
            .or_default()
            .insert("original".into(), "".into());

        // Empty matches empty → trust_state=true gives hard skip
        assert_eq!(
            ctx.should_download_fast("asset_empty_ck", VersionSizeKey::Original, "", true),
            Some(false)
        );
        // Empty matches empty → trust_state=false gives None (needs fs check)
        assert_eq!(
            ctx.should_download_fast("asset_empty_ck", VersionSizeKey::Original, "", false),
            None
        );
        // Non-empty vs empty stored → checksum changed, needs download
        assert_eq!(
            ctx.should_download_fast(
                "asset_empty_ck",
                VersionSizeKey::Original,
                "abc123def456",
                true,
            ),
            Some(true)
        );
    }

    // ── Gap coverage: retry_only known_ids filtering ────────────────────

    #[test]
    fn download_context_retry_only_skips_unknown_assets() {
        // In retry-only mode, the producer checks known_ids before sending
        // tasks. Simulate that filtering logic here.
        let mut ctx = DownloadContext::default();
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
        assert!(
            filter_asset_fresh(&asset, &config).is_empty(),
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
        assert!(
            filter_asset_fresh(&asset, &config).is_empty(),
            "Asset after the date window should be skipped"
        );
    }

    // ── Gap coverage: incremental Modified events are downloadable ──────

    #[test]
    fn change_event_modified_asset_is_downloadable() {
        // In the iCloud changes API, both new and modified records arrive as
        // ChangeReason::Created (the enum doc says "new or modified").
        // Verify that a "modified" asset with a ChangeReason::Created is
        // picked up by the download filter.
        let modified_asset = TestPhotoAsset::new("MODIFIED_ASSET_1")
            .filename("IMG_9876.HEIC")
            .item_type("public.heic")
            .orig_file_type("public.heic")
            .orig_size(4500000)
            .orig_url("https://cdn.icloud.com/photos/orig/modified")
            .orig_checksum("f0e1d2c3b4a5")
            .build();

        let event = ChangeEvent {
            record_name: "MODIFIED_ASSET_1".to_string(),
            record_type: Some("CPLAsset".to_string()),
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

    // ── Gap coverage: NameId7 produces task when file at original path ──

    #[test]
    fn filter_asset_name_id7_downloads_when_original_path_exists() {
        // With NameId7 policy, the download path includes an ID suffix.
        // Even if a file exists at the *non-suffixed* (original) path,
        // NameId7 should produce a task because its path is different.
        let dir = TempDir::new().unwrap();

        let asset = TestPhotoAsset::new("TEST_1").build(); // recordName "TEST_1", "photo.jpg"
        let mut config = test_config();
        config.directory = dir.path().to_path_buf();
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

    // ── State-write retry tests ──

    use crate::state::error::StateError;
    use crate::state::types::SyncSummary;
    use std::collections::{HashMap, HashSet};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A StateDb stub where `mark_downloaded` fails a configurable number
    /// of times before succeeding. All other methods panic (unused).
    struct FailingStateDb {
        remaining_failures: AtomicUsize,
        successes: AtomicUsize,
    }

    impl FailingStateDb {
        fn new(fail_count: usize) -> Self {
            Self {
                remaining_failures: AtomicUsize::new(fail_count),
                successes: AtomicUsize::new(0),
            }
        }

        fn success_count(&self) -> usize {
            self.successes.load(Ordering::Relaxed)
        }
    }

    #[async_trait::async_trait]
    impl StateDb for FailingStateDb {
        #[cfg(test)]
        async fn should_download(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: &Path,
        ) -> Result<bool, StateError> {
            unimplemented!()
        }
        async fn upsert_seen(&self, _: &AssetRecord) -> Result<(), StateError> {
            unimplemented!()
        }
        async fn mark_downloaded(
            &self,
            _: &str,
            _: &str,
            _: &Path,
            _: &str,
            _: Option<&str>,
        ) -> Result<(), StateError> {
            let prev = self.remaining_failures.fetch_sub(1, Ordering::Relaxed);
            if prev > 0 {
                Err(StateError::Query("simulated failure".into()))
            } else {
                self.remaining_failures.store(0, Ordering::Relaxed);
                self.successes.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
        }
        async fn mark_failed(&self, _: &str, _: &str, _: &str) -> Result<(), StateError> {
            unimplemented!()
        }
        async fn get_failed(&self) -> Result<Vec<AssetRecord>, StateError> {
            unimplemented!()
        }
        async fn get_summary(&self) -> Result<SyncSummary, StateError> {
            unimplemented!()
        }
        async fn get_downloaded_page(
            &self,
            _offset: u64,
            _limit: u32,
        ) -> Result<Vec<AssetRecord>, StateError> {
            unimplemented!()
        }
        async fn start_sync_run(&self) -> Result<i64, StateError> {
            unimplemented!()
        }
        async fn complete_sync_run(&self, _: i64, _: &SyncRunStats) -> Result<(), StateError> {
            unimplemented!()
        }
        async fn reset_failed(&self) -> Result<u64, StateError> {
            unimplemented!()
        }
        async fn get_downloaded_ids(&self) -> Result<HashSet<(String, String)>, StateError> {
            unimplemented!()
        }
        async fn get_all_known_ids(&self) -> Result<HashSet<String>, StateError> {
            unimplemented!()
        }
        async fn get_downloaded_checksums(
            &self,
        ) -> Result<HashMap<(String, String), String>, StateError> {
            unimplemented!()
        }
        async fn get_attempt_counts(&self) -> Result<HashMap<String, u32>, StateError> {
            Ok(HashMap::new())
        }
        async fn get_metadata(&self, _: &str) -> Result<Option<String>, StateError> {
            unimplemented!()
        }
        async fn set_metadata(&self, _: &str, _: &str) -> Result<(), StateError> {
            unimplemented!()
        }
        async fn delete_metadata_by_prefix(&self, _: &str) -> Result<u64, StateError> {
            unimplemented!()
        }
        async fn touch_last_seen(&self, _: &str) -> Result<(), StateError> {
            unimplemented!()
        }
        async fn sample_downloaded_paths(
            &self,
            _: usize,
        ) -> Result<Vec<std::path::PathBuf>, StateError> {
            unimplemented!()
        }
    }

    #[tokio::test]
    async fn flush_pending_state_writes_empty_is_noop() {
        let db = FailingStateDb::new(0);
        let result = flush_pending_state_writes(&db, &[]).await;
        assert_eq!(result, 0);
        assert_eq!(db.success_count(), 0);
    }

    #[tokio::test]
    async fn flush_pending_state_writes_succeeds_on_first_try() {
        let db = FailingStateDb::new(0);
        let pending = vec![PendingStateWrite {
            asset_id: "A1".into(),
            version_size: VersionSizeKey::Original,
            download_path: PathBuf::from("/tmp/claude/photo.jpg"),
            local_checksum: "abc".into(),
            download_checksum: None,
        }];
        let failures = flush_pending_state_writes(&db, &pending).await;
        assert_eq!(failures, 0);
        assert_eq!(db.success_count(), 1);
    }

    #[tokio::test]
    async fn flush_pending_state_writes_recovers_after_transient_failure() {
        // Fail the first attempt, succeed on retry
        let db = FailingStateDb::new(1);
        let pending = vec![PendingStateWrite {
            asset_id: "A1".into(),
            version_size: VersionSizeKey::Original,
            download_path: PathBuf::from("/tmp/claude/photo.jpg"),
            local_checksum: "abc".into(),
            download_checksum: None,
        }];
        let failures = flush_pending_state_writes(&db, &pending).await;
        assert_eq!(failures, 0);
        assert_eq!(db.success_count(), 1);
    }

    #[tokio::test]
    async fn flush_pending_state_writes_reports_persistent_failure() {
        // Fail all attempts — must exceed STATE_WRITE_MAX_RETRIES
        let db = FailingStateDb::new(STATE_WRITE_MAX_RETRIES as usize);
        let pending = vec![PendingStateWrite {
            asset_id: "A1".into(),
            version_size: VersionSizeKey::Original,
            download_path: PathBuf::from("/tmp/claude/photo.jpg"),
            local_checksum: "abc".into(),
            download_checksum: None,
        }];
        let failures = flush_pending_state_writes(&db, &pending).await;
        assert_eq!(failures, 1);
        assert_eq!(db.success_count(), 0);
    }

    #[tokio::test]
    async fn flush_pending_state_writes_partial_recovery() {
        // First write exhausts all STATE_WRITE_MAX_RETRIES attempts (reported as failure).
        // Second write fails once more then succeeds on retry.
        let db = FailingStateDb::new(STATE_WRITE_MAX_RETRIES as usize + 1);
        let pending = vec![
            PendingStateWrite {
                asset_id: "A1".into(),
                version_size: VersionSizeKey::Original,
                download_path: PathBuf::from("/tmp/claude/photo1.jpg"),
                local_checksum: "abc".into(),
                download_checksum: None,
            },
            PendingStateWrite {
                asset_id: "A2".into(),
                version_size: VersionSizeKey::Original,
                download_path: PathBuf::from("/tmp/claude/photo2.jpg"),
                local_checksum: "def".into(),
                download_checksum: None,
            },
        ];
        let failures = flush_pending_state_writes(&db, &pending).await;
        assert_eq!(
            failures, 1,
            "First write should fail, second should recover"
        );
        assert_eq!(db.success_count(), 1);
    }

    /// T-6: All pending state writes from the download loop are retained and
    /// re-flushed. Even with multiple records and transient failures, every
    /// write that eventually succeeds reaches the DB.
    #[tokio::test]
    async fn flush_pending_state_writes_retains_all_records() {
        // 5 pending writes. First 2 failures are transient (writes 1&2 fail once
        // each then succeed on retry). All 5 should eventually succeed.
        let db = FailingStateDb::new(2);
        let pending: Vec<PendingStateWrite> = (0..5)
            .map(|i| PendingStateWrite {
                asset_id: format!("ASSET_{i}").into(),
                version_size: VersionSizeKey::Original,
                download_path: PathBuf::from(format!("/tmp/claude/photo_{i}.jpg")),
                local_checksum: format!("ck_{i}"),
                download_checksum: Some(format!("dl_ck_{i}")),
            })
            .collect();

        let failures = flush_pending_state_writes(&db, &pending).await;
        assert_eq!(failures, 0, "all 5 writes should eventually succeed");
        assert_eq!(db.success_count(), 5);
    }

    /// T-11: When the API returns the same asset ID on two different pages,
    /// the dedup logic (seen_ids) ensures only one download task is created.
    #[test]
    fn test_duplicate_asset_id_detected() {
        use rustc_hash::FxHashSet;

        // Simulate the producer's seen_ids dedup logic
        let mut seen_ids: FxHashSet<Box<str>> = FxHashSet::default();

        let asset1_id: Box<str> = "DUPLICATE_ASSET".into();
        let asset2_id: Box<str> = "DUPLICATE_ASSET".into();
        let asset3_id: Box<str> = "UNIQUE_ASSET".into();

        // First occurrence: insert succeeds
        assert!(
            seen_ids.insert(asset1_id),
            "first occurrence should be accepted"
        );

        // Duplicate on second page: insert returns false
        assert!(
            !seen_ids.insert(asset2_id),
            "duplicate asset ID should be detected and skipped"
        );

        // Different asset: insert succeeds
        assert!(
            seen_ids.insert(asset3_id),
            "unique asset should be accepted"
        );

        assert_eq!(seen_ids.len(), 2, "only 2 unique IDs should be tracked");
    }

    /// NB-1: When a CancellationToken fires during a download pass with
    /// concurrent tasks, the function must return promptly (well within the
    /// Docker stop_grace_period) rather than blocking on the remaining stream.
    #[tokio::test]
    async fn shutdown_cancellation_exits_download_pass_promptly() {
        use futures_util::stream;
        use std::time::{Duration, Instant};
        use tokio_util::sync::CancellationToken;

        // Build a slow infinite stream of photo assets — yields one every 50ms.
        // Without cancellation this would run forever.
        let asset_stream = stream::unfold(0u32, |i| async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let asset = TestPhotoAsset::new(&format!("SHUTDOWN_{i}"))
                .orig_size(100)
                .orig_url("http://127.0.0.1:1/photo.jpg")
                .orig_checksum(&format!("ck_{i}"))
                .build();
            Some((Ok(asset) as anyhow::Result<PhotoAsset>, i + 1))
        });

        let dir = TempDir::new().unwrap();

        let config = Arc::new(DownloadConfig {
            directory: dir.path().to_path_buf(),
            concurrent_downloads: 10,
            retry: crate::retry::RetryConfig {
                max_retries: 0,
                base_delay_secs: 0,
                max_delay_secs: 0,
            },
            ..test_config()
        });

        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_millis(50))
            .build()
            .expect("client");

        let shutdown_token = CancellationToken::new();
        let token_clone = shutdown_token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            token_clone.cancel();
        });

        let start = Instant::now();
        let result =
            stream_and_download_from_stream(&client, asset_stream, &config, 10_000, shutdown_token)
                .await;
        let elapsed = start.elapsed();

        assert!(
            result.is_ok(),
            "should return Ok after cancellation, got: {result:?}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "should exit promptly after cancellation, took {elapsed:?}"
        );
    }

    // ── compute_config_hash tests ──────────────────────────────────

    /// Build a `Config` via `Config::build` with the given overrides.
    /// Uses a tempdir for cookie_directory so tests don't touch the real filesystem.
    fn build_config_with(
        cookie_dir: &std::path::Path,
        directory: &str,
        overrides: impl FnOnce(&mut crate::cli::SyncArgs),
    ) -> crate::config::Config {
        use crate::cli::SyncArgs;
        use crate::config::GlobalArgs;

        let globals = GlobalArgs {
            username: Some("test@example.com".to_string()),
            domain: None,
            data_dir: Some(cookie_dir.to_string_lossy().into_owned()),
            cookie_directory: None,
        };
        let mut sync = SyncArgs {
            directory: Some(directory.to_string()),
            ..SyncArgs::default()
        };
        overrides(&mut sync);
        crate::config::Config::build(&globals, crate::cli::PasswordArgs::default(), sync, None)
            .expect("Config::build should succeed")
    }

    #[test]
    fn test_compute_config_hash_same_config_same_hash() {
        let tmp = TempDir::new().unwrap();
        let a = build_config_with(tmp.path(), "/photos", |_| {});
        let b = build_config_with(tmp.path(), "/photos", |_| {});
        assert_eq!(compute_config_hash(&a), compute_config_hash(&b));
    }

    #[test]
    fn test_compute_config_hash_different_directory() {
        let tmp = TempDir::new().unwrap();
        let a = build_config_with(tmp.path(), "/photos/a", |_| {});
        let b = build_config_with(tmp.path(), "/photos/b", |_| {});
        assert_ne!(compute_config_hash(&a), compute_config_hash(&b));
    }

    #[test]
    fn test_compute_config_hash_different_size() {
        use crate::types::VersionSize;
        let tmp = TempDir::new().unwrap();
        let a = build_config_with(tmp.path(), "/photos", |_| {});
        let b = build_config_with(tmp.path(), "/photos", |s| {
            s.size = Some(VersionSize::Medium);
        });
        assert_ne!(compute_config_hash(&a), compute_config_hash(&b));
    }

    #[test]
    fn test_compute_config_hash_different_skip_videos() {
        let tmp = TempDir::new().unwrap();
        let a = build_config_with(tmp.path(), "/photos", |_| {});
        let b = build_config_with(tmp.path(), "/photos", |s| {
            s.skip_videos = Some(true);
        });
        assert_ne!(compute_config_hash(&a), compute_config_hash(&b));
    }

    #[test]
    fn test_compute_config_hash_different_albums() {
        let tmp = TempDir::new().unwrap();
        let a = build_config_with(tmp.path(), "/photos", |_| {});
        let b = build_config_with(tmp.path(), "/photos", |s| {
            s.albums = vec!["Favorites".to_string()];
        });
        assert_ne!(compute_config_hash(&a), compute_config_hash(&b));
    }

    #[test]
    fn test_compute_config_hash_different_exclude_albums() {
        let tmp = TempDir::new().unwrap();
        let a = build_config_with(tmp.path(), "/photos", |_| {});
        let b = build_config_with(tmp.path(), "/photos", |s| {
            s.exclude_albums = vec!["Hidden".to_string()];
        });
        assert_ne!(compute_config_hash(&a), compute_config_hash(&b));
    }

    #[test]
    fn test_compute_config_hash_different_live_photo_mode() {
        let tmp = TempDir::new().unwrap();
        let a = build_config_with(tmp.path(), "/photos", |_| {});
        let b = build_config_with(tmp.path(), "/photos", |s| {
            s.live_photo_mode = Some(LivePhotoMode::Skip);
        });
        assert_ne!(compute_config_hash(&a), compute_config_hash(&b));
    }

    #[test]
    fn test_compute_config_hash_different_recent_same_hash() {
        let tmp = TempDir::new().unwrap();
        let a = build_config_with(tmp.path(), "/photos", |_| {});
        let b = build_config_with(tmp.path(), "/photos", |s| {
            s.recent = Some(100);
        });
        assert_eq!(
            compute_config_hash(&a),
            compute_config_hash(&b),
            "recent is intentionally excluded from the config hash"
        );
    }

    #[test]
    fn test_compute_config_hash_different_dry_run_same_hash() {
        let tmp = TempDir::new().unwrap();
        let a = build_config_with(tmp.path(), "/photos", |_| {});
        let b = build_config_with(tmp.path(), "/photos", |s| {
            s.dry_run = true;
        });
        assert_eq!(
            compute_config_hash(&a),
            compute_config_hash(&b),
            "dry_run is a per-run flag and should not affect the config hash"
        );
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
        assert!(
            filter_asset_fresh(&asset, &config).is_empty(),
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
        assert!(
            filter_asset_fresh(&asset, &config).is_empty(),
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
        let tasks = filter_asset_fresh(&asset, &config);
        assert!(
            tasks.is_empty(),
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
        config.filename_exclude = vec![glob::Pattern::new("*.AAE").unwrap()];
        let tasks = filter_asset_fresh(&asset, &config);
        assert!(tasks.is_empty(), "*.AAE pattern should exclude AAE files");
    }

    #[test]
    fn test_filter_filename_exclude_case_insensitive() {
        let asset = TestPhotoAsset::new("EXCL_2").filename("Photo.aae").build();
        let mut config = test_config();
        config.filename_exclude = vec![glob::Pattern::new("*.AAE").unwrap()];
        let tasks = filter_asset_fresh(&asset, &config);
        assert!(
            tasks.is_empty(),
            "Pattern matching should be case-insensitive"
        );
    }

    #[test]
    fn test_filter_filename_exclude_no_match_passes() {
        let asset = TestPhotoAsset::new("EXCL_3")
            .filename("IMG_0001.JPG")
            .build();
        let mut config = test_config();
        config.filename_exclude = vec![glob::Pattern::new("*.AAE").unwrap()];
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
        let tasks = filter_asset_fresh(&asset, &config);
        assert!(tasks.is_empty(), "Asset in exclude set should be filtered");
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
        let candidates = extract_skip_candidates(&asset, &config);
        assert!(
            candidates.is_empty(),
            "extract_skip_candidates should return empty for excluded assets"
        );
    }

    #[test]
    fn test_hash_changes_on_live_photo_mode() {
        let config1 = test_config();
        let mut config2 = test_config();
        config2.live_photo_mode = LivePhotoMode::Skip;
        assert_ne!(
            hash_download_config(&config1),
            hash_download_config(&config2)
        );
    }

    #[test]
    fn test_hash_changes_on_filename_exclude() {
        let config1 = test_config();
        let mut config2 = test_config();
        config2.filename_exclude = vec![glob::Pattern::new("*.AAE").unwrap()];
        assert_ne!(
            hash_download_config(&config1),
            hash_download_config(&config2)
        );
    }

    // ── with_album_name tests ─────────────────────────────────────

    #[test]
    fn test_with_album_name_expands_album_token() {
        let mut config = test_config();
        config.folder_structure = "{album}/%Y/%m/%d".to_string();
        let derived = config.with_album_name(Arc::from("Vacation"));
        assert_eq!(derived.folder_structure, "Vacation/%Y/%m/%d");
    }

    #[test]
    fn test_with_album_name_sets_album_name_field() {
        let config = test_config();
        assert!(config.album_name.is_none());
        let derived = config.with_album_name(Arc::from("Favorites"));
        assert_eq!(derived.album_name.as_deref(), Some("Favorites"));
    }

    #[test]
    fn test_with_album_name_preserves_all_fields() {
        let mut config = test_config();
        config.folder_structure = "{album}/%Y".to_string();
        config.skip_videos = true;
        config.skip_photos = true;
        config.live_photo_mode = LivePhotoMode::ImageOnly;
        config.force_size = true;
        config.keep_unicode_in_filenames = true;
        config.dry_run = true;
        config.set_exif_datetime = true;
        config.filename_exclude = vec![glob::Pattern::new("*.AAE").unwrap()];
        config.temp_suffix = ".custom-tmp".to_string();
        let derived = config.with_album_name(Arc::from("Test"));
        assert!(derived.skip_videos);
        assert!(derived.skip_photos);
        assert_eq!(derived.live_photo_mode, LivePhotoMode::ImageOnly);
        assert!(derived.force_size);
        assert!(derived.keep_unicode_in_filenames);
        assert!(derived.dry_run);
        assert!(derived.set_exif_datetime);
        assert_eq!(derived.filename_exclude.len(), 1);
        assert_eq!(derived.temp_suffix, ".custom-tmp");
        assert_eq!(derived.directory, config.directory);
    }

    #[test]
    fn test_with_album_name_empty_name_leaves_token_stripped() {
        let mut config = test_config();
        config.folder_structure = "{album}/%Y/%m/%d".to_string();
        let derived = config.with_album_name(Arc::from(""));
        // Empty album name should strip the {album}/ prefix
        assert!(!derived.folder_structure.contains("{album}"));
        assert!(derived.album_name.as_deref() == Some(""));
    }

    #[test]
    fn test_with_album_name_no_token_in_structure() {
        let config = test_config(); // folder_structure = "%Y/%m/%d"
        let derived = config.with_album_name(Arc::from("MyAlbum"));
        // No {album} token, so structure should be unchanged
        assert_eq!(derived.folder_structure, "%Y/%m/%d");
        assert_eq!(derived.album_name.as_deref(), Some("MyAlbum"));
    }

    #[test]
    fn test_with_album_name_sanitizes_special_chars() {
        let mut config = test_config();
        config.folder_structure = "{album}/%Y".to_string();
        let derived = config.with_album_name(Arc::from("My/Album"));
        // The expand_album_token sanitizes path separators
        assert!(
            !derived.folder_structure.contains('/')
                || !derived.folder_structure.starts_with("My/Album")
        );
    }

    // ── extract_skip_candidates: filename_exclude ─────────────────

    #[test]
    fn test_extract_skip_candidates_filename_exclude_matches() {
        let asset = TestPhotoAsset::new("TEST_1").filename("photo.AAE").build();
        let mut config = test_config();
        config.filename_exclude = vec![glob::Pattern::new("*.AAE").unwrap()];
        assert!(
            extract_skip_candidates(&asset, &config).is_empty(),
            "filename_exclude should filter in extract_skip_candidates"
        );
    }

    #[test]
    fn test_extract_skip_candidates_filename_exclude_no_match_passes() {
        let asset = TestPhotoAsset::new("TEST_1").build(); // filename = "test_photo.jpg"
        let mut config = test_config();
        config.filename_exclude = vec![glob::Pattern::new("*.AAE").unwrap()];
        assert!(
            !extract_skip_candidates(&asset, &config).is_empty(),
            "non-matching filename should pass through"
        );
    }

    #[test]
    fn test_extract_skip_candidates_filename_exclude_case_insensitive() {
        let asset = TestPhotoAsset::new("TEST_1").filename("photo.aae").build();
        let mut config = test_config();
        config.filename_exclude = vec![glob::Pattern::new("*.AAE").unwrap()];
        assert!(
            extract_skip_candidates(&asset, &config).is_empty(),
            "filename_exclude should be case-insensitive"
        );
    }

    // ── compute_config_hash: filename_exclude ─────────────────────

    #[test]
    fn test_compute_config_hash_different_filename_exclude() {
        let tmp = TempDir::new().unwrap();
        let a = build_config_with(tmp.path(), "/photos", |_| {});
        let b = build_config_with(tmp.path(), "/photos", |s| {
            s.filename_exclude = vec!["*.AAE".to_string()];
        });
        assert_ne!(
            compute_config_hash(&a),
            compute_config_hash(&b),
            "changing filename_exclude should change the config hash"
        );
    }
}
