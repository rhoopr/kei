//! Borrowed version selection, RAW alignment, companions, and metadata version keys.

use std::sync::Arc;

use crate::icloud::photos::VersionsMap;
use crate::icloud::photos::types::AssetVersion;
use crate::state::VersionSizeKey;
use crate::types::{AssetItemType, AssetVersionSize, LivePhotoMode, RawPolicy};

use super::config::PathDerivationSource;

/// Borrowed view over a `VersionsMap` with an optional virtual swap of
/// the keys at two indices. Lets [`apply_raw_policy`] relabel the
/// `Original` / `Alternative` slots without cloning the version list.
#[derive(Debug, Clone, Copy)]
pub(in crate::download) struct VersionsView<'a> {
    versions: &'a VersionsMap,
    /// `(orig_idx, alt_idx)` when the keys at those indices should be
    /// presented swapped; iteration yields `Alternative` at `orig_idx`
    /// and `Original` at `alt_idx`. `None` means iterate as-is.
    swap: Option<(usize, usize)>,
}

impl<'a> VersionsView<'a> {
    fn borrowed(versions: &'a VersionsMap) -> Self {
        Self {
            versions,
            swap: None,
        }
    }

    fn swapped(versions: &'a VersionsMap, orig_idx: usize, alt_idx: usize) -> Self {
        Self {
            versions,
            swap: Some((orig_idx, alt_idx)),
        }
    }

    pub(in crate::download) fn iter(
        &self,
    ) -> impl Iterator<Item = (AssetVersionSize, &'a AssetVersion)> + 'a + use<'a> {
        let swap = self.swap;
        self.versions.iter().enumerate().map(move |(idx, (k, v))| {
            let key = match swap {
                Some((orig, _)) if idx == orig => AssetVersionSize::Alternative,
                Some((_, alt)) if idx == alt => AssetVersionSize::Original,
                _ => *k,
            };
            (key, v)
        })
    }

    pub(in crate::download) fn get(&self, key: AssetVersionSize) -> Option<&'a AssetVersion> {
        self.iter().find(|(k, _)| *k == key).map(|(_, v)| v)
    }
}

/// Apply the RAW alignment policy by virtually swapping Original and
/// Alternative versions when appropriate, matching Python's
/// `apply_raw_policy()`. Returns a borrowed view over the original map
/// regardless of swap outcome.
#[allow(
    clippy::indexing_slicing,
    reason = "orig_idx / alt_idx come from `enumerate()` over `versions`; \
              indexing back into `versions` is in-bounds by construction"
)]
pub(super) fn apply_raw_policy(versions: &VersionsMap, policy: RawPolicy) -> VersionsView<'_> {
    if policy == RawPolicy::AsIs {
        return VersionsView::borrowed(versions);
    }

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
        return VersionsView::borrowed(versions);
    };

    let should_swap = match policy {
        RawPolicy::PreferRaw => versions[alt_idx].1.asset_type.contains("raw"),
        RawPolicy::PreferJpeg => {
            orig_idx.is_some_and(|idx| versions[idx].1.asset_type.contains("raw"))
        }
        RawPolicy::AsIs => false,
    };

    match (should_swap, orig_idx) {
        (true, Some(orig_idx)) => VersionsView::swapped(versions, orig_idx, alt_idx),
        _ => VersionsView::borrowed(versions),
    }
}

/// Task keys follow RAW policy; provider metadata keys never do.
pub(crate) fn metadata_for_selected_version(
    asset: &crate::icloud::photos::PhotoAsset,
    config: &(impl PathDerivationSource + ?Sized),
    version: VersionSizeKey,
) -> Arc<crate::state::AssetMetadata> {
    let provider_version = if apply_raw_policy(asset.versions(), config.raw_policy())
        .swap
        .is_some()
    {
        match version {
            VersionSizeKey::Original => VersionSizeKey::Alternative,
            VersionSizeKey::Alternative => VersionSizeKey::Original,
            _ => version,
        }
    } else {
        version
    };
    asset.metadata_arc(provider_version)
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
    if let Some((version, effective_size)) = select_edited_extra(config, &versions, &seen_urls) {
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
    let requested = config.resolution().to_asset_version_size()?;
    let get_version = |key: &AssetVersionSize| versions.get(*key);
    version_with_fallback(
        &get_version,
        requested,
        AssetVersionSize::Original,
        config.force_resolution(),
    )
}

pub(super) fn select_edited_extra<'a>(
    config: &(impl PathDerivationSource + ?Sized),
    versions: &VersionsView<'a>,
    seen_urls: &[&str],
) -> Option<(&'a AssetVersion, AssetVersionSize)> {
    if !config.edited() {
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
    if !config.edited() || !asset.is_live_photo() || asset.item_type() != Some(AssetItemType::Image)
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
    if !config.alternative() || asset.is_live_photo() {
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
    if asset.item_type() != Some(AssetItemType::Image) {
        return None;
    }
    let get_version = |key: &AssetVersionSize| versions.get(*key);
    let selected = version_with_fallback(
        &get_version,
        config.live_resolution(),
        AssetVersionSize::LiveOriginal,
        config.force_resolution(),
    )?;
    (!url_seen(selected.0, seen_urls)).then_some(selected)
}

/// Look up a version by key, falling back to `fallback_key` when the requested
/// size is unavailable (unless `force_resolution` is set). Shared by both
/// `extract_skip_candidates` and `filter_asset_to_tasks`.
fn version_with_fallback<'a>(
    get_version: &dyn Fn(&AssetVersionSize) -> Option<&'a AssetVersion>,
    requested: AssetVersionSize,
    fallback: AssetVersionSize,
    force_resolution: bool,
) -> Option<(&'a AssetVersion, AssetVersionSize)> {
    match get_version(&requested) {
        Some(v) => Some((v, requested)),
        None if requested != fallback && !force_resolution => {
            get_version(&fallback).map(|v| (v, fallback))
        }
        _ => None,
    }
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
        exclude_aae_filenames, photo_asset_with_original_and_alternative, plain_photo_asset,
        skip_live_photos, test_config, test_live_photo_asset,
    };
    use super::{VersionsView, apply_raw_policy, extract_skip_candidates, metadata_capture};

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
    fn test_raw_policy_prefer_raw_swaps_when_alt_is_raw() {
        let asset = photo_asset_with_original_and_alternative("public.jpeg", "com.adobe.raw-image");
        let versions = apply_raw_policy(asset.versions(), RawPolicy::PreferRaw);
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
    fn test_raw_policy_prefer_jpeg_swaps_when_orig_is_raw() {
        let asset = photo_asset_with_original_and_alternative("com.adobe.raw-image", "public.jpeg");
        let versions = apply_raw_policy(asset.versions(), RawPolicy::PreferJpeg);
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

    /// On a swap, `iter()` must preserve the underlying `VersionsMap`
    /// element order — only the keys at the two swap slots flip. If a
    /// future refactor reorders elements (e.g. surfacing `Original`
    /// first regardless of position), this fails loudly.
    #[test]
    fn raw_policy_view_iter_order_matches_underlying_map() {
        let asset = photo_asset_with_original_and_alternative("public.jpeg", "com.adobe.raw-image");
        let view = apply_raw_policy(asset.versions(), RawPolicy::PreferRaw);

        let elements: Vec<(AssetVersionSize, &str)> =
            view.iter().map(|(k, v)| (k, v.url.as_ref())).collect();
        assert_eq!(elements.len(), 2);
        // Slot 0 (originally Original) reads as Alternative, still
        // pointing at orig_url.
        assert_eq!(elements[0].0, AssetVersionSize::Alternative);
        assert_eq!(elements[0].1, "https://p01.icloud-content.com/orig");
        // Slot 1 (originally Alternative) reads as Original, still
        // pointing at alt_url.
        assert_eq!(elements[1].0, AssetVersionSize::Original);
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
                name: "missing medium resolution falls back to original",
                asset: plain_photo_asset,
                configure: medium_resolution_with_fallback,
                expected: &[(VersionSizeKey::Original, "abc123")],
            },
            SkipCandidateCase {
                name: "force medium resolution prevents fallback",
                asset: plain_photo_asset,
                configure: medium_resolution_without_fallback,
                expected: &[],
            },
            SkipCandidateCase {
                name: "filename exclude no-match still emits original",
                asset: plain_photo_asset,
                configure: exclude_aae_filenames,
                expected: &[(VersionSizeKey::Original, "abc123")],
            },
            SkipCandidateCase {
                name: "missing live adjusted falls back to live original",
                asset: test_live_photo_asset,
                configure: live_adjusted_with_fallback,
                expected: &[
                    (VersionSizeKey::Original, "heic_ck"),
                    (VersionSizeKey::LiveOriginal, "mov_ck"),
                ],
            },
            SkipCandidateCase {
                name: "force live adjusted prevents mov fallback",
                asset: test_live_photo_asset,
                configure: live_adjusted_without_fallback,
                expected: &[(VersionSizeKey::Original, "heic_ck")],
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
