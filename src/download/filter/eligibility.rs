//! Asset eligibility and media classification. No path or filesystem decisions.

use crate::state::{MediaType, VersionSizeKey};
use crate::types::{AssetItemType, LivePhotoMode};

use super::config::PathDerivationSource;

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
pub(super) const GLOB_CASE_INSENSITIVE: glob::MatchOptions = glob::MatchOptions {
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
        tracing::warn!(target: "kei::download::filter", "Skipping malformed asset with empty CloudKit recordName");
        return Some(FilterReason::MalformedAsset);
    }
    if config
        .exclude_asset_ids()
        .contains(asset.asset_record_name())
        || config.exclude_asset_ids().contains(asset.id())
    {
        tracing::debug!(target: "kei::download::filter",
            asset_id = %asset.id(),
            asset_record_name = %asset.asset_record_name(),
            "Skipping (excluded album asset)"
        );
        return Some(FilterReason::ExcludedAlbum);
    }
    if asset.is_live_photo() && !config.media().live_photos {
        tracing::debug!(target: "kei::download::filter", asset_id = %asset.id(), "Skipping live photo (media filter)");
        return Some(FilterReason::MediaType);
    }
    if !config.media().videos && asset.item_type() == Some(AssetItemType::Movie) {
        tracing::debug!(target: "kei::download::filter", asset_id = %asset.id(), "Skipping video (media filter)");
        return Some(FilterReason::MediaType);
    }
    if !config.media().photos
        && asset.item_type() == Some(AssetItemType::Image)
        && !asset.is_live_photo()
    {
        tracing::debug!(target: "kei::download::filter", asset_id = %asset.id(), "Skipping photo (media filter)");
        return Some(FilterReason::MediaType);
    }
    if config.live_photo_mode() == LivePhotoMode::Skip && asset.is_live_photo() {
        tracing::debug!(target: "kei::download::filter", asset_id = %asset.id(), "Skipping live photo (live_photo_mode=skip)");
        return Some(FilterReason::LivePhoto);
    }
    let created_utc = asset.created();
    let created_local = asset.created_local();
    if let Some(before) = config.skip_created_before()
        && before.excludes_before(created_utc, created_local.date_naive())
    {
        tracing::debug!(target: "kei::download::filter", asset_id = %asset.id(), date = %created_local, "Skipping (before date range)");
        return Some(FilterReason::DateRange);
    }
    if let Some(after) = config.skip_created_after()
        && after.excludes_after(created_utc, created_local.date_naive())
    {
        tracing::debug!(target: "kei::download::filter", asset_id = %asset.id(), date = %created_local, "Skipping (after date range)");
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
        tracing::debug!(target: "kei::download::filter", asset_id = %asset.id(), filename, "Skipping (filename_exclude match)");
        return Some(FilterReason::Filename);
    }
    None
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::{DateTime, Local};
    use rustc_hash::FxHashSet;
    use serde_json::json;

    use crate::download::DownloadConfig;
    use crate::icloud::photos::PhotoAsset;
    use crate::state::{MediaType, VersionSizeKey};
    use crate::test_helpers::TestPhotoAsset;

    use super::super::expected_paths::expected_paths_for;
    use super::super::test_support::{
        exclude_aae_filenames, filter_asset_fresh, plain_photo_asset, skip_live_photos,
        test_config, test_live_photo_asset, video_asset,
    };
    use super::super::versions::extract_skip_candidates;
    use super::{FilterReason, determine_media_type, is_asset_filtered};

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

    struct FilterDecisionCase {
        name: &'static str,
        asset: fn() -> PhotoAsset,
        configure: fn(&mut DownloadConfig),
        expected: Option<FilterReason>,
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
}
