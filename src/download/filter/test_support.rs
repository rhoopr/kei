//! Shared filter test fixtures. Production owners do not depend on this module.

use std::sync::Arc;

use rustc_hash::FxHashMap;
use serde_json::json;

use crate::download::{DownloadConfig, paths};
use crate::icloud::photos::PhotoAsset;
use crate::test_helpers::TestPhotoAsset;
use crate::types::LivePhotoMode;

use super::collisions::PathPlanningMode;
use super::tasks::{DownloadTask, filter_asset_to_tasks};

pub(super) fn test_config() -> DownloadConfig {
    DownloadConfig::test_default()
}

/// Helper that calls filter_asset_to_tasks with a fresh claimed_paths map.
/// Use this for simple tests that don't need to track paths across calls.
pub(super) fn filter_asset_fresh(asset: &PhotoAsset, config: &DownloadConfig) -> Vec<DownloadTask> {
    let mut claimed_paths = FxHashMap::default();
    let mut dir_cache = paths::DirCache::new();
    filter_asset_to_tasks(
        asset,
        config,
        &mut claimed_paths,
        &mut dir_cache,
        PathPlanningMode::Download,
    )
    .unwrap()
}

pub(super) fn plain_photo_asset() -> PhotoAsset {
    TestPhotoAsset::new("TEST_1").build()
}

pub(super) fn video_asset() -> PhotoAsset {
    TestPhotoAsset::new("VID_1")
        .filename("movie.mov")
        .item_type("com.apple.quicktime-movie")
        .orig_file_type("com.apple.quicktime-movie")
        .orig_size(50_000)
        .orig_url("https://p01.icloud-content.com/vid")
        .orig_checksum("vid_ck")
        .build()
}

pub(super) fn skip_live_photos(config: &mut DownloadConfig) {
    config.live_photo_mode = LivePhotoMode::Skip;
}

pub(super) fn exclude_aae_filenames(config: &mut DownloadConfig) {
    config.filename_exclude = Arc::from(vec![glob::Pattern::new("*.AAE").unwrap()]);
}

pub(super) fn test_live_photo_asset() -> PhotoAsset {
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

pub(super) fn photo_asset_with_original_and_alternative(
    orig_type: &str,
    alt_type: &str,
) -> PhotoAsset {
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

// ── Medium/Thumb size suffix tests ──────────────────────────────────

pub(super) fn multi_size_photo_asset() -> PhotoAsset {
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
