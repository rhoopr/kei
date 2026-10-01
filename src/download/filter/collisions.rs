//! Collision policy, normalized claims, and existing-file path resolution.

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use chrono::{DateTime, FixedOffset};
use rustc_hash::FxHashMap;

use crate::download::{DownloadConfig, paths};

/// A normalized path string for case-insensitive collision detection.
///
/// On case-insensitive filesystems (macOS, Windows), we need to detect collisions between
/// paths like `IMG_0996.mov` and `IMG_0996.MOV`. This stores the normalized (lowercased)
/// form as a `Box<str>` and implements `Borrow<str>` to enable zero-copy lookups.
///
/// Use `NormalizedPath::normalize()` for temporary lookup keys to avoid `PathBuf` cloning.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(in crate::download) struct NormalizedPath(Box<str>);

impl NormalizedPath {
    pub(in crate::download) fn from_key(key: crate::state::ReconciliationPathKey) -> Self {
        Self(key.0.into_boxed_str())
    }

    /// Create a new normalized path from a borrowed `Path`.
    /// For lookup operations, prefer `normalize()` to avoid `PathBuf` cloning.
    pub(in crate::download) fn new(path: &Path) -> Self {
        Self(Self::normalize(path).into_owned().into_boxed_str())
    }

    /// Normalize a path reference for map lookups.
    ///
    /// On case-insensitive systems (macOS, Windows), returns a lowercase copy.
    /// On case-sensitive systems (Linux), returns a borrowed view when possible.
    ///
    /// Use with `claimed_paths.contains_key(NormalizedPath::normalize(&path).as_ref())`
    /// to avoid allocating a `PathBuf` just for the lookup.
    pub(in crate::download) fn normalize(path: &Path) -> Cow<'_, str> {
        crate::fs_util::normalized_path(path)
    }
}

impl AsRef<str> for NormalizedPath {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::borrow::Borrow<str> for NormalizedPath {
    fn borrow(&self) -> &str {
        &self.0
    }
}

/// Pre-populate the `DirCache` for the asset's date-based parent directory
/// on the blocking threadpool, so that subsequent sync `DirCache` lookups
/// inside `filter_asset_to_tasks` are guaranteed cache-hits.
pub(in crate::download) async fn pre_ensure_asset_dir(
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
pub(super) enum CollisionStrategy {
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

/// Existing files still need state and metadata finalization during reconciliation.
#[derive(Debug, Clone, Copy)]
pub(in crate::download) enum PathPlanningMode {
    Download,
    ReservedDownload,
    Reconciliation,
}

impl PathPlanningMode {
    /// Reconciliation keys compare equivalent root spellings without changing
    /// task paths. Invalid parent components remain errors, never aliases.
    fn normalize(self, path: &Path) -> std::io::Result<Cow<'_, str>> {
        match self {
            Self::Download => Ok(NormalizedPath::normalize(path)),
            Self::ReservedDownload | Self::Reconciliation => {
                crate::fs_util::confined_path_key(path).map(Cow::Owned)
            }
        }
    }

    pub(in crate::download) fn key(self, path: &Path) -> std::io::Result<NormalizedPath> {
        if matches!(self, Self::Download) {
            return Ok(NormalizedPath::new(path));
        }
        Ok(NormalizedPath(
            self.normalize(path)?.into_owned().into_boxed_str(),
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PathResolution {
    Download(PathBuf),
    Skip(PathBuf),
}

impl PathResolution {
    pub(super) fn download_path(self) -> Option<PathBuf> {
        match self {
            Self::Download(path) => Some(path),
            Self::Skip(_) => None,
        }
    }

    pub(super) fn effective_path(&self) -> &Path {
        match self {
            Self::Download(path) | Self::Skip(path) => path.as_path(),
        }
    }
}

/// Shared context for `resolve_download_path` -- groups the mutable/config
/// references that every call needs so the function stays under clippy's
/// argument limit.
#[derive(Debug)]
pub(super) struct ResolveContext<'a> {
    pub(super) config: &'a DownloadConfig,
    pub(super) planning_mode: PathPlanningMode,
    pub(super) created_local: &'a DateTime<FixedOffset>,
    pub(super) claimed_paths: &'a FxHashMap<NormalizedPath, u64>,
    pub(super) dir_cache: &'a mut paths::DirCache,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CollisionFilenameKind {
    /// The policy's historical collision filename: size suffix for primary
    /// media, asset-id suffix for live-photo MOV companions.
    Default,
    /// Asset-id suffix. Used when a same-size collision cannot be proven to be
    /// the same asset/version by the state layer.
    AssetIdentity,
    /// Stable fallback when the plain asset-id path is also occupied.
    AssetIdentityOrdinal(u64),
}

pub(super) fn size_or_identity_collision_filename(
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

pub(super) fn identity_collision_filename(
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
) -> std::io::Result<PathBuf> {
    let mut tried = Vec::<Box<str>>::with_capacity(4);

    for kind in [preferred, CollisionFilenameKind::AssetIdentity] {
        if let Some(path) = available_collision_path(ctx, &make_filename, kind, &mut tried)? {
            return Ok(path);
        }
    }

    let mut ordinal = 2u64;
    loop {
        if let Some(path) = available_collision_path(
            ctx,
            &make_filename,
            CollisionFilenameKind::AssetIdentityOrdinal(ordinal),
            &mut tried,
        )? {
            return Ok(path);
        }
        ordinal = ordinal.checked_add(1).unwrap_or(2);
    }
}

fn available_collision_path(
    ctx: &mut ResolveContext<'_>,
    make_filename: &impl Fn(CollisionFilenameKind) -> String,
    kind: CollisionFilenameKind,
    tried: &mut Vec<Box<str>>,
) -> std::io::Result<Option<PathBuf>> {
    let filename = make_filename(kind);
    let path = collision_path_for_filename(ctx, &filename);
    let normalized = ctx.planning_mode.normalize(&path)?.into_owned();
    if tried.iter().any(|seen| seen.as_ref() == normalized) {
        return Ok(None);
    }

    let unavailable = ctx.claimed_paths.contains_key(normalized.as_str())
        || (!matches!(ctx.planning_mode, PathPlanningMode::Reconciliation)
            && ctx.dir_cache.exists(&path));
    tried.push(normalized.into_boxed_str());
    Ok(if unavailable { None } else { Some(path) })
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
pub(super) fn resolve_download_path(
    download_path: &Path,
    version_size: u64,
    asset_id: &str,
    strategy: CollisionStrategy,
    ctx: &mut ResolveContext<'_>,
    check_ampm: bool,
    make_collision_filename: impl Fn(CollisionFilenameKind) -> String,
    label: &str,
) -> std::io::Result<PathResolution> {
    // Reconciliation must retry one deterministic destination after media
    // publication but before metadata/state completion. The confined copy
    // owner compares bytes and rejects conflicts instead of inventing a new
    // filename on every retry. Ordinary downloads retain collision naming.
    let normalized = ctx.planning_mode.normalize(download_path)?;
    if matches!(ctx.planning_mode, PathPlanningMode::Reconciliation)
        || (matches!(ctx.planning_mode, PathPlanningMode::ReservedDownload)
            && ctx.claimed_paths.contains_key(normalized.as_ref()))
    {
        if !ctx.claimed_paths.contains_key(normalized.as_ref()) {
            return Ok(PathResolution::Download(download_path.to_path_buf()));
        }
        // Another catalog asset owns this path. Reconciliation ignores disk
        // existence for stable retries; downloads still avoid occupied siblings.
        return Ok(PathResolution::Download(first_available_collision_path(
            ctx,
            CollisionFilenameKind::AssetIdentity,
            make_collision_filename,
        )?));
    }
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
        let normalized = ctx.planning_mode.normalize(download_path)?;
        if let Some(&size) = ctx.claimed_paths.get(normalized.as_ref()) {
            Some((size, "in-flight", download_path.to_path_buf()))
        } else {
            None
        }
    };

    let Some((existing_size, source, matched_path)) = existing_match else {
        // Path is unclaimed -- use it directly.
        return Ok(PathResolution::Download(download_path.to_path_buf()));
    };

    Ok(match strategy {
        CollisionStrategy::SkipIfExists => {
            if source == "on-disk" {
                tracing::info!(target: "kei::download::filter",
                    asset_id,
                    path = %download_path.display(),
                    "Skipping {label}: file exists (name-id7)"
                );
            } else {
                tracing::info!(target: "kei::download::filter",
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
                first_available_collision_path(ctx, preferred, make_collision_filename)?;
            if source == "on-disk" {
                tracing::debug!(target: "kei::download::filter",
                    asset_id,
                    path = %download_path.display(),
                    on_disk_size = existing_size,
                    expected_size = version_size,
                    collision_path = %collision_path.display(),
                    same_size = sizes_match,
                    "Resolved {label} path collision"
                );
            } else {
                tracing::debug!(target: "kei::download::filter",
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
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use rustc_hash::{FxHashMap, FxHashSet};
    use tempfile::TempDir;

    #[cfg(unix)]
    use crate::download::DownloadConfig;
    use crate::download::paths;
    #[cfg(unix)]
    use crate::icloud::photos::{PRIMARY_ZONE_NAME, PhotoAsset};
    #[cfg(unix)]
    use crate::state::SqliteStateDb;
    #[cfg(unix)]
    use crate::test_helpers::TestAssetRecord;
    use crate::test_helpers::TestPhotoAsset;
    use crate::types::FileMatchPolicy;

    use super::super::tasks::filter_asset_to_tasks;
    use super::super::test_support::{filter_asset_fresh, test_config, test_live_photo_asset};
    use super::{NormalizedPath, PathPlanningMode};

    #[cfg(unix)]
    fn post_rename_pre_state_final_path_for(download_dir: &std::path::Path) -> std::path::PathBuf {
        let asset = post_rename_pre_state_crash_asset();
        let config = post_rename_pre_state_config(download_dir);
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 1);
        tasks[0].download_path.clone()
    }

    #[cfg(unix)]
    fn post_rename_pre_state_config(download_dir: &std::path::Path) -> DownloadConfig {
        let mut config = test_config();
        config.directory = Arc::from(download_dir);
        config
    }

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
    const POST_RENAME_PRE_STATE_PART_FILE: &str = "published-before-state.kei-tmp";

    #[cfg(unix)]
    const POST_RENAME_PRE_STATE_CHECKSUM: &str = "ck_real_kill";

    #[cfg(unix)]
    const POST_RENAME_PRE_STATE_FILENAME: &str = "IMG_REAL_KILL.JPG";

    #[cfg(unix)]
    const POST_RENAME_PRE_STATE_ASSET_ID: &str = "ASSET_REAL_KILL";

    #[cfg(unix)]
    const POST_RENAME_PRE_STATE_CHILD_TEST: &str =
        "download::filter::collisions::tests::post_rename_pre_state_sigkill_child";

    #[cfg(unix)]
    const POST_RENAME_PRE_STATE_PART_ENV: &str = "KEI_CRASH_HARNESS_PART";

    #[cfg(unix)]
    const POST_RENAME_PRE_STATE_DB_ENV: &str = "KEI_CRASH_HARNESS_DB";

    #[cfg(unix)]
    const POST_RENAME_PRE_STATE_DOWNLOAD_DIR_ENV: &str = "KEI_CRASH_HARNESS_DOWNLOAD_DIR";

    #[cfg(unix)]
    const POST_RENAME_PRE_STATE_CHILD_ENV: &str = "KEI_POST_RENAME_PRE_STATE_SIGKILL_CHILD";

    #[test]
    fn reconciliation_path_keys_preserve_confinement_and_download_spelling() {
        let relative = Path::new("photos/./IMG.JPG");
        let absolute = std::env::current_dir().unwrap().join("photos/IMG.JPG");
        assert_eq!(
            PathPlanningMode::Reconciliation.key(relative).unwrap(),
            PathPlanningMode::Reconciliation.key(&absolute).unwrap()
        );
        assert_eq!(
            PathPlanningMode::ReservedDownload.key(relative).unwrap(),
            PathPlanningMode::Reconciliation.key(&absolute).unwrap()
        );
        assert!(
            PathPlanningMode::ReservedDownload
                .key(Path::new("photos/../IMG.JPG"))
                .is_err()
        );
        assert_ne!(
            PathPlanningMode::Download.key(relative).unwrap(),
            PathPlanningMode::Download.key(&absolute).unwrap()
        );
        let unsafe_path = Path::new("photos/../IMG.JPG");
        assert_eq!(
            PathPlanningMode::Reconciliation
                .key(unsafe_path)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::InvalidInput
        );
        assert_eq!(
            PathPlanningMode::Download.key(unsafe_path).unwrap(),
            NormalizedPath::new(unsafe_path)
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
        let tasks1 = filter_asset_to_tasks(
            &asset1,
            &config,
            &mut claimed_paths,
            &mut dir_cache,
            PathPlanningMode::Download,
        )
        .unwrap();
        assert_eq!(tasks1.len(), 2);
        let heic1_path = &tasks1[0].download_path;

        // Write asset1's HEIC to disk so asset2 sees a collision
        fs::create_dir_all(heic1_path.parent().unwrap()).unwrap();
        fs::write(heic1_path, vec![0u8; 2000]).unwrap();

        // Process asset2: same filename, different size → should dedup HEIC
        // Clear dir_cache since we just wrote a new file
        dir_cache.clear();
        let tasks2 = filter_asset_to_tasks(
            &asset2,
            &config,
            &mut claimed_paths,
            &mut dir_cache,
            PathPlanningMode::Download,
        )
        .unwrap();
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
        let tasks = filter_asset_to_tasks(
            &asset,
            &config,
            &mut claimed_paths,
            &mut dir_cache,
            PathPlanningMode::Download,
        )
        .unwrap();
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
        let tasks = filter_asset_to_tasks(
            &asset,
            &config,
            &mut claimed_paths,
            &mut dir_cache,
            PathPlanningMode::Download,
        )
        .unwrap();
        assert_eq!(
            tasks.len(),
            2,
            "same-size existing HEIC/MOV files are not safe skips without state identity"
        );
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
        let video_tasks = filter_asset_to_tasks(
            &video_asset,
            &config,
            &mut claimed_paths,
            &mut dir_cache,
            PathPlanningMode::Download,
        )
        .unwrap();
        assert_eq!(video_tasks.len(), 1);
        let video_path = &video_tasks[0].download_path;
        eprintln!("Video path: {:?}", video_path);

        let photo_tasks = filter_asset_to_tasks(
            &photo_asset,
            &config,
            &mut claimed_paths,
            &mut dir_cache,
            PathPlanningMode::Download,
        )
        .unwrap();
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

        let tasks_a = filter_asset_to_tasks(
            &asset_a,
            &config,
            &mut claimed_paths,
            &mut dir_cache,
            PathPlanningMode::Download,
        )
        .unwrap();
        assert_eq!(tasks_a.len(), 1, "first asset should resolve to one task");
        let path_a = tasks_a[0].download_path.clone();

        let tasks_b = filter_asset_to_tasks(
            &asset_b,
            &config,
            &mut claimed_paths,
            &mut dir_cache,
            PathPlanningMode::Download,
        )
        .unwrap();
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
        let second_tasks = filter_asset_to_tasks(
            &asset,
            &config,
            &mut claimed_paths,
            &mut dir_cache,
            PathPlanningMode::Download,
        )
        .unwrap();
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
        let tasks_a = filter_asset_to_tasks(
            &asset_a,
            &config,
            &mut claimed_paths,
            &mut dir_cache,
            PathPlanningMode::Download,
        )
        .unwrap();
        let tasks_b = filter_asset_to_tasks(
            &asset_b,
            &config,
            &mut claimed_paths,
            &mut dir_cache,
            PathPlanningMode::Download,
        )
        .unwrap();

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
                    filter_asset_to_tasks(
                        &asset,
                        &config,
                        &mut claimed_paths,
                        &mut dir_cache,
                        PathPlanningMode::Download,
                    )
                    .unwrap()
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
}
