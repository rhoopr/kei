//! Asset eligibility and media classification. No path or filesystem decisions.

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
    None
}
