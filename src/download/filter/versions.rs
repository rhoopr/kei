//! Borrowed rendition selection and historical resource identity recovery.

use crate::icloud::photos::VersionsMap;
use crate::icloud::photos::types::AssetVersion;
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
