//! Download configuration, coverage fingerprints, and compatibility hashes.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use rustc_hash::FxHashSet;

use crate::download::filter::AssetGroupings;
use crate::download::limiter::BandwidthLimiter;
use crate::download::metadata_rewrite::CaptureTimestampRepair;
use crate::download::{filter, paths};
use crate::retry::RetryConfig;
use crate::types::{
    AssetVersionSize, FileMatchPolicy, LivePhotoMode, LivePhotoMovFilenamePolicy, RawPolicy,
};

use super::models::{DownloadStore, SyncMode};

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
pub(in crate::download) fn hash_bytes(hasher: &mut sha2::Sha256, bytes: &[u8]) {
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

/// Bump this when path derivation changes without a corresponding config
/// field changing. That forces existing state to revalidate on disk instead
/// of trusting paths derived under older code.
const LEGACY_PATH_DERIVATION_HASH_VERSION: u8 = 2;

const PATH_DERIVATION_HASH_VERSION: u8 = 3;

const ENUMERATION_SAFETY_HASH_VERSION: u8 = 2;

pub(crate) const DOWNLOAD_CONFIG_HASH_KEY: &str = "config_hash";

/// Fields shared between [`hash_download_config`] and [`compute_config_hash`]
/// that affect path resolution and asset eligibility.
#[derive(Debug)]
struct SharedHashFields<'a> {
    directory: &'a std::path::Path,
    folder_structure: &'a str,
    folder_structure_albums: &'a str,
    folder_structure_smart_folders: &'a str,
    resolution: crate::types::PhotoResolution,
    live_resolution: AssetVersionSize,
    file_match_policy: FileMatchPolicy,
    live_photo_mov_filename_policy: LivePhotoMovFilenamePolicy,
    edited: bool,
    alternative: bool,
    raw_policy: RawPolicy,
    keep_unicode_in_filenames: bool,
    skip_created_before: Option<crate::config::CreatedDateFilter>,
    skip_created_after: Option<crate::config::CreatedDateFilter>,
    force_resolution: bool,
    media: crate::config::MediaSelection,
    live_photo_mode: LivePhotoMode,
    filename_exclude: &'a [glob::Pattern],
}

/// Hash the shared config fields into the hasher. All enum values use
/// `repr(u8)` byte representations and dates use "YYYY-MM-DD" Display
/// format for stability across compiler/library upgrades.
fn hash_shared_fields(hasher: &mut sha2::Sha256, version: u8, f: &SharedHashFields<'_>) {
    use sha2::Digest;

    hasher.update([version]);
    hash_bytes(hasher, f.directory.as_os_str().as_encoded_bytes());
    hash_bytes(hasher, f.folder_structure.as_bytes());
    hash_bytes(hasher, f.folder_structure_albums.as_bytes());
    hash_bytes(hasher, f.folder_structure_smart_folders.as_bytes());
    hasher.update([f.resolution as u8]);
    hasher.update([f.live_resolution as u8]);
    hasher.update([f.file_match_policy as u8]);
    hasher.update([f.live_photo_mov_filename_policy as u8]);
    hasher.update([u8::from(f.edited)]);
    hasher.update([u8::from(f.alternative)]);
    hasher.update([f.raw_policy as u8]);
    hasher.update([u8::from(f.keep_unicode_in_filenames)]);
    // Eligibility fields stay in the local trust hash where changing them can
    // alter the selected versions. Current path hashing passes no date bounds
    // because the enumeration hash already owns date eligibility; the legacy
    // caller supplies them only to reproduce the v2 shape.
    //
    // Dates are truncated to day precision before hashing so that relative
    // intervals like "20d" (resolved to now-minus-20-days at parse time)
    // produce a stable hash across consecutive runs on the same day.
    hash_optional_created_date_filter(hasher, f.skip_created_before);
    hash_optional_created_date_filter(hasher, f.skip_created_after);
    hasher.update([u8::from(f.force_resolution)]);
    hasher.update([u8::from(f.media.photos)]);
    hasher.update([u8::from(f.media.videos)]);
    hasher.update([u8::from(f.media.live_photos)]);
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

fn hash_optional_created_date_filter(
    hasher: &mut sha2::Sha256,
    filter: Option<crate::config::CreatedDateFilter>,
) {
    use crate::config::CreatedDateFilter;
    use sha2::Digest;

    match filter {
        None => hash_optional_date(hasher, None),
        Some(CreatedDateFilter::Instant(boundary)) => {
            // Preserve the pre-capture-date hash shape for instant cutoffs.
            hash_optional_date(hasher, Some(boundary.date_naive()));
        }
        Some(CreatedDateFilter::CaptureDate(boundary)) => {
            // A distinct tag invalidates only filters whose semantics changed.
            hasher.update([2]);
            hasher.update(boundary.to_string().as_bytes());
        }
    }
}

fn selector_set_fingerprint_json(set: &BTreeSet<String>) -> serde_json::Value {
    let values: Vec<&str> = set.iter().map(String::as_str).collect();
    serde_json::json!(values)
}

fn album_selector_fingerprint_json(
    selector: &crate::selection::AlbumSelector,
) -> serde_json::Value {
    use crate::selection::AlbumSelector;
    match selector {
        AlbumSelector::None => serde_json::json!({"kind": "none"}),
        AlbumSelector::All { excluded } => {
            serde_json::json!({"kind": "all", "excluded": selector_set_fingerprint_json(excluded)})
        }
        AlbumSelector::Named { included, excluded } => serde_json::json!({
            "kind": "named",
            "included": selector_set_fingerprint_json(included),
            "excluded": selector_set_fingerprint_json(excluded),
        }),
    }
}

fn smart_folder_selector_fingerprint_json(
    selector: &crate::selection::SmartFolderSelector,
) -> serde_json::Value {
    use crate::selection::SmartFolderSelector;
    match selector {
        SmartFolderSelector::None => serde_json::json!({"kind": "none"}),
        SmartFolderSelector::All {
            include_sensitive,
            excluded,
        } => serde_json::json!({
            "kind": "all",
            "include_sensitive": include_sensitive,
            "excluded": selector_set_fingerprint_json(excluded),
        }),
        SmartFolderSelector::Named { included, excluded } => serde_json::json!({
            "kind": "named",
            "included": selector_set_fingerprint_json(included),
            "excluded": selector_set_fingerprint_json(excluded),
        }),
    }
}

fn library_selector_fingerprint_json(
    selector: &crate::selection::LibrarySelector,
) -> serde_json::Value {
    serde_json::json!({
        "primary": selector.primary,
        "shared_all": selector.shared_all,
        "named": selector_set_fingerprint_json(&selector.named),
        "excluded": selector_set_fingerprint_json(&selector.excluded),
    })
}

/// Build the canonical coverage fingerprint stored with scoped
/// `/changes/database` precheck tokens.
///
/// Keep this next to the download and enumeration hash owners because it is
/// the durable audit shape that combines selection, filter coverage, enum
/// safety, and path/download eligibility proof.
pub(crate) fn sync_coverage_fingerprint_json(
    config: &crate::config::Config,
    provider: &str,
    shape_version: i64,
    selected_zones: &[String],
    enum_config_hash: &str,
    download_config_hash: &str,
) -> anyhow::Result<String> {
    let skip_created_before = config
        .filters
        .skip_created_before
        .map(crate::config::CreatedDateFilter::fingerprint);
    let skip_created_after = config
        .filters
        .skip_created_after
        .map(crate::config::CreatedDateFilter::fingerprint);
    let mut filename_exclude: Vec<&str> = config
        .download
        .filename_exclude
        .iter()
        .map(glob::Pattern::as_str)
        .collect();
    filename_exclude.sort_unstable();
    let coverage = if let Some(count) = config.filters.recent {
        serde_json::json!({
            "kind": "bounded-recent-count",
            "count": count,
            "recent_scope": config.filters.recent_scope,
        })
    } else if skip_created_before.is_some() || skip_created_after.is_some() {
        serde_json::json!({
            "kind": "bounded-date-window",
            "skip_created_before": skip_created_before,
            "skip_created_after": skip_created_after,
        })
    } else {
        serde_json::json!({"kind": "complete"})
    };

    serde_json::to_string(&serde_json::json!({
        "provider": provider,
        "domain": config.auth.domain.as_str(),
        "shape_version": shape_version,
        "selected_zones": selected_zones,
        "selection": {
            "albums": album_selector_fingerprint_json(&config.filters.selection.albums),
            "albums_explicit": config.filters.selection.albums_explicit,
            "smart_folders": smart_folder_selector_fingerprint_json(&config.filters.selection.smart_folders),
            "smart_folders_explicit": config.filters.selection.smart_folders_explicit,
            "libraries": library_selector_fingerprint_json(&config.filters.selection.libraries),
            "unfiled": config.filters.selection.unfiled,
        },
        "filters": {
            "media": {
                "photos": config.filters.media.photos,
                "videos": config.filters.media.videos,
                "live_photos": config.filters.media.live_photos,
            },
            "filename_exclude": filename_exclude,
            "skip_created_before": skip_created_before,
            "skip_created_after": skip_created_after,
            "recent": config.filters.recent,
            "recent_scope": config.filters.recent_scope,
        },
        "coverage": coverage,
        "enum_config_hash": enum_config_hash,
        "download_config_hash": download_config_hash,
    }))
    .context("serialize sync coverage fingerprint")
}

/// Compute a deterministic hash of the config fields that affect path resolution.
///
/// When this hash changes between runs, we can't trust the state DB's download
/// records (the resolved paths may differ), so we fall back to the full pipeline
/// with filesystem existence checks.
///
/// Separate from [`compute_config_hash`]: path-only changes revalidate local
/// download state without clearing CloudKit zone tokens.
pub(crate) fn hash_download_config(config: &DownloadConfig) -> String {
    hash_download_config_with_date_bounds(config, PATH_DERIVATION_HASH_VERSION, false)
}

/// Reproduce the v2 mixed path-and-date hash for in-place migration.
pub(crate) fn hash_legacy_download_config(config: &DownloadConfig) -> String {
    hash_download_config_with_date_bounds(config, LEGACY_PATH_DERIVATION_HASH_VERSION, true)
}

fn hash_download_config_with_date_bounds(
    config: &DownloadConfig,
    version: u8,
    include_date_bounds: bool,
) -> String {
    use sha2::{Digest, Sha256};

    let (skip_created_before, skip_created_after) = if include_date_bounds {
        (config.skip_created_before, config.skip_created_after)
    } else {
        (None, None)
    };
    let mut hasher = Sha256::new();
    hash_shared_fields(
        &mut hasher,
        version,
        &SharedHashFields {
            directory: &config.directory,
            folder_structure: &config.folder_structure,
            folder_structure_albums: &config.folder_structure_albums,
            folder_structure_smart_folders: &config.folder_structure_smart_folders,
            resolution: config.resolution,
            live_resolution: config.live_resolution,
            file_match_policy: config.file_match_policy,
            live_photo_mov_filename_policy: config.live_photo_mov_filename_policy,
            edited: config.edited,
            alternative: config.alternative,
            raw_policy: config.raw_policy,
            keep_unicode_in_filenames: config.keep_unicode_in_filenames,
            skip_created_before,
            skip_created_after,
            force_resolution: config.force_resolution,
            media: config.media,
            live_photo_mode: config.live_photo_mode,
            filename_exclude: &config.filename_exclude,
        },
    );
    // `recent` affects which already-downloaded assets to trust/skip
    hash_optional_u32(&mut hasher, config.recent);
    if config.recent.is_some() {
        hasher.update(b"recent_scope:");
        hasher.update(match config.recent_scope {
            crate::cli::RecentScope::Global => b"global".as_slice(),
            crate::cli::RecentScope::PerFilter => b"per-filter".as_slice(),
        });
        hasher.update(b"\0");
    }
    finalize_hash(hasher)
}

/// Compute the config hash from the app-level `Config`.
///
/// Called before the sync-mode decision so stale sync tokens are cleared only
/// when an unsafe eligibility/config change cannot be routed incrementally.
///
/// This hash tracks only changes that make a stored CloudKit zone token unsafe.
/// Path-only fields stay in [`hash_download_config`] so a folder/template
/// change revalidates local files without discarding the CloudKit cursor.
/// Album, library, and smart-folder selection are also excluded here: the
/// incremental router can prove those cases from per-library tokens, trusted
/// album snapshots, and targeted smart-folder refreshes.
pub(crate) fn compute_config_hash(config: &crate::config::Config) -> String {
    use sha2::{Digest, Sha256};

    let live_resolution = config.photos.live_resolution.to_asset_version_size();
    let mut hasher = Sha256::new();
    hasher.update([ENUMERATION_SAFETY_HASH_VERSION]);
    hasher.update([config.photos.resolution as u8]);
    hasher.update([live_resolution as u8]);
    hasher.update([u8::from(config.photos.edited)]);
    hasher.update([u8::from(config.photos.alternative)]);
    hasher.update([config.photos.raw_policy as u8]);
    hash_optional_created_date_filter(&mut hasher, config.filters.skip_created_before);
    hash_optional_created_date_filter(&mut hasher, config.filters.skip_created_after);
    hasher.update([u8::from(config.photos.force_resolution)]);
    hasher.update([u8::from(config.filters.media.photos)]);
    hasher.update([u8::from(config.filters.media.videos)]);
    hasher.update([u8::from(config.filters.media.live_photos)]);
    hasher.update([config.photos.live_photo_mode as u8]);
    let mut sorted_excludes: Vec<&str> = config
        .download
        .filename_exclude
        .iter()
        .map(glob::Pattern::as_str)
        .collect();
    sorted_excludes.sort_unstable();
    for pattern in &sorted_excludes {
        hash_bytes(&mut hasher, pattern.as_bytes());
    }
    // Note: `recent` is intentionally excluded from this enum hash.
    // Changing --recent should not invalidate sync tokens because the
    // incremental path already applies the recent cap post-fetch.
    // `recent` IS included in hash_download_config (trust-state) so
    // changing it still triggers filesystem re-verification.

    // The unfiled selector is still unsafe to classify from the current state
    // alone: switching it on can make old, never-enumerated unfiled assets
    // newly eligible. Keep the full fallback for that unknown drift class.
    hasher.update(b"unfiled:");
    hasher.update([u8::from(config.filters.selection.unfiled)]);
    finalize_hash(hasher)
}

/// Subset of application config consumed by the download engine.
/// Decoupled from CLI parsing so the engine can be tested independently.
#[derive(Clone)]
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
    pub(crate) resolution: crate::types::PhotoResolution,
    pub(crate) media: crate::config::MediaSelection,
    pub(crate) skip_created_before: Option<crate::config::CreatedDateFilter>,
    pub(crate) skip_created_after: Option<crate::config::CreatedDateFilter>,
    pub(crate) metadata: crate::config::MetadataConfig,
    pub(crate) refresh_metadata: bool,
    pub(crate) capture_timestamp_repair: CaptureTimestampRepair,
    pub(crate) repair_truncated: bool,
    pub(crate) concurrent_downloads: usize,
    pub(crate) recent: Option<u32>,
    pub(crate) recent_scope: crate::cli::RecentScope,
    pub(crate) retry: RetryConfig,
    pub(crate) live_photo_mode: LivePhotoMode,
    pub(crate) live_resolution: AssetVersionSize,
    pub(crate) live_photo_mov_filename_policy: LivePhotoMovFilenamePolicy,
    pub(crate) edited: bool,
    pub(crate) alternative: bool,
    pub(crate) raw_policy: RawPolicy,
    pub(crate) file_match_policy: FileMatchPolicy,
    pub(crate) force_resolution: bool,
    pub(crate) keep_unicode_in_filenames: bool,
    /// Compiled glob patterns for filename exclusion.
    ///
    /// Behind `Arc<[_]>` so per-pass clones share one allocation
    /// (significant with `-a all` over 100+ albums).
    pub(crate) filename_exclude: Arc<[glob::Pattern]>,
    /// Temp file suffix for partial downloads (e.g. `.kei-tmp`).
    pub(crate) temp_suffix: Arc<str>,
    /// State database for tracking download progress.
    pub(crate) state_db: Option<Arc<dyn DownloadStore>>,
    /// When true (retry-failed mode), only download assets already known to the
    /// state DB. Skip new assets discovered from iCloud that were never synced.
    pub(crate) retry_only: bool,
    /// Sync mode: full enumeration or incremental delta via syncToken.
    pub(crate) sync_mode: SyncMode,
    /// Hash of enumeration-affecting config. Full album snapshots persist this
    /// so later routing can prove a trusted snapshot still matches the plan.
    pub(crate) enum_config_hash: Option<Arc<str>>,
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
    /// Human-readable label for the active pass: the album's own name for
    /// album/smart-folder passes, "unfiled" for the unfiled pass (which uses
    /// `library.all()` whose `.name` is the empty string).
    pub(crate) fn pass_label(&self) -> &str {
        match self.album_name.as_deref() {
            Some("") | None => "unfiled",
            Some(name) => name,
        }
    }

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
    pub(crate) fn with_pass(&self, pass: &crate::commands::AlbumPass) -> Self {
        let folder_structure = filter::folder_structure_for_pass(
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

    /// Clone this config with a different `exclude_asset_ids` set. Used
    /// for the merged (non-`{album}`) full-sync path, where all passes
    /// share a single config but the exclude set is lifted off the plan.
    pub(in crate::download) fn with_exclude_ids(
        &self,
        exclude_ids: Arc<FxHashSet<String>>,
    ) -> Self {
        Self {
            exclude_asset_ids: exclude_ids,
            ..self.clone()
        }
    }

    pub(in crate::download) fn with_recent_scope(
        &self,
        recent_scope: crate::cli::RecentScope,
    ) -> Self {
        Self {
            recent_scope,
            ..self.clone()
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
            .field("resolution", &self.resolution)
            .field("media", &self.media)
            .field("skip_created_before", &self.skip_created_before)
            .field("skip_created_after", &self.skip_created_after);
        s.field("metadata", &self.metadata)
            .field("refresh_metadata", &self.refresh_metadata)
            .field("capture_timestamp_repair", &self.capture_timestamp_repair)
            .field("repair_truncated", &self.repair_truncated)
            .field("concurrent_downloads", &self.concurrent_downloads)
            .field("recent", &self.recent)
            .field("recent_scope", &self.recent_scope)
            .field("retry", &self.retry)
            .field("live_photo_mode", &self.live_photo_mode)
            .field("live_resolution", &self.live_resolution)
            .field(
                "live_photo_mov_filename_policy",
                &self.live_photo_mov_filename_policy,
            )
            .field("edited", &self.edited)
            .field("alternative", &self.alternative)
            .field("raw_policy", &self.raw_policy)
            .field("file_match_policy", &self.file_match_policy)
            .field("force_resolution", &self.force_resolution)
            .field("keep_unicode_in_filenames", &self.keep_unicode_in_filenames)
            .field("filename_exclude", &self.filename_exclude)
            .field("temp_suffix", &self.temp_suffix)
            .field("state_db", &self.state_db.is_some())
            .field("retry_only", &self.retry_only)
            .field("sync_mode", &self.sync_mode)
            .field("enum_config_hash", &self.enum_config_hash)
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
    pub(crate) fn test_default() -> Self {
        use rustc_hash::FxHashSet;
        Self {
            directory: Arc::from(Path::new("/nonexistent/download_filter_tests")),
            folder_structure: "{:%Y/%m/%d}".to_string(),
            folder_structure_albums: Arc::from(crate::config::DEFAULT_FOLDER_STRUCTURE_ALBUMS),
            folder_structure_smart_folders: Arc::from(
                crate::config::DEFAULT_FOLDER_STRUCTURE_SMART_FOLDERS,
            ),
            resolution: crate::types::PhotoResolution::Original,
            media: crate::config::MediaSelection::all(),
            skip_created_before: None,
            skip_created_after: None,
            metadata: crate::config::MetadataConfig::default(),
            refresh_metadata: false,
            capture_timestamp_repair: CaptureTimestampRepair::Preserve,
            repair_truncated: false,
            concurrent_downloads: 1,
            recent: None,
            recent_scope: crate::cli::RecentScope::Global,
            retry: crate::retry::RetryConfig::default(),
            live_photo_mode: LivePhotoMode::Both,
            live_resolution: AssetVersionSize::LiveOriginal,
            live_photo_mov_filename_policy: crate::types::LivePhotoMovFilenamePolicy::Suffix,
            edited: false,
            alternative: false,
            raw_policy: RawPolicy::AsIs,
            file_match_policy: FileMatchPolicy::NameSizeDedupWithSuffix,
            force_resolution: false,
            keep_unicode_in_filenames: false,
            filename_exclude: Arc::from(Vec::<glob::Pattern>::new()),
            temp_suffix: Arc::from(".kei-tmp"),
            state_db: None,
            retry_only: false,
            max_download_attempts: 10,
            sync_mode: SyncMode::Full,
            enum_config_hash: None,
            album_name: None,
            exclude_asset_ids: std::sync::Arc::new(FxHashSet::default()),
            asset_groupings: Arc::new(AssetGroupings::default()),
            bandwidth_limiter: None,
            library: Arc::from(crate::icloud::photos::PRIMARY_ZONE_NAME),
        }
    }
}

#[cfg(test)]
mod tests;
