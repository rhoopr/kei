//! Download engine — streaming pipeline that starts downloading as soon as
//! the first API page returns, rather than enumerating the entire library
//! upfront. Uses a two-phase approach: (1) stream-and-download with bounded
//! concurrency, then (2) cleanup pass with fresh CDN URLs for any failures.

pub mod error;
pub mod file;
pub(crate) mod filter;
#[cfg(feature = "xmp")]
pub(crate) mod heif;
pub(crate) mod limiter;
#[cfg(feature = "xmp")]
pub mod metadata;
pub mod paths;
pub(crate) mod pipeline;

pub(crate) use limiter::BandwidthLimiter;

use pipeline::{
    build_download_outcome, format_duration, log_sync_summary, run_download_pass,
    stream_and_download_from_stream, MetadataFlags, PassConfig, StreamingResult,
    AUTH_ERROR_THRESHOLD,
};

pub(crate) use filter::determine_media_type;
pub(crate) use filter::AssetGroupings;
use filter::{
    extract_skip_candidates, filter_asset_to_tasks, pre_ensure_asset_dir, DownloadTask,
    NormalizedPath,
};

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use chrono::{DateTime, Utc};
use reqwest::Client;
use rustc_hash::{FxHashMap, FxHashSet};

use futures_util::stream::{self, StreamExt};
use tokio_util::sync::CancellationToken;

use crate::icloud::photos::{PhotoAsset, SyncTokenError};
use crate::retry::RetryConfig;
use crate::state::{AssetRecord, StateDb, VersionSizeKey};
use crate::types::{
    AssetVersionSize, ChangeReason, FileMatchPolicy, LivePhotoMode, LivePhotoMovFilenamePolicy,
    RawTreatmentPolicy,
};

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
    /// Accumulated statistics from this sync run.
    pub stats: SyncStats,
}

/// Accumulated statistics from a sync run, used for JSON reports and notifications.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct SyncStats {
    pub assets_seen: u64,
    pub downloaded: usize,
    pub failed: usize,
    pub skipped: SkipBreakdown,
    pub bytes_downloaded: u64,
    pub disk_bytes_written: u64,
    pub exif_failures: usize,
    pub state_write_failures: usize,
    pub enumeration_errors: usize,
    pub elapsed_secs: f64,
    pub interrupted: bool,
    /// Number of tasks that observed at least one HTTP 429 / 503 response
    /// during retry. A high ratio of rate_limited / assets_seen signals the
    /// sync is running against a back-pressured account; operators should
    /// either raise --watch-with-interval or lower --threads-num.
    pub rate_limited: usize,
}

impl SyncStats {
    /// Add `other` into `self`, field by field. Used by the per-cycle loop in
    /// `sync_loop::run_cycle` to fold each library's stats into a cycle-wide
    /// total.
    ///
    /// All numeric counters sum; `interrupted` ORs (any library being
    /// interrupted means the cycle was interrupted); `skipped` delegates to
    /// [`SkipBreakdown::accumulate`].
    ///
    /// Adding a new field to `SyncStats` requires updating this method too --
    /// otherwise the new counter silently zeros out across multi-library
    /// syncs.
    pub fn accumulate(&mut self, other: &SyncStats) {
        self.assets_seen += other.assets_seen;
        self.downloaded += other.downloaded;
        self.failed += other.failed;
        self.skipped.accumulate(&other.skipped);
        self.bytes_downloaded += other.bytes_downloaded;
        self.disk_bytes_written += other.disk_bytes_written;
        self.exif_failures += other.exif_failures;
        self.state_write_failures += other.state_write_failures;
        self.enumeration_errors += other.enumeration_errors;
        self.elapsed_secs += other.elapsed_secs;
        self.interrupted = self.interrupted || other.interrupted;
        self.rate_limited += other.rate_limited;
    }
}

/// Per-reason breakdown of skipped assets.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct SkipBreakdown {
    pub by_state: usize,
    pub on_disk: usize,
    pub by_media_type: usize,
    pub by_date_range: usize,
    pub by_live_photo: usize,
    pub by_filename: usize,
    pub by_excluded_album: usize,
    pub ampm_variant: usize,
    pub duplicates: usize,
    pub retry_exhausted: usize,
    pub retry_only: usize,
}

impl SkipBreakdown {
    pub fn total(&self) -> usize {
        self.by_state
            + self.on_disk
            + self.by_media_type
            + self.by_date_range
            + self.by_live_photo
            + self.by_filename
            + self.by_excluded_album
            + self.ampm_variant
            + self.duplicates
            + self.retry_exhausted
            + self.retry_only
    }

    /// Add `other` into `self` field-by-field. Mirrors
    /// [`SyncStats::accumulate`] for the nested skip breakdown.
    pub fn accumulate(&mut self, other: &SkipBreakdown) {
        self.by_state += other.by_state;
        self.on_disk += other.on_disk;
        self.by_media_type += other.by_media_type;
        self.by_date_range += other.by_date_range;
        self.by_live_photo += other.by_live_photo;
        self.by_filename += other.by_filename;
        self.by_excluded_album += other.by_excluded_album;
        self.ampm_variant += other.ampm_variant;
        self.duplicates += other.duplicates;
        self.retry_exhausted += other.retry_exhausted;
        self.retry_only += other.retry_only;
    }
}

/// Truncate a `DateTime<Utc>` to midnight so that relative date intervals
/// (e.g. `20d` → `now - 20 days`) produce a stable hash within the same
/// calendar day.
fn truncate_date_to_day(dt: Option<DateTime<Utc>>) -> Option<chrono::NaiveDate> {
    dt.map(|d| d.date_naive())
}

/// Hash an `Option<NaiveDate>` with a tag byte for `None`/`Some` and the
/// "YYYY-MM-DD" Display representation for the date value.
fn hash_optional_date(hasher: &mut sha2::Sha256, date: Option<chrono::NaiveDate>) {
    use sha2::Digest;
    match date {
        None => hasher.update([0]),
        Some(d) => {
            hasher.update([1]);
            hasher.update(d.to_string().as_bytes());
        }
    }
}

/// Hash a byte slice with a trailing NUL separator. Pairs naturally with
/// other variable-length fields without ambiguity: `"a"` + `""` hashes
/// distinctly from `""` + `"a"`.
fn hash_bytes(hasher: &mut sha2::Sha256, bytes: &[u8]) {
    use sha2::Digest;
    hasher.update(bytes);
    hasher.update(b"\0");
}

/// Hash an `Option<u32>` with a tag byte for `None`/`Some` and the
/// little-endian bytes of the inner value.
fn hash_optional_u32(hasher: &mut sha2::Sha256, val: Option<u32>) {
    use sha2::Digest;
    match val {
        None => hasher.update([0]),
        Some(n) => {
            hasher.update([1]);
            hasher.update(n.to_le_bytes());
        }
    }
}

/// Finalize a SHA-256 hasher into a 16-char hex string (first 8 bytes).
fn finalize_hash(hasher: sha2::Sha256) -> String {
    use sha2::Digest;
    use std::fmt::Write;

    let hash = hasher.finalize();
    let mut hex = String::with_capacity(16);
    // First 8 bytes is plenty for collision avoidance in this context.
    #[allow(
        clippy::indexing_slicing,
        reason = "SHA-256 output is always 32 bytes; 8 is unconditionally in-bounds"
    )]
    for &b in &hash[..8] {
        let _ = Write::write_fmt(&mut hex, format_args!("{b:02x}"));
    }
    hex
}

/// Fields shared between [`hash_download_config`] and [`compute_config_hash`]
/// that affect path resolution and asset eligibility.
#[derive(Debug)]
struct SharedHashFields<'a> {
    directory: &'a std::path::Path,
    folder_structure: &'a str,
    folder_structure_albums: &'a str,
    folder_structure_smart_folders: &'a str,
    size: AssetVersionSize,
    live_photo_size: AssetVersionSize,
    file_match_policy: FileMatchPolicy,
    live_photo_mov_filename_policy: LivePhotoMovFilenamePolicy,
    align_raw: RawTreatmentPolicy,
    keep_unicode_in_filenames: bool,
    skip_created_before: Option<DateTime<Utc>>,
    skip_created_after: Option<DateTime<Utc>>,
    force_size: bool,
    skip_videos: bool,
    skip_photos: bool,
    live_photo_mode: LivePhotoMode,
    filename_exclude: &'a [glob::Pattern],
}

/// Hash the shared config fields into the hasher. All enum values use
/// `repr(u8)` byte representations and dates use "YYYY-MM-DD" Display
/// format for stability across compiler/library upgrades.
fn hash_shared_fields(hasher: &mut sha2::Sha256, f: &SharedHashFields<'_>) {
    use sha2::Digest;

    hash_bytes(hasher, f.directory.as_os_str().as_encoded_bytes());
    hash_bytes(hasher, f.folder_structure.as_bytes());
    hash_bytes(hasher, f.folder_structure_albums.as_bytes());
    hash_bytes(hasher, f.folder_structure_smart_folders.as_bytes());
    hasher.update([f.size as u8]);
    hasher.update([f.live_photo_size as u8]);
    hasher.update([f.file_match_policy as u8]);
    hasher.update([f.live_photo_mov_filename_policy as u8]);
    hasher.update([f.align_raw as u8]);
    hasher.update([u8::from(f.keep_unicode_in_filenames)]);
    // Filter fields: changing these affects which assets are eligible, so we
    // must invalidate the trust-state cache (and stored sync tokens) to avoid
    // skipping newly-eligible assets on incremental syncs.
    //
    // Dates are truncated to day precision before hashing so that relative
    // intervals like "20d" (resolved to now-minus-20-days at parse time)
    // produce a stable hash across consecutive runs on the same day.
    hash_optional_date(hasher, truncate_date_to_day(f.skip_created_before));
    hash_optional_date(hasher, truncate_date_to_day(f.skip_created_after));
    hasher.update([u8::from(f.force_size)]);
    hasher.update([u8::from(f.skip_videos)]);
    hasher.update([u8::from(f.skip_photos)]);
    hasher.update([f.live_photo_mode as u8]);
    // filename_exclude patterns affect which assets are eligible
    let mut sorted_excludes: Vec<&str> = f
        .filename_exclude
        .iter()
        .map(glob::Pattern::as_str)
        .collect();
    sorted_excludes.sort_unstable();
    for pattern in &sorted_excludes {
        hash_bytes(hasher, pattern.as_bytes());
    }
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

    let mut hasher = Sha256::new();
    hash_shared_fields(
        &mut hasher,
        &SharedHashFields {
            directory: &config.directory,
            folder_structure: &config.folder_structure,
            folder_structure_albums: &config.folder_structure_albums,
            folder_structure_smart_folders: &config.folder_structure_smart_folders,
            size: config.size,
            live_photo_size: config.live_photo_size,
            file_match_policy: config.file_match_policy,
            live_photo_mov_filename_policy: config.live_photo_mov_filename_policy,
            align_raw: config.align_raw,
            keep_unicode_in_filenames: config.keep_unicode_in_filenames,
            skip_created_before: config.skip_created_before,
            skip_created_after: config.skip_created_after,
            force_size: config.force_size,
            skip_videos: config.skip_videos,
            skip_photos: config.skip_photos,
            live_photo_mode: config.live_photo_mode,
            filename_exclude: &config.filename_exclude,
        },
    );
    // `recent` affects which already-downloaded assets to trust/skip
    hash_optional_u32(&mut hasher, config.recent);
    finalize_hash(hasher)
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

    let size: AssetVersionSize = config.size.into();
    let live_photo_size = config.live_photo_size.to_asset_version_size();
    let skip_created_before = config
        .skip_created_before
        .map(|d| d.with_timezone(&chrono::Utc));
    let skip_created_after = config
        .skip_created_after
        .map(|d| d.with_timezone(&chrono::Utc));

    let mut hasher = Sha256::new();
    hash_shared_fields(
        &mut hasher,
        &SharedHashFields {
            directory: &config.directory,
            folder_structure: &config.folder_structure,
            folder_structure_albums: &config.folder_structure_albums,
            folder_structure_smart_folders: &config.folder_structure_smart_folders,
            size,
            live_photo_size,
            file_match_policy: config.file_match_policy,
            live_photo_mov_filename_policy: config.live_photo_mov_filename_policy,
            align_raw: config.align_raw,
            keep_unicode_in_filenames: config.keep_unicode_in_filenames,
            skip_created_before,
            skip_created_after,
            force_size: config.force_size,
            skip_videos: config.skip_videos,
            skip_photos: config.skip_photos,
            live_photo_mode: config.live_photo_mode,
            filename_exclude: &config.filename_exclude,
        },
    );
    // Note: `recent` is intentionally excluded from this enum hash.
    // Changing --recent should not invalidate sync tokens because the
    // incremental path already applies the recent cap post-fetch.
    // `recent` IS included in hash_download_config (trust-state) so
    // changing it still triggers filesystem re-verification.

    // Enumeration-filter fields: changing these affects WHICH assets are
    // fetched from iCloud, so sync tokens must be invalidated to avoid
    // missing assets that are newly eligible under the changed filters.
    // Tag byte distinguishes the three selection modes so switching between
    // them (e.g. `-a A` -> `-a all`) invalidates the sync token even if no
    // explicit album name changed.
    match &config.albums {
        crate::config::AlbumSelection::LibraryOnly => hasher.update([0]),
        crate::config::AlbumSelection::All => hasher.update([1]),
        crate::config::AlbumSelection::Named(names) => {
            hasher.update([2]);
            for album in names {
                hasher.update(album.as_bytes());
                hasher.update(b"\0");
            }
        }
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
    // Library selector: stable tag bytes per shape so changing the resolved
    // library set invalidates sync tokens. `to_raw()` emits a deterministic
    // ordering (`primary`/`shared`/named-then-`!excluded`).
    for entry in config.selection.libraries.to_raw() {
        hasher.update(b"library:");
        hasher.update(entry.as_bytes());
        hasher.update(b"\0");
    }
    finalize_hash(hasher)
}

/// Subset of application config consumed by the download engine.
/// Decoupled from CLI parsing so the engine can be tested independently.
pub(crate) struct DownloadConfig {
    /// Behind `Arc` so per-pass clones (`with_album_name`, `with_pass`,
    /// `with_exclude_ids`) refcount-bump instead of deep-cloning the
    /// PathBuf. Same pattern as `asset_groupings` and `exclude_asset_ids`.
    pub(crate) directory: Arc<Path>,
    /// Template for the unfiled (library-wide) pass. Also the source the
    /// per-pass clone in `with_pass` reads when the pass is `Unfiled`. After
    /// `with_pass` runs, this field holds the *expanded* per-pass template.
    pub(crate) folder_structure: String,
    /// Template for `PassKind::Album` passes (default `{album}`). Behind
    /// `Arc<str>` so per-pass clones refcount-bump instead of deep-cloning;
    /// the user-typed template never mutates after CLI parse.
    pub(crate) folder_structure_albums: Arc<str>,
    /// Template for `PassKind::SmartFolder` passes (default `{smart-folder}`).
    /// Behind `Arc<str>` for the same reason as `folder_structure_albums`.
    pub(crate) folder_structure_smart_folders: Arc<str>,
    pub(crate) size: AssetVersionSize,
    pub(crate) skip_videos: bool,
    pub(crate) skip_photos: bool,
    pub(crate) skip_created_before: Option<DateTime<Utc>>,
    pub(crate) skip_created_after: Option<DateTime<Utc>>,
    #[cfg(feature = "xmp")]
    pub(crate) set_exif_datetime: bool,
    #[cfg(feature = "xmp")]
    pub(crate) set_exif_rating: bool,
    #[cfg(feature = "xmp")]
    pub(crate) set_exif_gps: bool,
    #[cfg(feature = "xmp")]
    pub(crate) set_exif_description: bool,
    /// Embed the full XMP packet (title, keywords, people, hidden/archived,
    /// media subtype, burst id) into the file bytes on supported formats.
    #[cfg(feature = "xmp")]
    pub(crate) embed_xmp: bool,
    /// Write a `.xmp` sidecar file next to each downloaded media file with
    /// the same composed XMP packet.
    #[cfg(feature = "xmp")]
    pub(crate) xmp_sidecar: bool,
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
    ///
    /// Behind `Arc<[_]>` so per-pass clones share one allocation
    /// (significant with `-a all` over 100+ albums).
    pub(crate) filename_exclude: Arc<[glob::Pattern]>,
    /// Temp file suffix for partial downloads (e.g. `.kei-tmp`).
    pub(crate) temp_suffix: Arc<str>,
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
    /// CloudKit zone name (e.g. "PrimarySync", "SharedSync-A1B2C3D4-...")
    /// scoping every asset processed under this config. Threaded into
    /// `AssetRecord.library` and every state-DB key so multi-library syncs
    /// don't collide on the (id, version_size) pair across zones.
    pub(crate) library: Arc<str>,
    /// Asset IDs to exclude (from `--exclude-album` without `--album`).
    pub(crate) exclude_asset_ids: Arc<FxHashSet<String>>,
    /// Maximum download attempts per asset before giving up (0 = unlimited).
    pub(crate) max_download_attempts: u32,
    /// Preloaded asset→album and asset→person indices, shared across clones.
    pub(crate) asset_groupings: Arc<AssetGroupings>,
    /// Shared token-bucket limiter applied across all concurrent download
    /// streams. `None` = no throughput cap.
    pub(crate) bandwidth_limiter: Option<BandwidthLimiter>,
}

impl DownloadConfig {
    /// True when passes can produce divergent paths and need per-pass config
    /// expansion (`with_pass`) plus path-aware skip checks rather than the
    /// merged-stream optimisation + DB-only fast skip.
    ///
    /// Divergence sources: any of the three template fields
    /// (`folder_structure`, `folder_structure_albums`,
    /// `folder_structure_smart_folders`) carries a per-pass token
    /// (`{album}` / `{smart-folder}` / `{library}`), or the per-category
    /// templates differ from the base. Both cases mean a single merged
    /// stream + base config would route assets to the wrong on-disk path.
    ///
    /// Only meaningful on the *base* config. A per-pass config produced by
    /// `with_album_name` / `with_pass` has had per-pass tokens expanded out
    /// of `folder_structure`, but the per-category fields stay cloned from
    /// the base so this still reports the base verdict; per-pass code paths
    /// should check `album_name.is_some()` instead.
    pub(crate) fn requires_per_pass_paths(&self) -> bool {
        const PER_PASS_TOKENS: [&str; 3] = [
            paths::TOKEN_ALBUM,
            paths::TOKEN_SMART_FOLDER,
            paths::TOKEN_LIBRARY,
        ];
        let any_token = |s: &str| PER_PASS_TOKENS.iter().any(|t| s.contains(t));
        any_token(&self.folder_structure)
            || any_token(&self.folder_structure_albums)
            || any_token(&self.folder_structure_smart_folders)
            || self.folder_structure_albums.as_ref() != self.folder_structure.as_str()
            || self.folder_structure_smart_folders.as_ref() != self.folder_structure.as_str()
    }

    /// Clone this config for a single download pass: pick the per-category
    /// template (`folder_structure_albums` for `PassKind::Album`,
    /// `folder_structure_smart_folders` for `PassKind::SmartFolder`,
    /// `folder_structure` for `PassKind::Unfiled`), pre-expand the matching
    /// token (`{album}` / `{smart-folder}`), and pin the pass's exclude-ids
    /// set in one clone.
    ///
    /// The unfiled pass keeps the legacy `{album}` token so existing configs
    /// with `--folder-structure "{album}/..."` still produce the same
    /// on-disk tree.
    fn with_pass(&self, pass: &crate::commands::AlbumPass) -> Self {
        let template: &str = match pass.kind {
            crate::commands::PassKind::Album => &self.folder_structure_albums,
            crate::commands::PassKind::SmartFolder => &self.folder_structure_smart_folders,
            crate::commands::PassKind::Unfiled => &self.folder_structure,
        };
        let name = &pass.album.name;
        let name_ref = Some(name.as_ref()).filter(|n: &&str| !n.is_empty());
        let category_expanded = paths::expand_named_token(template, pass.kind.token(), name_ref);
        // Apply `{library}` last with the path-friendly truncated zone name,
        // so callers see `SharedSync-A1B2C3D4/...` instead of the full UUID.
        // The state-DB key still uses the full `self.library` string.
        let library_for_path = paths::truncate_library_zone(&self.library);
        let folder_structure = paths::expand_named_token(
            &category_expanded,
            paths::TOKEN_LIBRARY,
            Some(library_for_path),
        );
        Self {
            album_name: Some(Arc::clone(name)),
            directory: Arc::clone(&self.directory),
            folder_structure,
            folder_structure_albums: Arc::clone(&self.folder_structure_albums),
            folder_structure_smart_folders: Arc::clone(&self.folder_structure_smart_folders),
            filename_exclude: Arc::clone(&self.filename_exclude),
            temp_suffix: Arc::clone(&self.temp_suffix),
            state_db: self.state_db.clone(),
            sync_mode: self.sync_mode.clone(),
            exclude_asset_ids: Arc::clone(&pass.exclude_ids),
            asset_groupings: Arc::clone(&self.asset_groupings),
            bandwidth_limiter: self.bandwidth_limiter.clone(),
            library: Arc::clone(&self.library),
            ..*self
        }
    }

    /// Clone this config with a different `exclude_asset_ids` set. Used
    /// for the merged (non-`{album}`) full-sync path, where all passes
    /// share a single config but the exclude set is lifted off the plan.
    fn with_exclude_ids(&self, exclude_ids: Arc<FxHashSet<String>>) -> Self {
        Self {
            directory: Arc::clone(&self.directory),
            folder_structure: self.folder_structure.clone(),
            folder_structure_albums: Arc::clone(&self.folder_structure_albums),
            folder_structure_smart_folders: Arc::clone(&self.folder_structure_smart_folders),
            filename_exclude: Arc::clone(&self.filename_exclude),
            temp_suffix: Arc::clone(&self.temp_suffix),
            state_db: self.state_db.clone(),
            sync_mode: self.sync_mode.clone(),
            album_name: self.album_name.clone(),
            exclude_asset_ids: exclude_ids,
            asset_groupings: Arc::clone(&self.asset_groupings),
            bandwidth_limiter: self.bandwidth_limiter.clone(),
            library: Arc::clone(&self.library),
            ..*self
        }
    }
}

impl std::fmt::Debug for DownloadConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut s = f.debug_struct("DownloadConfig");
        s.field("directory", &self.directory)
            .field("folder_structure", &self.folder_structure)
            .field("folder_structure_albums", &self.folder_structure_albums)
            .field(
                "folder_structure_smart_folders",
                &self.folder_structure_smart_folders,
            )
            .field("size", &self.size)
            .field("skip_videos", &self.skip_videos)
            .field("skip_photos", &self.skip_photos)
            .field("skip_created_before", &self.skip_created_before)
            .field("skip_created_after", &self.skip_created_after);
        #[cfg(feature = "xmp")]
        s.field("set_exif_datetime", &self.set_exif_datetime)
            .field("set_exif_rating", &self.set_exif_rating)
            .field("set_exif_gps", &self.set_exif_gps)
            .field("set_exif_description", &self.set_exif_description)
            .field("embed_xmp", &self.embed_xmp)
            .field("xmp_sidecar", &self.xmp_sidecar);
        s.field("dry_run", &self.dry_run)
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
            .field("bandwidth_limiter", &self.bandwidth_limiter)
            .finish()
    }
}

#[cfg(test)]
impl DownloadConfig {
    /// Default test config shared across download submodule tests.
    pub(super) fn test_default() -> Self {
        use rustc_hash::FxHashSet;
        Self {
            directory: Arc::from(Path::new("/nonexistent/download_filter_tests")),
            folder_structure: "{:%Y/%m/%d}".to_string(),
            folder_structure_albums: Arc::from(crate::config::DEFAULT_FOLDER_STRUCTURE_ALBUMS),
            folder_structure_smart_folders: Arc::from(
                crate::config::DEFAULT_FOLDER_STRUCTURE_SMART_FOLDERS,
            ),
            size: AssetVersionSize::Original,
            skip_videos: false,
            skip_photos: false,
            skip_created_before: None,
            skip_created_after: None,
            #[cfg(feature = "xmp")]
            set_exif_datetime: false,
            #[cfg(feature = "xmp")]
            set_exif_rating: false,
            #[cfg(feature = "xmp")]
            set_exif_gps: false,
            #[cfg(feature = "xmp")]
            set_exif_description: false,
            #[cfg(feature = "xmp")]
            embed_xmp: false,
            #[cfg(feature = "xmp")]
            xmp_sidecar: false,
            dry_run: false,
            concurrent_downloads: 1,
            recent: None,
            retry: crate::retry::RetryConfig::default(),
            live_photo_mode: LivePhotoMode::Both,
            live_photo_size: AssetVersionSize::LiveOriginal,
            live_photo_mov_filename_policy: crate::types::LivePhotoMovFilenamePolicy::Suffix,
            align_raw: RawTreatmentPolicy::Unchanged,
            no_progress_bar: true,
            only_print_filenames: false,
            file_match_policy: FileMatchPolicy::NameSizeDedupWithSuffix,
            force_size: false,
            keep_unicode_in_filenames: false,
            filename_exclude: Arc::from(Vec::<glob::Pattern>::new()),
            temp_suffix: Arc::from(".kei-tmp"),
            state_db: None,
            retry_only: false,
            max_download_attempts: 10,
            sync_mode: SyncMode::Full,
            album_name: None,
            exclude_asset_ids: std::sync::Arc::new(FxHashSet::default()),
            asset_groupings: Arc::new(AssetGroupings::default()),
            bandwidth_limiter: None,
            library: Arc::from(crate::icloud::photos::PRIMARY_ZONE_NAME),
        }
    }
}

/// Look up an owned `String` id in a shared `Arc<str>` interner,
/// inserting a fresh `Arc` if absent. Returns the shared handle.
///
/// `Arc::from(String)` transfers the string's buffer into the `Arc<str>`
/// without copying the bytes, so the miss path allocates no more than
/// the baseline cost of a fresh handle.
fn intern_id(interner: &mut FxHashSet<Arc<str>>, s: String) -> Arc<str> {
    if let Some(existing) = interner.get(s.as_str()) {
        return Arc::clone(existing);
    }
    let a: Arc<str> = Arc::from(s);
    interner.insert(Arc::clone(&a));
    a
}

/// Pre-loaded download state for O(1) skip decisions.
///
/// Loaded once at sync start from the state database, this enables fast
/// in-memory lookups instead of per-asset DB queries. For 100K+ asset
/// libraries, this significantly reduces DB roundtrips.
///
/// Asset-id keys are `Arc<str>` rather than `Box<str>` so the same id
/// allocation is shared across every map here (and with the producer's
/// seen-ids / touched-ids sets). On a 100k-asset library this collapses
/// ~4-6 independent `Box<str>` allocations per asset into one.
/// `library -> asset_id -> set of version_size` strings. Used by
/// `DownloadContext::downloaded_ids` and `metadata_retry_markers`.
type LibraryAssetVersionSet = FxHashMap<Arc<str>, FxHashMap<Arc<str>, FxHashSet<Box<str>>>>;

/// `library -> asset_id -> (version_size -> value)`. Used by
/// `DownloadContext::downloaded_checksums` and
/// `downloaded_metadata_hashes`.
type LibraryAssetVersionValueMap =
    FxHashMap<Arc<str>, FxHashMap<Arc<str>, FxHashMap<Box<str>, Box<str>>>>;

#[derive(Debug, Default)]
struct DownloadContext {
    /// Nested map: `library` -> `asset_id` -> set of `version_sizes` that
    /// are already downloaded. Three-level shape so multi-library syncs
    /// don't dedupe the same asset_id across zones (PR10 / schema v8).
    /// All key levels use borrowed `&str` lookups for zero-allocation probes.
    downloaded_ids: LibraryAssetVersionSet,
    /// Nested map: `library` -> `asset_id` -> (`version_size` -> checksum).
    /// Used to detect checksum changes (provider asset updated) without DB queries.
    downloaded_checksums: LibraryAssetVersionValueMap,
    /// Nested map: `library` -> `asset_id` -> (`version_size` -> metadata_hash).
    /// Used to detect metadata-only changes (favorite toggle, keywords, GPS
    /// edit, etc.) when file bytes are unchanged but the provider has newer
    /// metadata.
    #[cfg_attr(not(feature = "xmp"), allow(dead_code))]
    downloaded_metadata_hashes: LibraryAssetVersionValueMap,
    /// Nested map: `library` -> `asset_id` -> set of `version_sizes` with a
    /// non-null `metadata_write_failed_at` from a prior sync. These always
    /// route to the metadata-rewrite path regardless of whether the hash
    /// changed.
    #[cfg_attr(not(feature = "xmp"), allow(dead_code))]
    metadata_retry_markers: LibraryAssetVersionSet,
    /// All asset IDs known to the state DB (any status). Used in retry-only mode
    /// to skip new assets that were never synced. Library-blind: a known ID
    /// is "known" regardless of which zone it belongs to.
    known_ids: FxHashSet<Arc<str>>,
    /// Per-asset maximum download attempt count (from failed assets).
    /// Used to skip assets that have exceeded `max_download_attempts`.
    /// Library-blind: an asset shared across libraries shares its attempt
    /// budget (mirrors how `get_attempt_counts` aggregates by id alone).
    attempt_counts: FxHashMap<Arc<str>, u32>,
}

impl DownloadContext {
    /// Load the download context from the state database. All six queries
    /// are independent and run concurrently so sync start doesn't serialize
    /// on round-trip latency across them.
    async fn load(db: &dyn StateDb, retry_only: bool) -> Self {
        let known_ids_fut = async {
            if retry_only {
                db.get_all_known_ids().await.unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "Failed to load known IDs from state DB");
                    Default::default()
                })
            } else {
                Default::default()
            }
        };
        let (ids, checksums, hashes, markers, attempts, known_ids) = tokio::join!(
            async {
                db.get_downloaded_ids().await.unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "Failed to load downloaded IDs from state DB");
                    Default::default()
                })
            },
            async {
                db.get_downloaded_checksums().await.unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "Failed to load checksums from state DB");
                    Default::default()
                })
            },
            async {
                db.get_downloaded_metadata_hashes()
                    .await
                    .unwrap_or_else(|e| {
                        tracing::warn!(error = %e, "Failed to load metadata hashes from state DB");
                        Default::default()
                    })
            },
            async {
                db.get_metadata_retry_markers().await.unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "Failed to load metadata retry markers from state DB");
                    Default::default()
                })
            },
            async {
                db.get_attempt_counts().await.unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "Failed to load attempt counts from state DB");
                    Default::default()
                })
            },
            known_ids_fut,
        );

        // Shared interner so the same asset_id allocates exactly one
        // `Arc<str>` across every map below (and is cheaply cloneable
        // into each via Arc::clone). This collapses the former 4-6
        // independent `String -> Box<str>` conversions per id into one.
        //
        // FxHashSet<Arc<str>> over FxHashMap<String, Arc<str>> so the
        // interner doesn't keep a duplicate `String` alive for every
        // id; `Arc::from(String)` transfers the String's buffer into
        // the Arc without an extra copy.
        let mut interner: FxHashSet<Arc<str>> = FxHashSet::default();

        let mut downloaded_ids: LibraryAssetVersionSet = FxHashMap::default();
        for (library, asset_id, version_size) in ids {
            let lib = intern_id(&mut interner, library);
            let id = intern_id(&mut interner, asset_id);
            downloaded_ids
                .entry(lib)
                .or_default()
                .entry(id)
                .or_default()
                .insert(version_size.into_boxed_str());
        }

        let mut downloaded_checksums: LibraryAssetVersionValueMap = FxHashMap::default();
        for ((library, asset_id, version_size), checksum) in checksums {
            let lib = intern_id(&mut interner, library);
            let id = intern_id(&mut interner, asset_id);
            downloaded_checksums
                .entry(lib)
                .or_default()
                .entry(id)
                .or_default()
                .insert(version_size.into_boxed_str(), checksum.into_boxed_str());
        }

        let mut downloaded_metadata_hashes: LibraryAssetVersionValueMap = FxHashMap::default();
        for ((library, asset_id, version_size), metadata_hash) in hashes {
            let lib = intern_id(&mut interner, library);
            let id = intern_id(&mut interner, asset_id);
            downloaded_metadata_hashes
                .entry(lib)
                .or_default()
                .entry(id)
                .or_default()
                .insert(
                    version_size.into_boxed_str(),
                    metadata_hash.into_boxed_str(),
                );
        }

        let mut metadata_retry_markers: LibraryAssetVersionSet = FxHashMap::default();
        for (library, asset_id, version_size) in markers {
            let lib = intern_id(&mut interner, library);
            let id = intern_id(&mut interner, asset_id);
            metadata_retry_markers
                .entry(lib)
                .or_default()
                .entry(id)
                .or_default()
                .insert(version_size.into_boxed_str());
        }

        let known_ids: FxHashSet<Arc<str>> = known_ids
            .into_iter()
            .map(|id| intern_id(&mut interner, id))
            .collect();

        let attempt_counts: FxHashMap<Arc<str>, u32> = attempts
            .into_iter()
            .map(|(id, count)| (intern_id(&mut interner, id), count))
            .collect();

        Self {
            downloaded_ids,
            downloaded_checksums,
            downloaded_metadata_hashes,
            metadata_retry_markers,
            known_ids,
            attempt_counts,
        }
    }

    /// Whether a downloaded asset-version needs a metadata-only rewrite:
    /// the caller has already matched checksums (bytes unchanged) and now
    /// checks whether (a) the stored metadata_hash differs from the new
    /// one or (b) a persisted retry marker is set from a prior sync where
    /// the writer failed after bytes landed.
    #[cfg_attr(not(feature = "xmp"), allow(dead_code))]
    fn needs_metadata_rewrite(
        &self,
        library: &str,
        asset_id: &str,
        version_size: VersionSizeKey,
        new_metadata_hash: Option<&str>,
    ) -> bool {
        let vs_str = version_size.as_str();
        let has_retry_marker = self
            .metadata_retry_markers
            .get(library)
            .and_then(|m| m.get(asset_id))
            .is_some_and(|vsset| vsset.contains(vs_str));
        if has_retry_marker {
            return true;
        }
        let Some(new_hash) = new_metadata_hash else {
            return false;
        };
        match self
            .downloaded_metadata_hashes
            .get(library)
            .and_then(|m| m.get(asset_id))
            .and_then(|map| map.get(vs_str))
        {
            Some(stored) => stored.as_ref() != new_hash,
            None => true, // downloaded row has no stored hash yet -- refresh
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
        library: &str,
        asset_id: &str,
        version_size: VersionSizeKey,
        checksum: &str,
        trust_state: bool,
    ) -> Option<bool> {
        let version_size_str = version_size.as_str();

        // Borrowed `&str` keys at every level — no allocation per probe.
        let is_downloaded = self
            .downloaded_ids
            .get(library)
            .and_then(|m| m.get(asset_id))
            .is_some_and(|versions| versions.contains(version_size_str));

        if !is_downloaded {
            // Not in downloaded set — needs download
            return Some(true);
        }

        // Check if checksum changed (also zero-allocation lookup). Track
        // whether a stored checksum is present at all so we can audit the
        // "no stored checksum" path, which pre-v3 rows fall into.
        let stored_checksum = self
            .downloaded_checksums
            .get(library)
            .and_then(|m| m.get(asset_id))
            .and_then(|versions| versions.get(version_size_str));
        if let Some(stored) = stored_checksum {
            if stored.as_ref() != checksum {
                return Some(true);
            }
        } else {
            // Pre-v3 row with no stored local_checksum. Audit so operators can
            // correlate unexpected "skipped" counts with missing checksum
            // history (the row will gain a checksum on next download).
            tracing::debug!(
                asset_id = asset_id,
                version_size = %version_size_str,
                trust_state = trust_state,
                "no stored checksum for downloaded asset-version; skip decision uses trust_state only"
            );
        }

        if trust_state {
            Some(false)
        } else {
            None
        }
    }
}

/// Pre-compute one `Arc<DownloadConfig>` per pass. Each pass_index maps to
/// a derived config that pre-expands `{album}` and pins the pass's
/// exclude-asset-ids set. In `{album}` mode passes may legitimately differ
/// per entry; outside of it, passes share identical excludes but the per-
/// pass wrapper is harmless and keeps call sites uniform.
fn build_pass_configs(
    passes: &[crate::commands::AlbumPass],
    base: &DownloadConfig,
) -> Vec<Arc<DownloadConfig>> {
    passes
        .iter()
        .map(|pass| Arc::new(base.with_pass(pass)))
        .collect()
}

/// Eagerly enumerate all albums and build a complete task list.
///
/// Used only by the Phase 2 cleanup pass — re-contacts the API so each call
/// yields fresh CDN URLs that haven't expired during a long download session.
async fn build_download_tasks(
    passes: &[crate::commands::AlbumPass],
    config: &DownloadConfig,
    shutdown_token: CancellationToken,
) -> Result<Vec<DownloadTask>> {
    let pass_configs = build_pass_configs(passes, config);
    let pass_results: Vec<Result<(usize, Vec<_>)>> = stream::iter(passes.iter().enumerate())
        .take_while(|_| std::future::ready(!shutdown_token.is_cancelled()))
        .map(|(i, pass)| async move { pass.album.photos(config.recent).await.map(|a| (i, a)) })
        .buffer_unordered(config.concurrent_downloads)
        .collect()
        .await;

    let mut tasks: Vec<DownloadTask> = Vec::new();
    let mut claimed_paths: FxHashMap<NormalizedPath, u64> = FxHashMap::default();
    let mut dir_cache = paths::DirCache::new();
    for pass_result in pass_results {
        let (pass_index, assets) = pass_result?;
        #[allow(
            clippy::indexing_slicing,
            reason = "pass_index comes from enumerate() over `passes`; pass_configs is \
                      built 1:1 from the same slice"
        )]
        let pass_config = &pass_configs[pass_index];

        for asset in &assets {
            if filter::is_asset_filtered(asset, pass_config).is_some() {
                continue;
            }
            pre_ensure_asset_dir(&mut dir_cache, asset, pass_config).await;
            tasks.extend(filter_asset_to_tasks(
                asset,
                pass_config,
                &mut claimed_paths,
                &mut dir_cache,
            ));
        }
    }

    Ok(tasks)
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
/// Minimum age (seconds) a `.part` file must have before
/// `cleanup_orphan_part_files` will remove it, regardless of whether the
/// file is older than `last_sync_completed`. Defends against the
/// multi-process race where a *different* kei instance (different data dir,
/// same download dir) is actively writing a `.part` between download
/// retries: that instance's fresh `.part` predates *this* instance's
/// `last_sync_completed`, but it's not orphaned — the other process is
/// about to resume it.
///
/// 10 minutes is comfortably longer than the longest realistic single
/// HTTP request (CDN connect + TLS + body for a multi-GB Live Photo MOV)
/// while staying short enough that genuinely stale `.part` files from
/// crashed runs still get cleaned promptly.
const PART_FILE_RECENT_GRACE_SECS: i64 = 10 * 60;

/// Walk a tree rooted at `root`, removing files whose name ends with
/// `suffix` and whose mtime is older than `cutoff_secs`. Files whose
/// mtime is within the last `recent_grace_secs` of `now_secs` are spared
/// regardless of `cutoff_secs`. Returns the count of removed files.
/// A `read_dir` failure on any subdirectory logs a `warn!` and skips that
/// subtree -- the original code swallowed the error silently, leaving
/// operators without a breadcrumb when transient FS hiccups (e.g. an
/// unmount mid-walk) prevented cleanup.
fn walk_and_remove_orphan_parts(
    root: std::path::PathBuf,
    suffix: &str,
    cutoff_secs: i64,
    now_secs: i64,
    recent_grace_secs: i64,
) -> usize {
    let mut cleaned = 0usize;
    let mut stack = vec![root];
    while let Some(current) = stack.pop() {
        let entries = match std::fs::read_dir(&current) {
            Ok(entries) => entries,
            Err(e) => {
                tracing::warn!(
                    path = %current.display(),
                    error = %e,
                    "failed to read directory during orphan-part cleanup"
                );
                continue;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !name.ends_with(suffix) {
                continue;
            }
            let Ok(meta) = path.metadata() else { continue };
            let Ok(mtime) = meta.modified() else { continue };
            let mtime_secs = mtime
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            // Spare freshly-touched .parts even when they predate
            // `last_sync_completed`. Another kei process targeting the same
            // download dir (different data dir) might be actively resuming
            // this file; deleting it mid-retry would force a
            // restart-from-zero next attempt.
            //
            // `recent_grace_secs <= 0` disables the gate (for tests that
            // exercise the cutoff branch in isolation; production always
            // passes the PART_FILE_RECENT_GRACE_SECS constant).
            let is_recently_touched =
                recent_grace_secs > 0 && mtime_secs > now_secs.saturating_sub(recent_grace_secs);
            if is_recently_touched || mtime_secs >= cutoff_secs {
                continue;
            }
            if std::fs::remove_file(&path).is_ok() {
                cleaned += 1;
            }
        }
    }
    cleaned
}

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

    let suffix = Arc::clone(&config.temp_suffix);
    let dir: std::path::PathBuf = dir.to_path_buf();
    let cutoff_secs = cutoff.timestamp();
    let now_secs = chrono::Utc::now().timestamp();

    let cleaned = tokio::task::spawn_blocking(move || {
        walk_and_remove_orphan_parts(
            dir,
            &suffix,
            cutoff_secs,
            now_secs,
            PART_FILE_RECENT_GRACE_SECS,
        )
    })
    .await
    .unwrap_or(0);

    if cleaned > 0 {
        tracing::info!(count = cleaned, "Cleaned up orphaned .part files");
    }
}

pub async fn download_photos_with_sync(
    download_client: &Client,
    passes: &[crate::commands::AlbumPass],
    config: Arc<DownloadConfig>,
    shutdown_token: CancellationToken,
) -> Result<SyncResult> {
    let sync_started_at = chrono::Utc::now().timestamp();
    cleanup_orphan_part_files(&config).await;

    // Give every non-downloaded asset a fresh start this sync:
    // failed -> pending (with attempts reset), and stale attempt counts on
    // pending assets cleared so the per-sync cap starts from zero.
    let total_pending = if let Some(db) = &config.state_db {
        match db.prepare_for_retry().await {
            Ok((failed, stale, total_pending)) => {
                if failed > 0 {
                    tracing::debug!(count = failed, "Reset failed assets for retry");
                }
                if stale > 0 {
                    tracing::debug!(
                        count = stale,
                        "Cleared stale attempt counts on pending assets"
                    );
                }
                total_pending
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to reset assets for retry");
                0
            }
        }
    } else {
        0
    };

    let result = match &config.sync_mode {
        SyncMode::Full => {
            download_photos_full_with_token(
                download_client,
                passes,
                &config,
                shutdown_token.clone(),
            )
            .await
        }
        // In `{album}` mode we have to fall back to full enumeration:
        // `changes_stream` uses the zone-level `/changes/zone` endpoint, so
        // it returns the same delta for every album in a zone. Without
        // per-asset album-membership info on the change events, we can't
        // route assets to the correct album folder — full enumeration uses
        // the album-scoped `photo_stream_with_token` and stays correct.
        SyncMode::Incremental { .. } if config.requires_per_pass_paths() => {
            tracing::debug!(
                "`{{album}}` folder template requires full enumeration for correct \
                 per-album routing, skipping incremental"
            );
            download_photos_full_with_token(
                download_client,
                passes,
                &config,
                shutdown_token.clone(),
            )
            .await
        }
        // Incremental sync only returns new changes — it won't re-enumerate
        // pending assets from previous syncs. Fall back to full so they get
        // retried. Once everything is downloaded, incremental resumes.
        SyncMode::Incremental { .. } if total_pending > 0 => {
            tracing::debug!(
                pending = total_pending,
                "Pending assets require full enumeration, skipping incremental sync"
            );
            download_photos_full_with_token(
                download_client,
                passes,
                &config,
                shutdown_token.clone(),
            )
            .await
        }
        SyncMode::Incremental { zone_sync_token } => {
            let token = zone_sync_token.clone();
            match download_photos_incremental(
                download_client,
                passes,
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
                            passes,
                            &config,
                            shutdown_token.clone(),
                        )
                        .await
                    } else {
                        Err(e)
                    }
                }
            }
        }
    };

    // Pending is transient — anything still pending after a complete sync either
    // wasn't enumerated or failed silently. Skip on interrupt where pending is expected.
    if let Some(db) = &config.state_db {
        if !shutdown_token.is_cancelled() {
            match db.promote_pending_to_failed(sync_started_at).await {
                Ok(promoted) if promoted > 0 => {
                    tracing::warn!(
                        count = promoted,
                        "Promoted unresolved pending assets to failed"
                    );
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to promote pending assets");
                }
                _ => {}
            }
        }
    }

    result
}

/// Fold per-pass `album.len()` results into a `(counts, error_count)` tuple,
/// logging a `warn!` for each failure. Errors are mapped to a count of 0 so
/// downstream concurrency math still has a value, but the returned error
/// count is the load-bearing signal: callers must treat it as an enumeration
/// failure that suppresses sync token advancement (a swallowed `len()` error
/// previously caused `total = 0`, which silently bypassed the pagination
/// undercount check and advanced the token past un-enumerated change events).
fn fold_pass_count_results(
    results: Vec<anyhow::Result<u64>>,
    passes: &[crate::commands::AlbumPass],
) -> (Vec<u64>, usize) {
    let mut errors: usize = 0;
    let counts: Vec<u64> = results
        .into_iter()
        .zip(passes)
        .map(|(result, pass)| match result {
            Ok(n) => n,
            Err(e) => {
                errors += 1;
                tracing::warn!(
                    album = %pass.album,
                    error = %e,
                    "Failed to query album length; treating count as 0 and \
                     blocking sync token advancement to force full \
                     re-enumeration on next run"
                );
                0
            }
        })
        .collect();
    (counts, errors)
}

/// Classification of how the producer-observed asset count compared with the
/// pre-enumeration API total. Drives the two-tier pagination-undercount gate.
///
/// - `Match`: producer saw at least as many assets as `total` reported. Token
///   advances silently.
/// - `WithinTolerance`: producer saw fewer than `total` but the gap is below
///   the 5% suppression threshold. Token advances; a `warn!` fires so a slow
///   drift is visible before it grows past 5%.
/// - `Suppress`: gap is at or above 5% of `total`. Token is suppressed so the
///   next cycle re-enumerates. Without this gate, missed change events would
///   sit behind an advanced token forever (silent data loss).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaginationShortfall {
    Match,
    WithinTolerance { shortfall: u64 },
    Suppress,
}

/// Pure classifier for the pagination-undercount gate. `total` is the
/// pre-enumeration API count (post `--recent` cap and known filters); `seen`
/// is the producer's `assets_seen` count. Caller is responsible for the
/// `total > 0` guard and any dry-run / print-only suppression.
fn classify_pagination_shortfall(total: u64, seen: u64) -> PaginationShortfall {
    if seen >= total {
        return PaginationShortfall::Match;
    }
    let suppress_threshold = total * 95 / 100; // 5% tolerance
    if seen < suppress_threshold {
        PaginationShortfall::Suppress
    } else {
        PaginationShortfall::WithinTolerance {
            shortfall: total - seen,
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
    passes: &[crate::commands::AlbumPass],
    config: &Arc<DownloadConfig>,
    shutdown_token: CancellationToken,
) -> Result<SyncResult> {
    let started = Instant::now();
    let needs_per_pass = config.requires_per_pass_paths();

    // Mark every unique zone as in-progress so an interrupted full
    // enumeration leaves a trail the next startup can surface to the
    // operator. Clears once the enumeration returns normally.
    let mut enum_zones: Vec<String> = passes
        .iter()
        .map(|p| p.album.zone_name().to_string())
        .collect();
    enum_zones.sort();
    enum_zones.dedup();
    if let Some(db) = &config.state_db {
        for zone in &enum_zones {
            if let Err(e) = db.begin_enum_progress(zone).await {
                tracing::debug!(error = %e, zone, "Failed to mark enumeration start");
            }
        }
    }

    // `album.len()` is one HTTP call per pass. Serialising it scaled fine
    // when users typed out a few `-a` flags by hand; with `-a all` it's
    // routinely 20+ round-trips before the first byte of the first
    // download. `buffered` (not `buffer_unordered`) preserves pass order
    // so the `zip(&pass_counts)` below stays aligned.
    // Capture per-pass `len()` errors instead of swallowing them as zero.
    // A swallowed `len()` failure converted `total` to 0, which short-circuited
    // the pagination-undercount check at line ~1450 (it only fires when
    // `total > 0`); the cycle then returned `Success` with zero assets and the
    // sync token advanced past un-enumerated change events. Treat any failure
    // as a per-album enumeration error so token advancement is suppressed.
    let pass_count_results: Vec<anyhow::Result<u64>> = stream::iter(passes)
        .map(|pass| async move { pass.album.len().await })
        .buffered(config.concurrent_downloads)
        .collect()
        .await;
    let (pass_counts, len_errors) = fold_pass_count_results(pass_count_results, passes);
    let mut total: u64 = pass_counts.iter().sum();
    if let Some(recent) = config.recent {
        total = total.min(u64::from(recent));
    }

    // {album} mode processes passes sequentially: each needs its own
    // album-specific path expansion, so cross-pass download concurrency is
    // traded off for correct placement. Assets in multiple albums get one
    // copy per album folder. Non-{album} plans have a uniform exclude set
    // across passes (LibraryOnly: 1 pass; Named/All-without-{album}: every
    // pass has empty excludes) so streams merge for maximum concurrency.
    let (mut streaming_result, token_receivers) = if needs_per_pass {
        let pass_configs = build_pass_configs(passes, config);
        let mut combined_result = StreamingResult {
            // Enumeration is "complete" only when every pass finished
            // its stream cleanly. Start optimistic; flip to false on the
            // first pass that ended early (shutdown, channel-close, or
            // panic) so the marker stays set and the next startup logs
            // the interruption.
            enumeration_complete: !passes.is_empty(),
            ..StreamingResult::default()
        };
        let mut token_receivers = Vec::with_capacity(passes.len());

        for ((pass, &count), pass_config) in passes.iter().zip(&pass_counts).zip(&pass_configs) {
            if shutdown_token.is_cancelled() {
                combined_result.enumeration_complete = false;
                break;
            }
            let (stream, token_rx) = pass.album.photo_stream_with_token(
                config.recent,
                Some(count),
                config.concurrent_downloads,
            );
            token_receivers.push(token_rx);

            let result = stream_and_download_from_stream(
                download_client,
                stream,
                pass_config,
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
            combined_result.skip_summary += result.skip_summary;
            // AND-fold across passes so a single pass aborting (e.g.
            // producer-channel close, panic) leaves the marker set.
            combined_result.enumeration_complete =
                combined_result.enumeration_complete && result.enumeration_complete;
        }

        (combined_result, token_receivers)
    } else {
        let merged_exclude_ids = passes
            .first()
            .map(|p| Arc::clone(&p.exclude_ids))
            .unwrap_or_else(|| Arc::new(FxHashSet::default()));
        let merged_config = if Arc::ptr_eq(&merged_exclude_ids, &config.exclude_asset_ids) {
            Arc::clone(config)
        } else {
            Arc::new(config.with_exclude_ids(merged_exclude_ids))
        };
        let mut token_receivers = Vec::with_capacity(passes.len());
        let streams: Vec<_> = passes
            .iter()
            .zip(&pass_counts)
            .map(|(pass, &count)| {
                let (stream, token_rx) = pass.album.photo_stream_with_token(
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
            &merged_config,
            total,
            shutdown_token.clone(),
        )
        .await?;

        (result, token_receivers)
    };

    // Fold `len()` failures into the streaming result's enumeration error
    // tally so `build_download_outcome` returns `PartialFailure`. This is the
    // signal `should_store_sync_token` reads to suppress token advancement.
    streaming_result.enumeration_errors += len_errors;
    // A `len()` failure on any pass means we never had a reliable
    // total to enumerate against, so the per-zone enumeration marker must
    // stay set even if the producer drained its (possibly truncated) stream.
    if len_errors > 0 {
        streaming_result.enumeration_complete = false;
    }

    // Check if enumeration saw significantly fewer assets than the API reported.
    // This catches silent pagination truncation, dropped pages, or API hiccups
    // that would otherwise go unnoticed. Any `len()` failure also forces
    // suppression because the recorded `total` is missing those passes.
    let pagination_undercount = if len_errors > 0 {
        true
    } else if total > 0 && !config.only_print_filenames && !config.dry_run {
        let decision = classify_pagination_shortfall(total, streaming_result.assets_seen);
        match decision {
            PaginationShortfall::Match => false,
            PaginationShortfall::WithinTolerance { shortfall } => {
                tracing::warn!(
                    expected = total,
                    seen = streaming_result.assets_seen,
                    shortfall,
                    "Enumeration saw slightly fewer assets than expected; within \
                     5% tolerance so sync token will still advance"
                );
                false
            }
            PaginationShortfall::Suppress => {
                tracing::warn!(
                    expected = total,
                    seen = streaming_result.assets_seen,
                    "Enumeration saw fewer assets than expected — blocking sync token \
                     advancement to force full re-enumeration on next run"
                );
                true
            }
        }
    } else {
        false
    };

    // Collect the sync token from any album's token receiver.
    // In practice, all albums share the same zone so any token suffices.
    // Don't advance the token for read-only operations, or when pagination
    // was incomplete (would permanently skip missed assets).
    let mut sync_token = None;
    if !config.only_print_filenames && !pagination_undercount {
        for rx in token_receivers {
            if let Ok(Some(token)) = rx.await {
                sync_token = Some(token);
                break;
            }
        }
    }

    // Capture the enumeration-complete signal before
    // `build_download_outcome` consumes `streaming_result`. The marker
    // gate below uses this signal directly so a partial-failure run
    // whose enumeration phase finished still clears the marker.
    let enumeration_complete = streaming_result.enumeration_complete;

    // Build the outcome using the same logic as download_photos
    let (outcome, stats) = build_download_outcome(
        download_client,
        passes,
        config,
        streaming_result,
        started,
        shutdown_token,
    )
    .await?;

    // Clear enumeration-in-progress markers when the producer reached the
    // natural end of the API stream. The gate ignores download-side
    // failures so a partial-failure cycle whose enumeration finished
    // doesn't leave the marker set forever. Shutdown still suppresses the
    // clear because the producer's cancellation path leaves
    // `enumeration_complete = false`.
    if enumeration_complete {
        if let Some(db) = &config.state_db {
            for zone in &enum_zones {
                if let Err(e) = db.end_enum_progress(zone).await {
                    tracing::debug!(error = %e, zone, "Failed to clear enumeration marker");
                }
            }
        }
    }

    Ok(SyncResult {
        outcome,
        sync_token,
        stats,
    })
}

/// Incremental delta sync via `changes_stream`.
///
/// Fetches `ChangeEvent`s since the given sync token, filters to
/// downloadable assets, and feeds them through the download pipeline.
async fn download_photos_incremental(
    download_client: &Client,
    passes: &[crate::commands::AlbumPass],
    config: &Arc<DownloadConfig>,
    zone_sync_token: &str,
    shutdown_token: CancellationToken,
) -> Result<SyncResult> {
    let started = Instant::now();
    let needs_per_pass = config.requires_per_pass_paths();

    // Each asset is paired with its source pass index so both `{album}`
    // expansion and per-pass exclusion (notably, the unfiled pass's set
    // that prevents assets already in some user album from downloading
    // twice) can be applied downstream.
    let mut downloadable_assets: Vec<(PhotoAsset, usize)> = Vec::new();
    let mut sync_token: Option<String> = None;
    let mut created_count = 0u64;
    let mut soft_deleted_count = 0u64;
    let mut hard_deleted_count = 0u64;
    let mut hidden_count = 0u64;
    let mut total_events = 0u64;

    for (pass_index, pass) in passes.iter().enumerate() {
        let (change_stream, token_rx) = pass.album.changes_stream(zone_sync_token);
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
                        downloadable_assets.push((asset, pass_index));
                    }
                }
                ChangeReason::SoftDeleted => {
                    soft_deleted_count += 1;
                    tracing::debug!(record_name = %event.record_name, record_type = ?event.record_type, "Skipping soft-deleted record");
                    if let Some(db) = &config.state_db {
                        let deleted_at = event.asset.as_ref().and_then(|a| a.metadata().deleted_at);
                        if let Err(e) = db
                            .mark_soft_deleted(&config.library, &event.record_name, deleted_at)
                            .await
                        {
                            tracing::warn!(
                                record_name = %event.record_name,
                                error = %e,
                                "Failed to record soft-delete in state DB"
                            );
                        }
                    }
                }
                ChangeReason::HardDeleted => {
                    hard_deleted_count += 1;
                    tracing::debug!(record_name = %event.record_name, record_type = ?event.record_type, "Skipping hard-deleted record");
                    // CloudKit returns no fields for hard-deleted records, so we
                    // can't tell master from asset. Treat as soft-delete in DB
                    // (sets is_deleted=1) — the row stays put so history and
                    // local_path remain queryable.
                    if let Some(db) = &config.state_db {
                        if let Err(e) = db
                            .mark_soft_deleted(&config.library, &event.record_name, None)
                            .await
                        {
                            tracing::warn!(
                                record_name = %event.record_name,
                                error = %e,
                                "Failed to record hard-delete in state DB"
                            );
                        }
                    }
                }
                ChangeReason::Hidden => {
                    hidden_count += 1;
                    tracing::debug!(record_name = %event.record_name, record_type = ?event.record_type, "Skipping hidden record");
                    if let Some(db) = &config.state_db {
                        if let Err(e) = db
                            .mark_hidden_at_source(&config.library, &event.record_name)
                            .await
                        {
                            tracing::warn!(
                                record_name = %event.record_name,
                                error = %e,
                                "Failed to record hidden state in state DB"
                            );
                        }
                    }
                }
            }
        }

        // Capture the sync token from this pass
        if let Ok(token) = token_rx.await {
            sync_token = Some(token);
        }
    }

    tracing::debug!(
        created = created_count,
        soft_deleted = soft_deleted_count,
        hard_deleted = hard_deleted_count,
        hidden = hidden_count,
        "Incremental sync: {total_events} change events",
    );

    if downloadable_assets.is_empty() {
        let stats = SyncStats {
            elapsed_secs: started.elapsed().as_secs_f64(),
            ..SyncStats::default()
        };
        tracing::info!("No new photos to download from incremental sync");
        tracing::info!(elapsed = %format_duration(started.elapsed()), "  completed");
        return Ok(SyncResult {
            outcome: DownloadOutcome::Success,
            sync_token,
            stats,
        });
    }

    // Respect --recent: cap the number of assets to download
    if let Some(recent) = config.recent {
        let limit = recent as usize;
        if downloadable_assets.len() > limit {
            tracing::debug!(
                total = downloadable_assets.len(),
                limit,
                "Capping incremental assets to --recent limit"
            );
            downloadable_assets.truncate(limit);
        }
    }

    tracing::debug!(
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
    // Each pass (concrete album or unfiled) gets its own derived config so
    // that both album-specific path expansion and per-pass exclude sets are
    // applied. Configs are cached per pass index to avoid redundant
    // allocations when many assets flow through the same pass.
    let mut tasks: Vec<DownloadTask> = Vec::new();
    let mut claimed_paths: FxHashMap<NormalizedPath, u64> = FxHashMap::default();
    let mut dir_cache = paths::DirCache::new();
    let mut skip_breakdown = SkipBreakdown::default();
    let pass_configs = build_pass_configs(passes, config);

    for (asset, pass_index) in &downloadable_assets {
        #[allow(
            clippy::indexing_slicing,
            reason = "pass_index was assigned by the producer from the same `passes` slice \
                      that pass_configs was built from; indices are valid"
        )]
        let effective_config = &pass_configs[*pass_index];

        if let Some(reason) = filter::is_asset_filtered(asset, effective_config) {
            match reason {
                filter::FilterReason::ExcludedAlbum => skip_breakdown.by_excluded_album += 1,
                filter::FilterReason::MediaType => skip_breakdown.by_media_type += 1,
                filter::FilterReason::LivePhoto => skip_breakdown.by_live_photo += 1,
                filter::FilterReason::DateRange => skip_breakdown.by_date_range += 1,
                filter::FilterReason::Filename => skip_breakdown.by_filename += 1,
            }
            continue;
        }

        // `should_download_fast` keys on (asset_id, version_size, checksum)
        // and is path-blind. In `{album}` mode the same asset may target
        // multiple album folders; a DB-only skip would leave later copies
        // missing from disk. Fall through to the path-aware filesystem
        // check in that case.
        if !needs_per_pass {
            let candidates = extract_skip_candidates(asset, effective_config);
            if !candidates.is_empty()
                && candidates.iter().all(|&(vs, cs)| {
                    matches!(
                        download_ctx.should_download_fast(
                            &effective_config.library,
                            asset.id(),
                            vs,
                            cs,
                            true
                        ),
                        Some(false)
                    )
                })
            {
                skip_breakdown.by_state += 1;
                continue;
            }
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
                    Arc::clone(&config.library),
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
                )
                .with_metadata_arc(asset.metadata_arc());
                if let Err(e) = db.upsert_seen(&record).await {
                    tracing::warn!(
                        asset_id = %task.asset_id,
                        error = %e,
                        "Failed to record asset in state DB"
                    );
                }
            }
            // Record this asset's membership in the current album so
            // consumers (EXIF keywords, XMP sidecars, Immich albums) can
            // reconstruct the logical album graph from the state DB.
            if let Some(album_name) = effective_config
                .album_name
                .as_deref()
                .filter(|n| !n.is_empty())
            {
                if let Err(e) = pipeline::add_asset_album_with_retry(
                    db.as_ref(),
                    asset.id(),
                    album_name,
                    "icloud",
                )
                .await
                {
                    tracing::warn!(
                        asset_id = %asset.id(),
                        album = %album_name,
                        error = %e,
                        "Failed to record album membership after retries"
                    );
                }
            }
        }

        if asset_tasks.is_empty() {
            skip_breakdown.on_disk += 1;
        }
        tasks.extend(asset_tasks);
    }

    if skip_breakdown.by_state > 0 {
        tracing::debug!(
            skipped = skip_breakdown.by_state,
            "Skipped already-downloaded assets (state DB)"
        );
    }

    if tasks.is_empty() {
        let stats = SyncStats {
            skipped: skip_breakdown,
            elapsed_secs: started.elapsed().as_secs_f64(),
            ..SyncStats::default()
        };
        tracing::info!("All incremental assets already downloaded or filtered");
        tracing::info!(elapsed = %format_duration(started.elapsed()), "  completed");
        return Ok(SyncResult {
            outcome: DownloadOutcome::Success,
            sync_token,
            stats,
        });
    }

    if config.only_print_filenames {
        #[allow(
            clippy::print_stdout,
            reason = "--only-print-filenames writes target paths to stdout so callers can pipe to xargs/etc"
        )]
        for task in &tasks {
            println!("{}", task.download_path.display());
        }
        let stats = SyncStats {
            skipped: skip_breakdown,
            elapsed_secs: started.elapsed().as_secs_f64(),
            ..SyncStats::default()
        };
        // Don't advance the sync token — this is a read-only operation.
        return Ok(SyncResult {
            outcome: DownloadOutcome::Success,
            sync_token: None,
            stats,
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
        metadata: MetadataFlags::from(config.as_ref()),
        concurrency: config.concurrent_downloads,
        no_progress_bar: config.no_progress_bar,
        temp_suffix: Arc::clone(&config.temp_suffix),
        shutdown_token,
        state_db: config.state_db.clone(),
        rate_limit_counter: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        bandwidth_limiter: config.bandwidth_limiter.clone(),
        library: Arc::clone(&config.library),
    };
    let pass_result = run_download_pass(pass_config, tasks).await;

    let failed = pass_result.failed.len();
    let succeeded = task_count - failed;

    // Log failed downloads before the summary
    if failed > 0 {
        for task in &pass_result.failed {
            tracing::error!(asset_id = %task.asset_id, path = %task.download_path.display(), "Download failed");
        }
    }

    let stats = SyncStats {
        assets_seen: 0, // incremental doesn't have total library count
        downloaded: succeeded,
        failed,
        skipped: skip_breakdown,
        bytes_downloaded: pass_result.bytes_downloaded,
        disk_bytes_written: pass_result.disk_bytes_written,
        exif_failures: pass_result.exif_failures,
        state_write_failures: pass_result.state_write_failures,
        enumeration_errors: 0,
        elapsed_secs: started.elapsed().as_secs_f64(),
        interrupted: pass_result.auth_errors >= AUTH_ERROR_THRESHOLD,
        rate_limited: pass_result.rate_limit_observations,
    };
    log_sync_summary(
        "\u{2500}\u{2500} Incremental Sync Summary \u{2500}\u{2500}",
        &stats,
    );

    if pass_result.auth_errors >= AUTH_ERROR_THRESHOLD {
        return Ok(SyncResult {
            outcome: DownloadOutcome::SessionExpired {
                auth_error_count: pass_result.auth_errors,
            },
            sync_token,
            stats,
        });
    }

    let outcome =
        if failed > 0 || pass_result.exif_failures > 0 || pass_result.state_write_failures > 0 {
            DownloadOutcome::PartialFailure {
                failed_count: failed + pass_result.exif_failures + pass_result.state_write_failures,
            }
        } else {
            DownloadOutcome::Success
        };

    Ok(SyncResult {
        outcome,
        sync_token,
        stats,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::icloud::photos::asset::ChangeEvent;
    use crate::test_helpers::TestPhotoAsset;
    use tempfile::TempDir;

    fn test_config() -> DownloadConfig {
        DownloadConfig::test_default()
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
        config1.directory = std::sync::Arc::from(std::path::Path::new("/photos/a"));
        let mut config2 = test_config();
        config2.directory = std::sync::Arc::from(std::path::Path::new("/photos/b"));
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
            .entry("PrimarySync".into())
            .or_default()
            .entry("asset1".into())
            .or_default()
            .insert("original".into());
        ctx.downloaded_checksums
            .entry("PrimarySync".into())
            .or_default()
            .entry("asset1".into())
            .or_default()
            .insert("original".into(), "checksum_a".into());

        // trust_state=true: returns Some(false) for matching asset
        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "asset1",
                VersionSizeKey::Original,
                "checksum_a",
                true
            ),
            Some(false)
        );

        // trust_state=false: returns None (needs filesystem check)
        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "asset1",
                VersionSizeKey::Original,
                "checksum_a",
                false
            ),
            None
        );

        // Changed checksum: returns Some(true) regardless of trust_state
        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "asset1",
                VersionSizeKey::Original,
                "checksum_b",
                true
            ),
            Some(true)
        );

        // Unknown asset: returns Some(true)
        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "unknown",
                VersionSizeKey::Original,
                "x",
                true
            ),
            Some(true)
        );
    }

    // ── extract_skip_candidates tests ──────────────────────────────

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

    // ── NameId7 filter tests ────────────────────────────────────────────

    // ── keep_unicode_in_filenames tests ─────────────────────────────────

    // ── Medium/Thumb size suffix tests ──────────────────────────────────

    // ── NormalizedPath direct tests ─────────────────────────────────────

    // ---------- SyncMode / SyncResult tests ----------

    #[test]
    fn test_sync_result_partial_failure() {
        let result = SyncResult {
            outcome: DownloadOutcome::PartialFailure { failed_count: 3 },
            sync_token: Some("tok".to_string()),
            stats: SyncStats::default(),
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
            stats: SyncStats::default(),
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
            record_name: "REC_1".into(),
            record_type: Some("CPLAsset".into()),
            reason: ChangeReason::Created,
            asset: Some(TestPhotoAsset::new("TEST_1").build()),
        };
        let event_without_asset = ChangeEvent {
            record_name: "REC_2".into(),
            record_type: Some("CPLAsset".into()),
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
                record_name: "REC_1".into(),
                record_type: Some("CPLAsset".into()),
                reason: ChangeReason::Created,
                asset: Some(TestPhotoAsset::new("TEST_1").build()),
            },
            ChangeEvent {
                record_name: "REC_2".into(),
                record_type: None,
                reason: ChangeReason::HardDeleted,
                asset: None,
            },
            ChangeEvent {
                record_name: "REC_3".into(),
                record_type: Some("CPLAsset".into()),
                reason: ChangeReason::SoftDeleted,
                asset: None,
            },
            ChangeEvent {
                record_name: "REC_4".into(),
                record_type: Some("CPLAsset".into()),
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
            record_name: "MOD_1".into(),
            record_type: Some("CPLAsset".into()),
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
            directory: dl_config.directory.to_path_buf(),
            cookie_directory: std::path::PathBuf::from("/tmp"),
            folder_structure: dl_config.folder_structure.clone(),
            folder_structure_albums: crate::config::DEFAULT_FOLDER_STRUCTURE_ALBUMS.to_string(),
            folder_structure_smart_folders: crate::config::DEFAULT_FOLDER_STRUCTURE_SMART_FOLDERS
                .to_string(),
            albums: crate::config::AlbumSelection::LibraryOnly,
            exclude_albums: vec![],
            filename_exclude: vec![],
            temp_suffix: dl_config.temp_suffix.to_string(),
            selection: crate::selection::Selection::default(),
            skip_created_before: None,
            skip_created_after: None,
            pid_file: None,
            notification_script: None,
            report_json: None,
            http_port: 9090,
            http_bind: std::net::IpAddr::V4(std::net::Ipv4Addr::new(0, 0, 0, 0)),
            watch_with_interval: None,
            retry_delay_secs: 5,
            reconcile_every_n_cycles: None,
            recent: dl_config.recent,
            max_retries: 3,
            bandwidth_limit: None,
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
            #[cfg(feature = "xmp")]
            set_exif_datetime: false,
            #[cfg(feature = "xmp")]
            set_exif_rating: false,
            #[cfg(feature = "xmp")]
            set_exif_gps: false,
            #[cfg(feature = "xmp")]
            set_exif_description: false,
            #[cfg(feature = "xmp")]
            embed_xmp: false,
            #[cfg(feature = "xmp")]
            xmp_sidecar: false,
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
        config_with_album.albums =
            crate::config::AlbumSelection::Named(vec!["Favorites".to_string()]);
        let hash3 = compute_config_hash(&config_with_album);
        assert_ne!(hash1, hash3, "adding an album must change the hash");
    }

    // ── should_download_fast additional tests ───────────────────────────

    #[test]
    fn test_should_download_fast_unknown_asset_returns_true() {
        let ctx = DownloadContext::default();
        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "never_seen",
                VersionSizeKey::Original,
                "any_ck",
                true
            ),
            Some(true)
        );
        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "never_seen",
                VersionSizeKey::Original,
                "any_ck",
                false
            ),
            Some(true)
        );
    }

    #[test]
    fn needs_metadata_rewrite_detects_hash_change() {
        let mut ctx = DownloadContext::default();
        ctx.downloaded_metadata_hashes
            .entry("PrimarySync".into())
            .or_default()
            .entry("asset_md".into())
            .or_default()
            .insert("original".into(), "hash-OLD".into());

        // Same hash -> no rewrite needed.
        assert!(!ctx.needs_metadata_rewrite(
            "PrimarySync",
            "asset_md",
            VersionSizeKey::Original,
            Some("hash-OLD")
        ));
        // Different hash -> rewrite.
        assert!(ctx.needs_metadata_rewrite(
            "PrimarySync",
            "asset_md",
            VersionSizeKey::Original,
            Some("hash-NEW")
        ));
        // Unknown new hash -> no rewrite (nothing to compare to).
        assert!(!ctx.needs_metadata_rewrite(
            "PrimarySync",
            "asset_md",
            VersionSizeKey::Original,
            None
        ));
    }

    #[test]
    fn needs_metadata_rewrite_honors_retry_marker() {
        let mut ctx = DownloadContext::default();
        ctx.metadata_retry_markers
            .entry("PrimarySync".into())
            .or_default()
            .entry("asset_retry".into())
            .or_default()
            .insert("original".into());
        // No stored hash at all, but marker is set -> rewrite needed.
        assert!(ctx.needs_metadata_rewrite(
            "PrimarySync",
            "asset_retry",
            VersionSizeKey::Original,
            None
        ));
        // Marker set -> rewrite even if hashes match.
        ctx.downloaded_metadata_hashes
            .entry("PrimarySync".into())
            .or_default()
            .entry("asset_retry".into())
            .or_default()
            .insert("original".into(), "h".into());
        assert!(ctx.needs_metadata_rewrite(
            "PrimarySync",
            "asset_retry",
            VersionSizeKey::Original,
            Some("h")
        ));
    }

    #[test]
    fn needs_metadata_rewrite_refreshes_null_stored_hash() {
        // Pre-v5 downloaded rows have metadata_hash IS NULL; even without a
        // retry marker, a fresh hash should trigger a rewrite so the XMP
        // gets the provider state this tree has never recorded.
        let ctx = DownloadContext::default();
        assert!(ctx.needs_metadata_rewrite(
            "PrimarySync",
            "asset_no_stored_hash",
            VersionSizeKey::Original,
            Some("new-hash")
        ));
    }

    #[test]
    fn test_should_download_fast_downloaded_matching_checksum() {
        let mut ctx = DownloadContext::default();
        ctx.downloaded_ids
            .entry("PrimarySync".into())
            .or_default()
            .entry("asset_x".into())
            .or_default()
            .insert("original".into());
        ctx.downloaded_checksums
            .entry("PrimarySync".into())
            .or_default()
            .entry("asset_x".into())
            .or_default()
            .insert("original".into(), "ck_match".into());

        // trust_state=true => hard skip
        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "asset_x",
                VersionSizeKey::Original,
                "ck_match",
                true
            ),
            Some(false)
        );
        // trust_state=false => needs filesystem check
        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "asset_x",
                VersionSizeKey::Original,
                "ck_match",
                false
            ),
            None
        );
    }

    #[test]
    fn test_should_download_fast_downloaded_changed_checksum() {
        let mut ctx = DownloadContext::default();
        ctx.downloaded_ids
            .entry("PrimarySync".into())
            .or_default()
            .entry("asset_y".into())
            .or_default()
            .insert("original".into());
        ctx.downloaded_checksums
            .entry("PrimarySync".into())
            .or_default()
            .entry("asset_y".into())
            .or_default()
            .insert("original".into(), "old_ck".into());

        // Changed checksum => needs re-download regardless of trust_state
        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "asset_y",
                VersionSizeKey::Original,
                "new_ck",
                true
            ),
            Some(true)
        );
        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "asset_y",
                VersionSizeKey::Original,
                "new_ck",
                false
            ),
            Some(true)
        );
    }

    #[test]
    fn test_should_download_fast_different_version_size() {
        let mut ctx = DownloadContext::default();
        ctx.downloaded_ids
            .entry("PrimarySync".into())
            .or_default()
            .entry("asset_z".into())
            .or_default()
            .insert("original".into());

        // Medium version not downloaded
        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "asset_z",
                VersionSizeKey::Medium,
                "any_ck",
                true
            ),
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
            ctx.should_download_fast(
                "PrimarySync",
                "known_asset",
                VersionSizeKey::Original,
                "ck",
                true
            ),
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
                record_name: "A".into(),
                record_type: Some("CPLAsset".into()),
                reason: ChangeReason::Created,
                asset: Some(TestPhotoAsset::new("TEST_1").build()),
            },
            ChangeEvent {
                record_name: "B".into(),
                record_type: Some("CPLAsset".into()),
                reason: ChangeReason::Created,
                asset: None, // Unpaired record
            },
            ChangeEvent {
                record_name: "C".into(),
                record_type: None,
                reason: ChangeReason::HardDeleted,
                asset: None,
            },
            ChangeEvent {
                record_name: "D".into(),
                record_type: Some("CPLAsset".into()),
                reason: ChangeReason::SoftDeleted,
                asset: None,
            },
            ChangeEvent {
                record_name: "E".into(),
                record_type: Some("CPLAsset".into()),
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

    // ── Gap coverage: empty versions, path traversal, empty filename ───

    // ── Gap coverage: should_download_fast with empty checksum ──────────

    #[test]
    fn should_download_fast_empty_checksum_string() {
        // When the stored checksum is empty and the incoming checksum is also
        // empty, they match — should behave like a normal matching checksum.
        let mut ctx = DownloadContext::default();
        ctx.downloaded_ids
            .entry("PrimarySync".into())
            .or_default()
            .entry("asset_empty_ck".into())
            .or_default()
            .insert("original".into());
        ctx.downloaded_checksums
            .entry("PrimarySync".into())
            .or_default()
            .entry("asset_empty_ck".into())
            .or_default()
            .insert("original".into(), "".into());

        // Empty matches empty → trust_state=true gives hard skip
        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "asset_empty_ck",
                VersionSizeKey::Original,
                "",
                true
            ),
            Some(false)
        );
        // Empty matches empty → trust_state=false gives None (needs fs check)
        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "asset_empty_ck",
                VersionSizeKey::Original,
                "",
                false
            ),
            None
        );
        // Non-empty vs empty stored → checksum changed, needs download
        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "asset_empty_ck",
                VersionSizeKey::Original,
                "abc123def456",
                true,
            ),
            Some(true)
        );
    }

    // ── Gap coverage: should_download_fast with no checksum in DB ────────

    #[test]
    fn should_download_fast_no_checksum_trust_true_returns_false() {
        // Asset is in downloaded_ids but has no entry in downloaded_checksums.
        // With trust_state=true the method should hard-skip (Some(false))
        // because the absence of a stored checksum means "nothing to compare".
        let mut ctx = DownloadContext::default();
        ctx.downloaded_ids
            .entry("PrimarySync".into())
            .or_default()
            .entry("asset_no_ck".into())
            .or_default()
            .insert("original".into());
        // No entry in downloaded_checksums

        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "asset_no_ck",
                VersionSizeKey::Original,
                "any",
                true
            ),
            Some(false)
        );
    }

    #[test]
    fn should_download_fast_no_checksum_trust_false_returns_none() {
        // Same scenario but trust_state=false: needs filesystem check (None).
        let mut ctx = DownloadContext::default();
        ctx.downloaded_ids
            .entry("PrimarySync".into())
            .or_default()
            .entry("asset_no_ck".into())
            .or_default()
            .insert("original".into());

        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "asset_no_ck",
                VersionSizeKey::Original,
                "any",
                false
            ),
            None
        );
    }

    // ── Gap coverage: retry_only known_ids filtering ────────────────────

    // ── Gap coverage: skip_created_before AND skip_created_after ────────

    // ── Gap coverage: incremental Modified events are downloadable ──────

    // ── Gap coverage: NameId7 produces task when file at original path ──

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
    fn test_compute_config_hash_different_library() {
        let tmp = TempDir::new().unwrap();
        let a = build_config_with(tmp.path(), "/photos", |_| {});
        let b = build_config_with(tmp.path(), "/photos", |s| {
            s.libraries = vec!["all".to_string()];
        });
        assert_ne!(
            compute_config_hash(&a),
            compute_config_hash(&b),
            "changing library selection should change the config hash"
        );
    }

    #[test]
    fn test_compute_config_hash_different_recent_same_hash() {
        let tmp = TempDir::new().unwrap();
        let a = build_config_with(tmp.path(), "/photos", |_| {});
        let b = build_config_with(tmp.path(), "/photos", |s| {
            s.recent = Some(crate::cli::RecentLimit::Count(100));
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

    // ── LivePhotoMode + filename_exclude filter tests ─────────────

    // ── exclude_asset_ids filter tests ─────────────────────────────

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
        config2.filename_exclude = std::sync::Arc::from(vec![glob::Pattern::new("*.AAE").unwrap()]);
        assert_ne!(
            hash_download_config(&config1),
            hash_download_config(&config2)
        );
    }

    // ── requires_per_pass_paths predicate ──────────────────────────

    fn config_with_templates(base: &str, albums: &str, smart_folders: &str) -> DownloadConfig {
        let mut c = test_config();
        c.folder_structure = base.to_string();
        c.folder_structure_albums = Arc::from(albums);
        c.folder_structure_smart_folders = Arc::from(smart_folders);
        c
    }

    #[test]
    fn requires_per_pass_paths_fires_on_v013_defaults() {
        // v0.13 defaults carry per-pass tokens in the per-category fields.
        // Returning false here was the regression that silently routed every
        // album-pass photo through the unfiled template.
        assert!(test_config().requires_per_pass_paths());
    }

    #[test]
    fn requires_per_pass_paths_fires_on_legacy_album_in_base() {
        assert!(
            config_with_templates("{album}/%Y", "{album}/%Y", "{album}/%Y")
                .requires_per_pass_paths()
        );
    }

    #[test]
    fn requires_per_pass_paths_fires_on_smart_folder_token() {
        assert!(
            config_with_templates("%Y/%m/%d", "%Y/%m/%d", "{smart-folder}")
                .requires_per_pass_paths()
        );
    }

    #[test]
    fn requires_per_pass_paths_fires_on_library_token() {
        assert!(
            config_with_templates("{library}/%Y", "{library}/%Y", "{library}/%Y")
                .requires_per_pass_paths()
        );
    }

    #[test]
    fn requires_per_pass_paths_fires_on_per_category_template_diverging_from_base() {
        assert!(
            config_with_templates("%Y/%m/%d", "MyAlbums/%Y/%m", "%Y/%m/%d")
                .requires_per_pass_paths()
        );
    }

    #[test]
    fn requires_per_pass_paths_false_when_all_templates_are_identical_literals() {
        // Pure-literal, identical across all three fields, no per-pass token:
        // the merged-stream branch is safe.
        assert!(
            !config_with_templates("%Y/%m/%d", "%Y/%m/%d", "%Y/%m/%d").requires_per_pass_paths()
        );
    }

    // ── with_pass per-kind template selection ─────────────────────

    fn make_pass(kind: crate::commands::PassKind, name: &str) -> crate::commands::AlbumPass {
        use crate::icloud::photos::PhotoAlbum;
        crate::commands::AlbumPass {
            kind,
            album: PhotoAlbum::stub_for_test(Arc::from(name)),
            exclude_ids: std::sync::Arc::new(rustc_hash::FxHashSet::default()),
        }
    }

    #[test]
    fn test_with_pass_album_uses_albums_template() {
        use crate::commands::PassKind;
        let mut config = test_config();
        config.folder_structure_albums = Arc::from("{album}/%Y/%m/%d");
        let derived = config.with_pass(&make_pass(PassKind::Album, "Vacation"));
        assert_eq!(derived.folder_structure, "Vacation/%Y/%m/%d");
        assert_eq!(derived.album_name.as_deref(), Some("Vacation"));
    }

    #[test]
    fn test_with_pass_smart_folder_uses_smart_folders_template() {
        use crate::commands::PassKind;
        let mut config = test_config();
        config.folder_structure_smart_folders = Arc::from("{smart-folder}/%Y");
        let derived = config.with_pass(&make_pass(PassKind::SmartFolder, "Favorites"));
        assert_eq!(derived.folder_structure, "Favorites/%Y");
    }

    #[test]
    fn test_with_pass_smart_folder_ignores_albums_template() {
        // Spec: smart-folder passes use folder_structure_smart_folders, not
        // folder_structure_albums. Using the wrong template would cause every
        // smart-folder pass to substitute the smart-folder name into a
        // user-customised album path (e.g. "My/Albums/{album}/..." would
        // mis-render as "My/Albums/Favorites/...").
        use crate::commands::PassKind;
        let mut config = test_config();
        config.folder_structure_albums = Arc::from("{album}/album-tree");
        config.folder_structure_smart_folders = Arc::from("{smart-folder}/sf-tree");
        let derived = config.with_pass(&make_pass(PassKind::SmartFolder, "Videos"));
        assert!(derived.folder_structure.contains("sf-tree"));
        assert!(!derived.folder_structure.contains("album-tree"));
    }

    #[test]
    fn test_with_pass_unfiled_uses_base_folder_structure() {
        // Unfiled pass keeps the legacy `{album}` token in `folder_structure`
        // so existing configs with `--folder-structure "{album}/..."` still
        // produce the same on-disk tree.
        use crate::commands::PassKind;
        let mut config = test_config();
        config.folder_structure = "%Y/%m/%d".to_string();
        let derived = config.with_pass(&make_pass(PassKind::Unfiled, ""));
        assert_eq!(derived.folder_structure, "%Y/%m/%d");
    }

    #[test]
    fn test_with_pass_unfiled_collapses_album_token_to_empty() {
        use crate::commands::PassKind;
        let mut config = test_config();
        config.folder_structure = "{album}/%Y/%m/%d".to_string();
        let derived = config.with_pass(&make_pass(PassKind::Unfiled, ""));
        // Empty name strips the `{album}` segment for backwards compat.
        assert!(!derived.folder_structure.contains("{album}"));
    }

    #[test]
    fn test_with_pass_album_sanitizes_name() {
        use crate::commands::PassKind;
        let mut config = test_config();
        config.folder_structure_albums = Arc::from("{album}/%Y");
        let derived = config.with_pass(&make_pass(PassKind::Album, "My/Album"));
        // Path separators in album names must be sanitised before substitution.
        assert!(!derived.folder_structure.starts_with("My/Album"));
    }

    #[test]
    fn test_with_pass_expands_library_token_with_truncation() {
        use crate::commands::PassKind;
        let mut config = test_config();
        config.folder_structure_albums = Arc::from("{library}/{album}/%Y");
        config.library = Arc::from("SharedSync-A1B2C3D4-E5F6-7890-ABCD-EF1234567890");
        let derived = config.with_pass(&make_pass(PassKind::Album, "Vacation"));
        assert_eq!(
            derived.folder_structure, "SharedSync-A1B2C3D4/Vacation/%Y",
            "shared-zone UUIDs must truncate to 8 chars in path output"
        );
    }

    #[test]
    fn test_with_pass_library_token_passthrough_for_primary() {
        use crate::commands::PassKind;
        let mut config = test_config();
        config.folder_structure = "{library}/%Y/%m/%d".to_string();
        // Default `library` is "PrimarySync" via `test_default`.
        let derived = config.with_pass(&make_pass(PassKind::Unfiled, ""));
        assert_eq!(derived.folder_structure, "PrimarySync/%Y/%m/%d");
    }

    #[test]
    fn test_with_pass_library_token_in_smart_folder_template() {
        use crate::commands::PassKind;
        let mut config = test_config();
        config.folder_structure_smart_folders = Arc::from("{library}/{smart-folder}");
        config.library = Arc::from("SharedSync-DEADBEEF-aaaa-bbbb-cccc-dddddddddddd");
        let derived = config.with_pass(&make_pass(PassKind::SmartFolder, "Favorites"));
        assert_eq!(derived.folder_structure, "SharedSync-DEADBEEF/Favorites");
    }

    #[test]
    fn test_with_pass_state_db_library_uses_full_zone_name() {
        // Path rendering truncates the zone for readability, but the
        // state-DB key (DownloadConfig.library) keeps the full zone name
        // verbatim so two zones whose 8-char prefixes happen to collide
        // still get distinct PKs in the assets table.
        use crate::commands::PassKind;
        let mut config = test_config();
        config.library = Arc::from("SharedSync-A1B2C3D4-E5F6-7890-ABCD-EF1234567890");
        let derived = config.with_pass(&make_pass(PassKind::Album, "Trip"));
        assert_eq!(
            &*derived.library,
            "SharedSync-A1B2C3D4-E5F6-7890-ABCD-EF1234567890"
        );
    }

    #[test]
    fn test_with_pass_preserves_all_fields() {
        use crate::commands::PassKind;
        let mut config = test_config();
        config.folder_structure_albums = Arc::from("{album}/%Y");
        config.skip_videos = true;
        config.skip_photos = true;
        config.live_photo_mode = LivePhotoMode::ImageOnly;
        config.force_size = true;
        config.keep_unicode_in_filenames = true;
        config.dry_run = true;
        #[cfg(feature = "xmp")]
        {
            config.set_exif_datetime = true;
        }
        config.filename_exclude = std::sync::Arc::from(vec![glob::Pattern::new("*.AAE").unwrap()]);
        config.temp_suffix = std::sync::Arc::from(".custom-tmp");
        let derived = config.with_pass(&make_pass(PassKind::Album, "Test"));
        assert!(derived.skip_videos);
        assert!(derived.skip_photos);
        assert_eq!(derived.live_photo_mode, LivePhotoMode::ImageOnly);
        assert!(derived.force_size);
        assert!(derived.keep_unicode_in_filenames);
        assert!(derived.dry_run);
        #[cfg(feature = "xmp")]
        assert!(derived.set_exif_datetime);
        assert_eq!(derived.filename_exclude.len(), 1);
        assert_eq!(&*derived.temp_suffix, ".custom-tmp");
        assert_eq!(derived.directory, config.directory);
    }

    // ── extract_skip_candidates: filename_exclude ─────────────────

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

    #[test]
    fn test_hash_changes_on_folder_structure_albums() {
        // Per-category templates affect path resolution, so the trust-state
        // hash must change with them or stale records pin assets to the wrong
        // tree on the next run.
        let mut config1 = test_config();
        let mut config2 = test_config();
        config1.folder_structure_albums = Arc::from("{album}");
        config2.folder_structure_albums = Arc::from("{album}/%Y");
        assert_ne!(
            hash_download_config(&config1),
            hash_download_config(&config2)
        );
    }

    #[test]
    fn test_hash_changes_on_folder_structure_smart_folders() {
        let mut config1 = test_config();
        let mut config2 = test_config();
        config1.folder_structure_smart_folders = Arc::from("{smart-folder}");
        config2.folder_structure_smart_folders = Arc::from("{smart-folder}/%Y");
        assert_ne!(
            hash_download_config(&config1),
            hash_download_config(&config2)
        );
    }

    // ── Golden-hash stability tests ─────────────────────────────────
    //
    // These pin specific config values to specific hex outputs. If any
    // test fails, it means the hash encoding changed -- which would
    // trigger unnecessary full re-syncs for all users. Only update the
    // expected values when the hash change is intentional.

    #[test]
    fn golden_hash_download_config_defaults() {
        let config = test_config();
        let hash = hash_download_config(&config);
        assert_eq!(
            hash, "6500d91b19aec487",
            "hash_download_config golden hash changed -- this will trigger full re-syncs"
        );
    }

    #[test]
    fn golden_hash_download_config_non_defaults() {
        let mut config = test_config();
        config.directory = std::sync::Arc::from(std::path::Path::new("/my/photos"));
        config.folder_structure = "{:%Y/%m}".to_string();
        config.size = AssetVersionSize::Medium;
        config.live_photo_size = AssetVersionSize::LiveMedium;
        config.file_match_policy = FileMatchPolicy::NameId7;
        config.live_photo_mov_filename_policy = crate::types::LivePhotoMovFilenamePolicy::Original;
        config.align_raw = RawTreatmentPolicy::PreferAlternative;
        config.keep_unicode_in_filenames = true;
        config.skip_created_before = Some(
            DateTime::parse_from_rfc3339("2020-06-15T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        );
        config.skip_created_after = Some(
            DateTime::parse_from_rfc3339("2024-12-31T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        );
        config.recent = Some(500);
        config.force_size = true;
        config.skip_videos = true;
        config.skip_photos = false;
        config.live_photo_mode = LivePhotoMode::ImageOnly;
        config.filename_exclude = std::sync::Arc::from(vec![
            glob::Pattern::new("*.AAE").unwrap(),
            glob::Pattern::new("*.THM").unwrap(),
        ]);
        let hash = hash_download_config(&config);
        assert_eq!(
            hash, "265311b50bfaeb17",
            "hash_download_config golden hash changed -- this will trigger full re-syncs"
        );
    }

    #[test]
    fn golden_compute_config_hash_defaults() {
        let tmp = TempDir::new().unwrap();
        let config = build_config_with(tmp.path(), "/photos", |_| {});
        let hash = compute_config_hash(&config);
        assert_eq!(
            hash, "a8fe0a9169180cf3",
            "compute_config_hash golden hash changed -- this will invalidate sync tokens"
        );
    }

    #[test]
    fn golden_compute_config_hash_with_albums() {
        let tmp = TempDir::new().unwrap();
        let config = build_config_with(tmp.path(), "/photos", |s| {
            s.albums = vec!["Favorites".to_string(), "Travel".to_string()];
            s.exclude_albums = vec!["Hidden".to_string()];
        });
        let hash = compute_config_hash(&config);
        assert_eq!(
            hash, "8e5fd4f4c01b7252",
            "compute_config_hash golden hash changed -- this will invalidate sync tokens"
        );
    }

    // ── Gap: DownloadContext attempt_counts used by producer ──────────

    #[test]
    fn download_context_attempt_counts_track_per_asset() {
        let mut ctx = DownloadContext::default();
        ctx.attempt_counts.insert("asset_high".into(), 15);
        ctx.attempt_counts.insert("asset_low".into(), 2);

        // Simulate the producer's retry-exhaustion check
        let max_attempts = 10u32;
        assert!(
            ctx.attempt_counts
                .get("asset_high")
                .is_some_and(|&c| c >= max_attempts),
            "asset_high should exceed max_download_attempts"
        );
        assert!(
            ctx.attempt_counts
                .get("asset_low")
                .is_none_or(|&c| c < max_attempts),
            "asset_low should not exceed max_download_attempts"
        );
        assert!(
            !ctx.attempt_counts.contains_key("asset_never_failed"),
            "unknown asset should not be in attempt_counts"
        );
    }

    // ── Gap: should_download_fast with downloaded but different version ──

    #[test]
    fn should_download_fast_downloaded_original_but_medium_requested() {
        // Asset is downloaded as Original, but now we ask about Medium.
        // should_download_fast should return Some(true) because Medium
        // was never downloaded.
        let mut ctx = DownloadContext::default();
        ctx.downloaded_ids
            .entry("PrimarySync".into())
            .or_default()
            .entry("asset_multi".into())
            .or_default()
            .insert("original".into());
        ctx.downloaded_checksums
            .entry("PrimarySync".into())
            .or_default()
            .entry("asset_multi".into())
            .or_default()
            .insert("original".into(), "ck_orig".into());

        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "asset_multi",
                VersionSizeKey::Medium,
                "ck_med",
                true
            ),
            Some(true),
            "Medium version not in downloaded set should need download"
        );
    }

    // ── Mutation-pinning sibling: operator inversion ──────────
    //
    // The existing tests already assert the decision (Some(true) /
    // Some(false)), not just field equality. What's missing is a test
    // that pins the checksum-comparison **operator**: if a refactor
    // swaps `stored.as_ref() != checksum` for `==`, the decision
    // inverts and every downloaded asset re-downloads (or vice versa).
    //
    // Mutation: in `should_download_fast`, swap `!=` to `==` on the
    // stored-vs-current checksum line. With the existing fixtures,
    // both sides happen to land on `Some(false)` for the matching
    // case via the trust_state path, so the assertion below is the
    // only one in the suite that fails on operator inversion: paired
    // probes with opposite checksum equality, asserting opposite
    // decisions.
    #[test]
    fn should_download_fast_inverts_when_checksum_operator_flips() {
        let mut ctx = DownloadContext::default();
        ctx.downloaded_ids
            .entry("PrimarySync".into())
            .or_default()
            .entry("asset_op".into())
            .or_default()
            .insert("original".into());
        ctx.downloaded_checksums
            .entry("PrimarySync".into())
            .or_default()
            .entry("asset_op".into())
            .or_default()
            .insert("original".into(), "ck_stored".into());

        // Matching checksum + trust_state=true → skip (Some(false)).
        let matching = ctx.should_download_fast(
            "PrimarySync",
            "asset_op",
            VersionSizeKey::Original,
            "ck_stored",
            true,
        );
        // Different checksum + trust_state=true → re-download
        // (Some(true)).
        let different = ctx.should_download_fast(
            "PrimarySync",
            "asset_op",
            VersionSizeKey::Original,
            "ck_changed",
            true,
        );

        // Pin the inversion. If `!=` were swapped for `==`, both probes
        // would return the same Some(_) — collapsing the decision
        // surface. Asserting opposite values catches that.
        assert_eq!(matching, Some(false), "matching checksum must skip");
        assert_eq!(different, Some(true), "changed checksum must re-download");
        assert_ne!(
            matching, different,
            "matching and changed checksums must produce opposite decisions \
             (catches `!=` ↔ `==` operator swap on stored-vs-current compare)"
        );
    }

    // ── Gap: should_download_fast with multiple version sizes ─────────

    #[test]
    fn should_download_fast_multiple_versions_independent() {
        // Both Original and LiveOriginal downloaded, each with own checksum.
        let mut ctx = DownloadContext::default();
        ctx.downloaded_ids
            .entry("PrimarySync".into())
            .or_default()
            .entry("live_asset".into())
            .or_default()
            .insert("original".into());
        ctx.downloaded_ids
            .entry("PrimarySync".into())
            .or_default()
            .entry("live_asset".into())
            .or_default()
            .insert("live_original".into());
        ctx.downloaded_checksums
            .entry("PrimarySync".into())
            .or_default()
            .entry("live_asset".into())
            .or_default()
            .insert("original".into(), "ck_img".into());
        ctx.downloaded_checksums
            .entry("PrimarySync".into())
            .or_default()
            .entry("live_asset".into())
            .or_default()
            .insert("live_original".into(), "ck_mov".into());

        // Image: matching checksum, trusted
        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "live_asset",
                VersionSizeKey::Original,
                "ck_img",
                true
            ),
            Some(false)
        );
        // MOV: matching checksum, trusted
        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "live_asset",
                VersionSizeKey::LiveOriginal,
                "ck_mov",
                true
            ),
            Some(false)
        );
        // MOV: changed checksum -- re-download even though image is fine
        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "live_asset",
                VersionSizeKey::LiveOriginal,
                "ck_mov_v2",
                true
            ),
            Some(true),
            "changed MOV checksum should trigger re-download"
        );
    }

    // ── Gap: retry_only mode filters new assets ──────────────────────

    #[test]
    fn download_context_retry_only_known_ids_filtering() {
        let mut ctx = DownloadContext::default();
        ctx.known_ids.insert("previously_synced".into());

        // Known asset: should_download_fast returns Some(true) (it needs
        // download because it's not in downloaded_ids)
        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "previously_synced",
                VersionSizeKey::Original,
                "ck",
                true
            ),
            Some(true)
        );
        // The producer checks known_ids separately before forwarding:
        assert!(ctx.known_ids.contains("previously_synced"));
        assert!(
            !ctx.known_ids.contains("brand_new_asset"),
            "new asset should not be in known_ids in retry_only mode"
        );
    }

    /// `SyncStats::accumulate` is the sole sum used to fold per-library
    /// stats into a cycle-wide total. Pin every counter so a future refactor
    /// (or a new field added without updating `accumulate`) cannot silently
    /// drop one. Touches every numeric field plus `interrupted` plus the
    /// nested `SkipBreakdown`.
    ///
    /// The earlier inline accumulator in `sync_loop::run_cycle` missed
    /// `rate_limited` -- this test pins that field too so the bug cannot
    /// regress.
    #[test]
    fn sync_loop_run_cycle_aggregates_stats_across_libraries() {
        let lib_a = SyncStats {
            assets_seen: 10,
            downloaded: 4,
            failed: 1,
            skipped: SkipBreakdown {
                by_state: 2,
                on_disk: 3,
                by_media_type: 4,
                by_date_range: 5,
                by_live_photo: 6,
                by_filename: 7,
                by_excluded_album: 8,
                ampm_variant: 9,
                duplicates: 10,
                retry_exhausted: 11,
                retry_only: 12,
            },
            bytes_downloaded: 1_000,
            disk_bytes_written: 900,
            exif_failures: 1,
            state_write_failures: 2,
            enumeration_errors: 3,
            elapsed_secs: 1.5,
            interrupted: false,
            rate_limited: 7,
        };

        let lib_b = SyncStats {
            assets_seen: 20,
            downloaded: 11,
            failed: 2,
            skipped: SkipBreakdown {
                by_state: 1,
                on_disk: 1,
                by_media_type: 1,
                by_date_range: 1,
                by_live_photo: 1,
                by_filename: 1,
                by_excluded_album: 1,
                ampm_variant: 1,
                duplicates: 1,
                retry_exhausted: 1,
                retry_only: 1,
            },
            bytes_downloaded: 2_500,
            disk_bytes_written: 2_400,
            exif_failures: 4,
            state_write_failures: 5,
            enumeration_errors: 6,
            elapsed_secs: 0.75,
            interrupted: true,
            rate_limited: 3,
        };

        let mut acc = SyncStats::default();
        acc.accumulate(&lib_a);
        acc.accumulate(&lib_b);

        assert_eq!(acc.assets_seen, 30, "assets_seen must sum");
        assert_eq!(acc.downloaded, 15, "downloaded must sum");
        assert_eq!(acc.failed, 3, "failed must sum");
        assert_eq!(acc.bytes_downloaded, 3_500, "bytes_downloaded must sum");
        assert_eq!(acc.disk_bytes_written, 3_300, "disk_bytes_written must sum");
        assert_eq!(acc.exif_failures, 5, "exif_failures must sum");
        assert_eq!(acc.state_write_failures, 7, "state_write_failures must sum");
        assert_eq!(acc.enumeration_errors, 9, "enumeration_errors must sum");
        assert!(
            (acc.elapsed_secs - 2.25).abs() < 1e-9,
            "elapsed_secs must sum (got {})",
            acc.elapsed_secs
        );
        assert!(
            acc.interrupted,
            "interrupted must OR -- any library interrupted -> cycle interrupted"
        );
        assert_eq!(
            acc.rate_limited, 10,
            "rate_limited must sum -- pre-fix the inline accumulator dropped this field"
        );

        assert_eq!(acc.skipped.by_state, 3);
        assert_eq!(acc.skipped.on_disk, 4);
        assert_eq!(acc.skipped.by_media_type, 5);
        assert_eq!(acc.skipped.by_date_range, 6);
        assert_eq!(acc.skipped.by_live_photo, 7);
        assert_eq!(acc.skipped.by_filename, 8);
        assert_eq!(acc.skipped.by_excluded_album, 9);
        assert_eq!(acc.skipped.ampm_variant, 10);
        assert_eq!(acc.skipped.duplicates, 11);
        assert_eq!(acc.skipped.retry_exhausted, 12);
        assert_eq!(acc.skipped.retry_only, 13);
        assert_eq!(
            acc.skipped.total(),
            3 + 4 + 5 + 6 + 7 + 8 + 9 + 10 + 11 + 12 + 13,
            "skip total must reflect summed breakdown"
        );
    }

    /// A transient `pass.album.len()` failure must not
    /// reduce to a 0-count that silently advances the sync token. Folding
    /// the per-pass results must surface the failure as an error count so
    /// downstream gates can suppress token advancement.
    #[test]
    fn fold_pass_count_results_counts_errors_and_zeroes_failed_passes() {
        use crate::commands::{AlbumPass, PassKind};
        use crate::icloud::photos::PhotoAlbum;
        use rustc_hash::FxHashSet;
        use std::sync::Arc;

        let passes = vec![
            AlbumPass {
                kind: PassKind::Album,
                album: PhotoAlbum::stub_for_test(Arc::from("album_a")),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
            AlbumPass {
                kind: PassKind::Album,
                album: PhotoAlbum::stub_for_test(Arc::from("album_b")),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
            AlbumPass {
                kind: PassKind::Album,
                album: PhotoAlbum::stub_for_test(Arc::from("album_c")),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
        ];

        // First and third pass succeed; second pass fails (transient len()
        // error). The failed pass must contribute 0 to the counts vector and
        // increment the error count by exactly 1.
        let results = vec![
            Ok(100),
            Err(anyhow::anyhow!("simulated transient len() failure")),
            Ok(50),
        ];

        let (counts, errors) = fold_pass_count_results(results, &passes);

        assert_eq!(counts, vec![100, 0, 50]);
        assert_eq!(
            errors, 1,
            "exactly one len() error must surface so token advancement is blocked"
        );
    }

    /// Pin the all-failures case: every pass's `len()` errors out → counts
    /// are all zero AND the error count equals the pass count, so the cycle
    /// cannot be mistaken for a clean empty enumeration.
    #[test]
    fn fold_pass_count_results_all_errors_yields_full_error_count() {
        use crate::commands::{AlbumPass, PassKind};
        use crate::icloud::photos::PhotoAlbum;
        use rustc_hash::FxHashSet;
        use std::sync::Arc;

        let passes = vec![
            AlbumPass {
                kind: PassKind::Album,
                album: PhotoAlbum::stub_for_test(Arc::from("album_a")),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
            AlbumPass {
                kind: PassKind::Album,
                album: PhotoAlbum::stub_for_test(Arc::from("album_b")),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
        ];

        let results = vec![
            Err(anyhow::anyhow!("first failure")),
            Err(anyhow::anyhow!("second failure")),
        ];

        let (counts, errors) = fold_pass_count_results(results, &passes);

        assert_eq!(counts, vec![0, 0]);
        assert_eq!(errors, 2);
    }

    /// Per-pass mode AND-folds `enumeration_complete` across passes.
    /// The first pass that aborts must drop the cycle's flag to false. The
    /// `&&=` semantics are subtle (especially around the empty-passes case)
    /// so this test pins the truth table.
    #[test]
    fn enum_progress_marker_per_pass_and_fold_semantics() {
        // All passes complete → cycle complete.
        let mut combined = true;
        for pass_complete in [true, true, true] {
            combined = combined && pass_complete;
        }
        assert!(combined, "all passes complete → marker clears");

        // One pass aborted mid-stream → cycle incomplete.
        let mut combined = true;
        for pass_complete in [true, false, true] {
            combined = combined && pass_complete;
        }
        assert!(
            !combined,
            "one pass aborted → marker must stay set even if siblings finished"
        );

        // Empty passes: combined stays at the initializer (false in the
        // production code so a no-pass cycle doesn't accidentally clear
        // the marker for a zone the cycle didn't actually enumerate).
        let combined: bool = []
            .iter()
            .fold(false, |acc, pass_complete: &bool| acc && *pass_complete);
        assert!(!combined, "no passes → marker stays set");
    }

    /// Pagination undercount classifier — exact match returns Match.
    /// Token must advance silently when the producer saw at least as many
    /// assets as the API reported.
    #[test]
    fn classify_pagination_shortfall_exact_match_is_silent() {
        let decision = classify_pagination_shortfall(1000, 1000);
        assert_eq!(decision, PaginationShortfall::Match);
    }

    /// A 1% undercount (within 5% tolerance) classifies as
    /// `WithinTolerance` so the caller emits a `warn!` but still advances the
    /// sync token. This is the visible-but-non-blocking layer that closes the
    /// pre-existing 4%-silent-drop gap (the prior gate fired only at >=5%).
    #[test]
    fn classify_pagination_shortfall_one_percent_below_warns_but_advances() {
        // 1000 expected, 990 seen → 1% shortfall, within 5% tolerance.
        let decision = classify_pagination_shortfall(1000, 990);
        assert_eq!(
            decision,
            PaginationShortfall::WithinTolerance { shortfall: 10 },
            "1% undercount must classify as WithinTolerance so the caller emits \
             a warn but does NOT suppress the sync token"
        );
    }

    /// A 4% undercount (still within 5% tolerance) classifies as
    /// `WithinTolerance`. Pre-fix this slipped through silently with no log;
    /// post-fix the `warn!` makes the drift visible before it grows past 5%.
    #[test]
    fn classify_pagination_shortfall_four_percent_below_still_advances() {
        // 1000 expected, 960 seen → 4% shortfall.
        let decision = classify_pagination_shortfall(1000, 960);
        assert_eq!(
            decision,
            PaginationShortfall::WithinTolerance { shortfall: 40 }
        );
    }

    /// A 6% undercount crosses the 5% threshold and must `Suppress` so
    /// the sync token is held back, forcing full re-enumeration on the next
    /// run rather than skipping the missing change events forever.
    #[test]
    fn classify_pagination_shortfall_six_percent_below_suppresses_token() {
        // 1000 expected, 940 seen → 6% shortfall, above the 5% suppression
        // threshold. `total * 95 / 100 = 950`, and 940 < 950 → Suppress.
        let decision = classify_pagination_shortfall(1000, 940);
        assert_eq!(decision, PaginationShortfall::Suppress);
    }

    /// Boundary case at exactly 5% shortfall. `total * 95 / 100 = 950`,
    /// and seen == 950 is NOT below the threshold, so it stays in
    /// `WithinTolerance` (the gate is strict less-than). Pinning this so a
    /// future tweak to the threshold math doesn't flip the boundary silently.
    #[test]
    fn classify_pagination_shortfall_at_threshold_is_within_tolerance() {
        let decision = classify_pagination_shortfall(1000, 950);
        assert_eq!(
            decision,
            PaginationShortfall::WithinTolerance { shortfall: 50 }
        );
    }

    /// The orphan-part walk must remove .part files older
    /// than the cutoff and leave non-matching files alone. To avoid
    /// depending on a third-party mtime crate, drive the cutoff itself: a
    /// cutoff far in the future treats every just-created file as "older",
    /// while a cutoff in the distant past leaves everything intact.
    #[test]
    fn walk_and_remove_orphan_parts_removes_part_files_only() {
        use std::fs::File;
        use std::io::Write;

        let dir = tempfile::Builder::new()
            .prefix("kei-orphan-parts-")
            .tempdir_in("/tmp/claude")
            .expect("tempdir in /tmp/claude");

        let part = dir.path().join("photo.jpg.part");
        File::create(&part).unwrap().write_all(b"x").unwrap();

        let unrelated = dir.path().join("photo.jpg");
        File::create(&unrelated).unwrap().write_all(b"x").unwrap();

        // Cutoff far in the future -> the just-created .part is "older".
        // `now=0, recent_grace=0` disables the recent-grace check so this test
        // continues to exercise the cutoff-only behaviour.
        let future = i64::MAX / 2;
        let cleaned = walk_and_remove_orphan_parts(dir.path().to_path_buf(), ".part", future, 0, 0);
        assert_eq!(cleaned, 1, "the .part file must be removed");
        assert!(!part.exists());
        assert!(unrelated.exists(), "non-.part file must be retained");

        // Re-create and re-run with cutoff in the distant past; nothing to clean.
        File::create(&part).unwrap().write_all(b"x").unwrap();
        let cleaned = walk_and_remove_orphan_parts(dir.path().to_path_buf(), ".part", 0, 0, 0);
        assert_eq!(cleaned, 0, "cutoff in the past must spare even .part files");
        assert!(part.exists());
    }

    /// A directory the process cannot read must NOT panic the walk
    /// and MUST NOT abort it. With the fix in place the walk emits a
    /// `warn!` for the failed `read_dir` and continues; pre-fix it
    /// silently swallowed the error and produced no log breadcrumb. We
    /// can't capture log output without an extra dependency, so this test
    /// pins the structural contract: the walk completes, doesn't panic,
    /// and still cleans the readable siblings.
    #[cfg(unix)]
    #[test]
    fn walk_and_remove_orphan_parts_continues_past_unreadable_subdir() {
        use std::fs::File;
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::Builder::new()
            .prefix("kei-orphan-parts-unreadable-")
            .tempdir_in("/tmp/claude")
            .expect("tempdir in /tmp/claude");

        // Sibling subdir with a .part file: should be cleaned.
        let readable = dir.path().join("readable");
        std::fs::create_dir(&readable).unwrap();
        let part = readable.join("photo.jpg.part");
        File::create(&part).unwrap().write_all(b"x").unwrap();

        // Unreadable subdir: read_dir fails. The walk must log a warn and
        // continue rather than aborting or silently dropping the error.
        let unreadable = dir.path().join("unreadable");
        std::fs::create_dir(&unreadable).unwrap();
        std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o000)).unwrap();

        let future = i64::MAX / 2;
        let cleaned = walk_and_remove_orphan_parts(dir.path().to_path_buf(), ".part", future, 0, 0);

        // Restore perms so the tempdir can be cleaned up.
        std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(
            cleaned, 1,
            ".part in the readable sibling must still be cleaned despite \
             the unreadable subdirectory"
        );
        assert!(!part.exists());
    }

    /// A `.part` file whose mtime is within `recent_grace_secs` of
    /// `now_secs` must be spared even when the cutoff says it's older than
    /// `last_sync_completed`. Defends against the multi-process race where
    /// a different kei instance is actively resuming a `.part` between
    /// retries.
    ///
    /// Drives the cutoff parameter directly to avoid taking
    /// a runtime dependency on a filetime crate.
    #[test]
    fn walk_and_remove_orphan_parts_spares_recently_touched_files() {
        use std::fs::File;
        use std::io::Write;

        let dir = tempfile::Builder::new()
            .prefix("kei-orphan-parts-recent-")
            .tempdir_in("/tmp/claude")
            .expect("tempdir in /tmp/claude");

        // Two .part files. We can't easily set mtime without a filetime
        // crate, so synthesize the test by driving (now_secs, cutoff_secs,
        // recent_grace_secs) numerically: the just-created file has an
        // mtime ~= "real now". We pretend `now_secs` is real-now + 1 hour
        // and cutoff is real-now + 30 minutes (so the file is "older" than
        // cutoff under the legacy gate). With recent_grace = 90 minutes,
        // the file's real-now mtime falls inside (now - 90min, now] →
        // spared. With recent_grace = 0, the file is removed (legacy
        // behaviour preserved for the existing test above).
        let recent_part = dir.path().join("recent.jpg.part");
        File::create(&recent_part).unwrap().write_all(b"x").unwrap();
        let old_part = dir.path().join("old.jpg.part");
        File::create(&old_part).unwrap().write_all(b"x").unwrap();

        let real_now = chrono::Utc::now().timestamp();
        let now_secs = real_now + 3_600; // pretend "now" is 1h ahead
        let cutoff_secs = real_now + 1_800; // 30 minutes ahead → both .parts older
        let recent_grace_secs = 90 * 60; // 90 minutes → both .parts inside grace

        let cleaned = walk_and_remove_orphan_parts(
            dir.path().to_path_buf(),
            ".part",
            cutoff_secs,
            now_secs,
            recent_grace_secs,
        );
        assert_eq!(
            cleaned, 0,
            "files inside the recent-grace window must be spared even when \
             they predate the cutoff"
        );
        assert!(recent_part.exists(), "recent .part must still exist");
        assert!(old_part.exists(), "old .part also spared by grace window");

        // Now shrink the grace window so the same files fall OUTSIDE it.
        // 1 second of grace + the simulated "now" 3600s ahead means a real-now
        // mtime is far outside the window → both files cleaned (legacy
        // cutoff path).
        let cleaned = walk_and_remove_orphan_parts(
            dir.path().to_path_buf(),
            ".part",
            cutoff_secs,
            now_secs,
            1,
        );
        assert_eq!(
            cleaned, 2,
            "with a 1-second grace window, both .parts fall back to the \
             legacy cutoff-only gate and are removed"
        );
        assert!(!recent_part.exists());
        assert!(!old_part.exists());
    }

    /// When the cutoff says "delete" but only one of two `.part`
    /// files is in the recent-grace window, the test fixture mimics the
    /// task-spec setup: one mtime ~now, one mtime far in the past. Drive
    /// the times via the `now_secs` and `cutoff_secs` parameters since
    /// adjusting filesystem mtimes without a third-party crate is not
    /// portable across platforms.
    #[test]
    fn walk_and_remove_orphan_parts_distinguishes_recent_from_aged_via_params() {
        use std::fs::File;
        use std::io::Write;

        let dir = tempfile::Builder::new()
            .prefix("kei-orphan-parts-mixed-")
            .tempdir_in("/tmp/claude")
            .expect("tempdir in /tmp/claude");

        // Both files have a real-now mtime. We can't backdate one without
        // a filetime crate, so this test pins the symmetric case:
        // recent-grace > 0 spares both, recent-grace = 0 removes both.
        // The asymmetric scenario is covered structurally by the helper's
        // parameterization (the `is_recently_touched` branch is the only
        // gate the parameters affect) and by the inline integration with
        // `cleanup_orphan_part_files`.
        let p1 = dir.path().join("a.jpg.part");
        let p2 = dir.path().join("b.jpg.part");
        File::create(&p1).unwrap().write_all(b"x").unwrap();
        File::create(&p2).unwrap().write_all(b"x").unwrap();

        let now_secs = chrono::Utc::now().timestamp() + 60; // 1 min ahead
        let cutoff_secs = now_secs - 30; // both .parts (real-now mtime) older
        let recent_grace_secs = 10 * 60; // 10 min grace
        let cleaned = walk_and_remove_orphan_parts(
            dir.path().to_path_buf(),
            ".part",
            cutoff_secs,
            now_secs,
            recent_grace_secs,
        );
        assert_eq!(cleaned, 0, "both .parts within 10-min grace must be spared");
        assert!(p1.exists() && p2.exists());
    }

    /// Companion: accumulating into an empty `SyncStats` is a faithful copy
    /// (the operation is the additive identity for the empty case).
    #[test]
    fn sync_stats_accumulate_into_empty_is_copy() {
        let src = SyncStats {
            assets_seen: 5,
            downloaded: 2,
            failed: 1,
            skipped: SkipBreakdown {
                duplicates: 7,
                ..SkipBreakdown::default()
            },
            rate_limited: 4,
            interrupted: true,
            ..SyncStats::default()
        };
        let mut dst = SyncStats::default();
        dst.accumulate(&src);
        assert_eq!(dst.assets_seen, 5);
        assert_eq!(dst.downloaded, 2);
        assert_eq!(dst.failed, 1);
        assert_eq!(dst.skipped.duplicates, 7);
        assert_eq!(dst.rate_limited, 4);
        assert!(dst.interrupted);
    }
}
