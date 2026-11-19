//! Borrowed rendition selection and historical resource identity recovery.

use std::sync::Arc;

use crate::icloud::photos::VersionsMap;
use crate::icloud::photos::types::AssetVersion;
use crate::state::VersionSizeKey;
use crate::types::{AssetItemType, AssetVersionSize, LivePhotoMode, RawPolicy};

use super::config::PathDerivationSource;

/// Borrowed provider renditions with their original keys intact.
#[derive(Debug, Clone, Copy)]
pub(in crate::download) struct VersionsView<'a> {
    versions: &'a VersionsMap,
}

impl<'a> VersionsView<'a> {
    fn borrowed(versions: &'a VersionsMap) -> Self {
        Self { versions }
    }

    pub(in crate::download) fn iter(
        &self,
    ) -> impl Iterator<Item = (AssetVersionSize, &'a AssetVersion)> + 'a + use<'a> {
        self.versions.iter().map(|(key, version)| (*key, version))
    }

    pub(in crate::download) fn get(&self, key: AssetVersionSize) -> Option<&'a AssetVersion> {
        self.iter().find(|(k, _)| *k == key).map(|(_, v)| v)
    }
}

/// Legacy in-memory preferences cannot relabel newly selected resources.
pub(super) fn apply_raw_policy(versions: &VersionsMap, _policy: RawPolicy) -> VersionsView<'_> {
    VersionsView::borrowed(versions)
}

/// Newly selected task keys are provider metadata keys.
pub(crate) fn metadata_for_selected_version(
    asset: &crate::icloud::photos::PhotoAsset,
    config: &(impl PathDerivationSource + ?Sized),
    version: VersionSizeKey,
) -> Arc<crate::state::AssetMetadata> {
    asset.metadata_arc(provider_version_for_selected(asset, config, version))
}

/// Newly selected renditions retain provider identity.
pub(crate) fn provider_version_for_selected(
    _asset: &crate::icloud::photos::PhotoAsset,
    _config: &(impl PathDerivationSource + ?Sized),
    version: VersionSizeKey,
) -> VersionSizeKey {
    version
}

/// Recover a frozen task's provider key without replaying today's RAW policy.
/// Only original/alternative keys could historically be relabeled. Ambiguous
/// resource evidence cannot authorize refreshing either sibling's URL.
#[must_use]
pub(crate) fn provider_version_for_frozen_resource(
    asset: &crate::icloud::photos::PhotoAsset,
    version: VersionSizeKey,
    checksum: &str,
    size: u64,
) -> Option<VersionSizeKey> {
    let mut matches = asset.versions().iter().filter_map(|(key, resource)| {
        let key = VersionSizeKey::from(*key);
        let eligible = if matches!(
            version,
            VersionSizeKey::Original | VersionSizeKey::Alternative
        ) {
            matches!(key, VersionSizeKey::Original | VersionSizeKey::Alternative)
        } else {
            key == version
        };
        (eligible && resource.checksum.as_ref() == checksum && resource.size == size).then_some(key)
    });
    let selected = matches.next()?;
    matches.next().is_none().then_some(selected)
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

/// Lightweight pre-check: extract (`version_size`, checksum) pairs for an asset
/// after applying content/date filters but WITHOUT path resolution or disk I/O.
///
/// Returns the candidate versions that would be downloaded. Used by the early
/// skip gate to check the state DB before the expensive `filter_asset_to_tasks`.
/// Caller must check [`crate::download::filter::is_asset_filtered`] first.
pub(in crate::download) fn extract_skip_candidates<'a>(
    asset: &'a crate::icloud::photos::PhotoAsset,
    config: &(impl PathDerivationSource + ?Sized),
) -> Vec<(VersionSizeKey, &'a str)> {
    if !asset.has_valid_id() {
        return Vec::new();
    }

    let versions = apply_raw_policy(asset.versions(), config.raw_policy());
    let mut result = Vec::with_capacity(5);
    let mut seen_urls = Vec::<&str>::with_capacity(4);

    if let Some((version, effective_size)) = select_primary(asset, config, &versions) {
        seen_urls.push(version.url.as_ref());
        result.push((
            VersionSizeKey::from(effective_size),
            version.checksum.as_ref(),
        ));
    }
    if let Some((version, effective_size)) =
        select_edited_extra(asset, config, &versions, &seen_urls)
    {
        seen_urls.push(version.url.as_ref());
        result.push((
            VersionSizeKey::from(effective_size),
            version.checksum.as_ref(),
        ));
    }
    if let Some((version, effective_size)) =
        select_alternative_extra(asset, config, &versions, &seen_urls)
    {
        seen_urls.push(version.url.as_ref());
        result.push((
            VersionSizeKey::from(effective_size),
            version.checksum.as_ref(),
        ));
    }
    if let Some((version, effective_size)) =
        select_live_edited_extra(asset, config, &versions, &seen_urls)
    {
        seen_urls.push(version.url.as_ref());
        result.push((
            VersionSizeKey::from(effective_size),
            version.checksum.as_ref(),
        ));
    }
    if let Some((version, effective_size)) =
        select_mov_companion(asset, config, &versions, &seen_urls)
    {
        result.push((
            VersionSizeKey::from(effective_size),
            version.checksum.as_ref(),
        ));
    }

    result
}

fn url_seen(version: &AssetVersion, seen_urls: &[&str]) -> bool {
    seen_urls.iter().any(|seen| *seen == version.url.as_ref())
}

pub(super) fn select_primary<'a>(
    asset: &crate::icloud::photos::PhotoAsset,
    config: &(impl PathDerivationSource + ?Sized),
    versions: &VersionsView<'a>,
) -> Option<(&'a AssetVersion, AssetVersionSize)> {
    if matches!(
        config.live_photo_mode(),
        LivePhotoMode::Skip | LivePhotoMode::VideoOnly
    ) && asset.is_live_photo()
    {
        return None;
    }
    let key = AssetVersionSize::Original;
    versions.get(key).map(|version| (version, key))
}

pub(super) fn select_edited_extra<'a>(
    asset: &crate::icloud::photos::PhotoAsset,
    config: &(impl PathDerivationSource + ?Sized),
    versions: &VersionsView<'a>,
    seen_urls: &[&str],
) -> Option<(&'a AssetVersion, AssetVersionSize)> {
    if asset.is_live_photo()
        && matches!(
            config.live_photo_mode(),
            LivePhotoMode::Skip | LivePhotoMode::VideoOnly
        )
    {
        return None;
    }
    let key = AssetVersionSize::Adjusted;
    let version = versions.get(key)?;
    (!url_seen(version, seen_urls)).then_some((version, key))
}

pub(super) fn select_live_edited_extra<'a>(
    asset: &crate::icloud::photos::PhotoAsset,
    config: &(impl PathDerivationSource + ?Sized),
    versions: &VersionsView<'a>,
    seen_urls: &[&str],
) -> Option<(&'a AssetVersion, AssetVersionSize)> {
    if !matches!(
        config.live_photo_mode(),
        LivePhotoMode::Both | LivePhotoMode::VideoOnly
    ) || !asset.is_live_photo()
        || asset.item_type() != Some(AssetItemType::Image)
    {
        return None;
    }
    let key = AssetVersionSize::LiveAdjusted;
    let version = versions.get(key)?;
    (!url_seen(version, seen_urls)).then_some((version, key))
}

pub(super) fn select_alternative_extra<'a>(
    asset: &crate::icloud::photos::PhotoAsset,
    config: &(impl PathDerivationSource + ?Sized),
    versions: &VersionsView<'a>,
    seen_urls: &[&str],
) -> Option<(&'a AssetVersion, AssetVersionSize)> {
    if asset.is_live_photo()
        && matches!(
            config.live_photo_mode(),
            LivePhotoMode::Skip | LivePhotoMode::VideoOnly
        )
    {
        return None;
    }
    let version = versions.get(AssetVersionSize::Alternative)?;
    (!url_seen(version, seen_urls)).then_some((version, AssetVersionSize::Alternative))
}

pub(super) fn select_mov_companion<'a>(
    asset: &crate::icloud::photos::PhotoAsset,
    config: &(impl PathDerivationSource + ?Sized),
    versions: &VersionsView<'a>,
    seen_urls: &[&str],
) -> Option<(&'a AssetVersion, AssetVersionSize)> {
    if !matches!(
        config.live_photo_mode(),
        LivePhotoMode::Both | LivePhotoMode::VideoOnly
    ) {
        return None;
    }
    if !asset.is_live_photo() || asset.item_type() != Some(AssetItemType::Image) {
        return None;
    }
    let key = AssetVersionSize::LiveOriginal;
    let version = versions.get(key)?;
    (!url_seen(version, seen_urls)).then_some((version, key))
}

#[cfg(test)]
mod tests {
    use crate::download::DownloadConfig;
    use crate::icloud::photos::PhotoAsset;
    use crate::icloud::photos::types::AssetVersion;
    use crate::state::VersionSizeKey;
    use crate::test_helpers::TestPhotoAsset;
    use crate::types::{AssetVersionSize, LivePhotoMode, RawPolicy};

    use super::super::test_support::{
        photo_asset_with_original_and_alternative, plain_photo_asset, skip_live_photos,
        test_config, test_live_photo_asset,
    };
    use super::{VersionsView, apply_raw_policy, extract_skip_candidates, metadata_capture};

    #[test]
    fn frozen_raw_retry_requires_unique_checksum_and_size_without_policy() {
        for (alt_checksum, alt_size, expected) in [
            ("raw", 2000, Some(VersionSizeKey::Alternative)),
            ("changed", 2000, None),
            ("raw", 2001, None),
        ] {
            let asset = TestPhotoAsset::new("FROZEN_RAW")
                .orig_checksum("jpeg")
                .orig_size(1000)
                .alt_version(
                    "https://p01.icloud-content.com/raw",
                    alt_checksum,
                    alt_size,
                    "com.adobe.raw-image",
                )
                .build();
            assert_eq!(
                super::provider_version_for_frozen_resource(
                    &asset,
                    VersionSizeKey::Original,
                    "raw",
                    2000
                ),
                expected
            );
            assert_eq!(
                super::provider_version_for_frozen_resource(
                    &asset,
                    VersionSizeKey::Alternative,
                    "jpeg",
                    1000
                ),
                Some(VersionSizeKey::Original)
            );
            assert_eq!(
                super::provider_version_for_frozen_resource(
                    &asset,
                    VersionSizeKey::Adjusted,
                    "jpeg",
                    1000
                ),
                None
            );
        }
        let ambiguous = TestPhotoAsset::new("AMBIGUOUS_RAW")
            .orig_checksum("shared")
            .orig_size(1000)
            .alt_version(
                "https://p01.icloud-content.com/raw",
                "shared",
                1000,
                "com.adobe.raw-image",
            )
            .build();
        for key in [VersionSizeKey::Original, VersionSizeKey::Alternative] {
            assert_eq!(
                super::provider_version_for_frozen_resource(&ambiguous, key, "shared", 1000),
                None
            );
        }
    }

    // ── extract_skip_candidates tests ──────────────────────────────

    struct SkipCandidateCase {
        name: &'static str,
        asset: fn() -> PhotoAsset,
        configure: fn(&mut DownloadConfig),
        expected: &'static [(VersionSizeKey, &'static str)],
    }

    /// Helper to check whether a version exists in a `VersionsView`.
    fn has_ver(view: &VersionsView<'_>, key: AssetVersionSize) -> bool {
        view.iter().any(|(k, _)| k == key)
    }

    /// Helper to get a version from a `VersionsView` by key.
    fn get_ver<'a>(view: &VersionsView<'a>, key: AssetVersionSize) -> Option<&'a AssetVersion> {
        view.get(key)
    }

    fn live_adjusted_without_fallback(config: &mut DownloadConfig) {
        config.live_resolution = AssetVersionSize::LiveAdjusted;
        config.force_resolution = true;
    }

    fn live_adjusted_with_fallback(config: &mut DownloadConfig) {
        config.live_resolution = AssetVersionSize::LiveAdjusted;
        config.force_resolution = false;
    }

    fn medium_resolution_without_fallback(config: &mut DownloadConfig) {
        config.resolution = crate::types::PhotoResolution::Medium;
        config.force_resolution = true;
    }

    fn medium_resolution_with_fallback(config: &mut DownloadConfig) {
        config.resolution = crate::types::PhotoResolution::Medium;
        config.force_resolution = false;
    }

    fn live_photo_video_only(config: &mut DownloadConfig) {
        config.live_photo_mode = LivePhotoMode::VideoOnly;
    }

    fn live_photo_image_only(config: &mut DownloadConfig) {
        config.live_photo_mode = LivePhotoMode::ImageOnly;
    }

    fn no_config_change(_: &mut DownloadConfig) {}

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
    fn retired_raw_preference_keeps_provider_keys() {
        let asset = photo_asset_with_original_and_alternative("public.jpeg", "com.adobe.raw-image");
        let versions = apply_raw_policy(asset.versions(), RawPolicy::PreferRaw);
        // Original retains its provider identity even when the sibling is RAW.
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
    fn retired_jpeg_preference_keeps_provider_keys() {
        let asset = photo_asset_with_original_and_alternative("com.adobe.raw-image", "public.jpeg");
        let versions = apply_raw_policy(asset.versions(), RawPolicy::PreferJpeg);
        // A RAW original remains the original.
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

    /// Iteration preserves both provider keys and provider order.
    #[test]
    fn raw_policy_view_iter_order_matches_underlying_map() {
        let asset = photo_asset_with_original_and_alternative("public.jpeg", "com.adobe.raw-image");
        let view = apply_raw_policy(asset.versions(), RawPolicy::PreferRaw);

        let elements: Vec<(AssetVersionSize, &str)> =
            view.iter().map(|(k, v)| (k, v.url.as_ref())).collect();
        assert_eq!(elements.len(), 2);
        assert_eq!(elements[0].0, AssetVersionSize::Original);
        assert_eq!(elements[0].1, "https://p01.icloud-content.com/orig");
        assert_eq!(elements[1].0, AssetVersionSize::Alternative);
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
                name: "legacy medium preference still selects original",
                asset: plain_photo_asset,
                configure: medium_resolution_with_fallback,
                expected: &[(VersionSizeKey::Original, "abc123")],
            },
            SkipCandidateCase {
                name: "legacy forced medium preference still selects original",
                asset: plain_photo_asset,
                configure: medium_resolution_without_fallback,
                expected: &[(VersionSizeKey::Original, "abc123")],
            },
            SkipCandidateCase {
                name: "ordinary filename emits original",
                asset: plain_photo_asset,
                configure: |_| {},
                expected: &[(VersionSizeKey::Original, "abc123")],
            },
            SkipCandidateCase {
                name: "missing live edit keeps original",
                asset: test_live_photo_asset,
                configure: live_adjusted_with_fallback,
                expected: &[
                    (VersionSizeKey::Original, "heic_ck"),
                    (VersionSizeKey::LiveOriginal, "mov_ck"),
                ],
            },
            SkipCandidateCase {
                name: "legacy forced live edit keeps original",
                asset: test_live_photo_asset,
                configure: live_adjusted_without_fallback,
                expected: &[
                    (VersionSizeKey::Original, "heic_ck"),
                    (VersionSizeKey::LiveOriginal, "mov_ck"),
                ],
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
}
