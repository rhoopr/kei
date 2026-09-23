//! Download task construction from derived paths, metadata, and collision results.

use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, FixedOffset, Local};
use rustc_hash::FxHashMap;

use crate::download::file::FinalPublication;
use crate::download::{DownloadConfig, paths};
use crate::state::{MediaType, VersionSizeKey};
use crate::types::FileMatchPolicy;

use super::collisions::{
    CollisionStrategy, NormalizedPath, PathPlanningMode, ResolveContext,
    identity_collision_filename, resolve_download_path, size_or_identity_collision_filename,
};
use super::eligibility::{GLOB_CASE_INSENSITIVE, determine_media_type};
use super::expected_paths::{DerivedPath, raw_filename};
use super::metadata::{MetadataPayload, build_payload};
use super::{
    DerivationContext, derive_alternative_extra, derive_edited_extra, derive_live_edited_extra,
    derive_mov_companion, derive_primary,
};

/// A unit of work produced by the filter phase and consumed by the download phase.
///
/// Fields ordered for optimal memory layout:
/// - Heap types first (`Box<str>`, `PathBuf`, `MetadataPayload`)
/// - 8-byte primitives (u64)
/// - `DateTime` (12-16 bytes)
/// - 1-byte enum last
#[derive(Debug, Clone)]
pub(in crate::download) struct DownloadTask {
    // Heap types first
    pub(in crate::download) url: Box<str>,
    pub(in crate::download) download_path: PathBuf,
    pub(in crate::download) publication: FinalPublication,
    pub(in crate::download) checksum: Box<str>,
    /// iCloud asset ID for state tracking. Shared with the producer's
    /// dedup set and any deferred state writes via refcount bump.
    pub(in crate::download) asset_id: Arc<str>,
    /// Stable `CPLAsset.recordName` used to recover this task's selected state
    /// identity when cleanup re-enumeration returns a different sibling order.
    pub(in crate::download) asset_record_name: Arc<str>,
    /// CloudKit zone that owns this asset. Usually matches the pass config's
    /// library, but cross-zone album hydration can produce bounded assets
    /// from another zone while preserving the album pass context.
    pub(in crate::download) library: Arc<str>,
    /// Metadata fields surfaced from `AssetMetadata` for writer consumption.
    /// Behind `Arc` so `task.metadata.clone()` in the download hot path is a
    /// refcount bump instead of a deep clone of every `Vec<String>` inside.
    #[cfg_attr(not(feature = "xmp"), allow(dead_code))]
    pub(in crate::download) metadata: Arc<MetadataPayload>,
    // 8-byte primitives
    pub(in crate::download) size: u64,
    // DateTime
    pub(in crate::download) created_local: DateTime<FixedOffset>,
    // 1-byte enum
    /// Version size key for state tracking.
    pub(in crate::download) version_size: VersionSizeKey,
    /// Resolved media type at task-creation time. Carried on the task so
    /// the post-success site can split the run's downloaded count by
    /// photos vs videos without re-running `determine_media_type` (and
    /// without holding the heavier `PhotoAsset` reference past the filter
    /// stage).
    pub(in crate::download) media_type: MediaType,
}

impl DownloadTask {
    /// Project the task fields the recap renderer needs (basename of the
    /// download path, byte size, capture timestamp). Lives here because
    /// the path-to-filename and `created_local` source are private to
    /// this struct; keeps the success-arm call site a one-liner.
    pub(in crate::download) fn to_recap_asset(&self) -> crate::download::recap::RecapAsset {
        crate::download::recap::RecapAsset {
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

/// Apply content filters (type, date range) and local existence check,
/// producing download tasks for assets that need fetching.
/// Returns up to two tasks: the primary photo/video and an optional live photo MOV.
///
/// The `claimed_paths` map tracks paths that have been claimed by earlier tasks
/// in the same download session, preventing race conditions where two assets
/// with the same filename both see "file doesn't exist" during concurrent downloads.
/// Caller must check [`crate::download::filter::is_asset_filtered`] first.
pub(in crate::download) fn filter_asset_to_tasks(
    asset: &crate::icloud::photos::PhotoAsset,
    config: &DownloadConfig,
    claimed_paths: &mut FxHashMap<NormalizedPath, u64>,
    dir_cache: &mut paths::DirCache,
    planning_mode: PathPlanningMode,
) -> std::io::Result<Vec<DownloadTask>> {
    if !asset.has_valid_id() {
        return Ok(Vec::new());
    }

    // Sync-only fingerprint-fallback exclusion: when `asset.filename()` is
    // None, log the synthesized name and apply `filename_exclude` patterns
    // to it (`is_asset_filtered` only sees real filenames). Import never
    // populates `filename_exclude`.
    if asset.filename().is_none() {
        let fp = raw_filename(asset);
        tracing::info!(target: "kei::download::filter",
            asset_id = %asset.id(),
            filename = %fp,
            "Using fingerprint fallback filename"
        );
        if config
            .filename_exclude
            .iter()
            .any(|p| p.matches_with(&fp, GLOB_CASE_INSENSITIVE))
        {
            tracing::debug!(target: "kei::download::filter",
                asset_id = %asset.id(),
                filename = %fp,
                "Skipping (filename_exclude match on fallback)"
            );
            return Ok(Vec::new());
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
                planning_mode,
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
            )?
        };

        if let Some(stem) = primary_resolution
            .effective_path()
            .file_name()
            .and_then(|f| f.to_str())
        {
            effective_primary_filename = Some(stem.to_string());
        }
        if let Some(p) = primary_resolution.download_path() {
            claimed_paths.insert(planning_mode.key(&p)?, size);
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
                planning_mode,
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
            )?
            .download_path()
        };

        if let Some(p) = final_path {
            claimed_paths.insert(planning_mode.key(&p)?, size);
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
            return Ok(tasks);
        }
        let asset_id = asset.state_id();
        let final_mov_path = {
            let mut rctx = ResolveContext {
                config,
                planning_mode,
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
            )?
            .download_path()
        };

        if let Some(p) = final_mov_path {
            claimed_paths.insert(planning_mode.key(&p)?, size);
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

    Ok(tasks)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use rustc_hash::FxHashMap;
    use serde_json::json;
    use tempfile::TempDir;

    use crate::download::paths;
    use crate::icloud::photos::PhotoAsset;
    use crate::state::VersionSizeKey;
    use crate::test_helpers::TestPhotoAsset;
    use crate::types::{AssetVersionSize, FileMatchPolicy, LivePhotoMode, RawPolicy};

    use super::super::collisions::PathPlanningMode;
    use super::super::expected_paths::expected_paths_for;
    use super::super::test_support::{
        filter_asset_fresh, multi_size_photo_asset, photo_asset_with_original_and_alternative,
        test_config, test_live_photo_asset,
    };
    use super::{DownloadTask, filter_asset_to_tasks};

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

    // ── keep_unicode_in_filenames tests ─────────────────────────────────

    fn unicode_photo_asset() -> PhotoAsset {
        TestPhotoAsset::new("UNI_1")
            .filename("Caf\u{e9}_photo.jpg")
            .build()
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
        let mut ctx = crate::download::DownloadContext::default();
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
}
