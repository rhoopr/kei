//! Pass configuration and incremental routing eligibility.

use std::sync::Arc;

use anyhow::{Context, Result};
use futures_util::{StreamExt, stream};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::icloud::photos::PhotoAsset;

use super::config::DownloadConfig;
use super::models::{DownloadControls, FullEnumerationReason};

/// Pre-compute one `Arc<DownloadConfig>` per pass. Each pass_index maps to
/// a derived config that pre-expands `{album}` and pins the pass's
/// exclude-asset-ids set. In `{album}` mode passes may legitimately differ
/// per entry; outside of it, passes share identical excludes but the per-
/// pass wrapper is harmless and keeps call sites uniform.
pub(super) fn build_pass_configs(
    passes: &[crate::commands::AlbumPass],
    base: &DownloadConfig,
) -> Vec<Arc<DownloadConfig>> {
    passes
        .iter()
        .map(|pass| Arc::new(base.with_pass(pass)))
        .collect()
}

pub(super) fn build_pass_configs_with_download_concurrency(
    passes: &[crate::commands::AlbumPass],
    base: &DownloadConfig,
    per_pass_download_concurrency: usize,
) -> Vec<Arc<DownloadConfig>> {
    passes
        .iter()
        .map(|pass| {
            let mut config = base.with_pass(pass);
            config.concurrent_downloads = per_pass_download_concurrency.max(1);
            Arc::new(config)
        })
        .collect()
}

#[cfg(test)]
fn incremental_requires_full_enumeration(passes: &[crate::commands::AlbumPass]) -> bool {
    passes
        .iter()
        .any(|pass| pass.kind != crate::commands::PassKind::Unfiled)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum IncrementalRoutingDecision {
    Safe,
    TargetedAlbumBackfill {
        pass_indices: Vec<usize>,
        reason: FullEnumerationReason,
    },
    NeedsFull {
        reason: FullEnumerationReason,
    },
}

#[derive(Debug)]
pub(super) struct IncrementalPassRouting {
    pub(super) selected_album_passes: FxHashMap<String, Vec<usize>>,
    pub(super) selected_container_ids: Vec<String>,
    pub(super) unfiled_passes: Vec<usize>,
}

impl IncrementalPassRouting {
    pub(super) fn from_passes(passes: &[crate::commands::AlbumPass]) -> Self {
        let mut selected_album_passes: FxHashMap<String, Vec<usize>> = FxHashMap::default();
        let mut unfiled_passes = Vec::new();

        for (index, pass) in passes.iter().enumerate() {
            match pass.kind {
                crate::commands::PassKind::Album => {
                    if let Some(container_id) = pass.album.container_id() {
                        selected_album_passes
                            .entry(container_id.to_string())
                            .or_default()
                            .push(index);
                    }
                }
                crate::commands::PassKind::Unfiled => unfiled_passes.push(index),
                crate::commands::PassKind::SmartFolder => {}
            }
        }

        let mut selected_container_ids: Vec<String> =
            selected_album_passes.keys().cloned().collect();
        selected_container_ids.sort_unstable();

        Self {
            selected_album_passes,
            selected_container_ids,
            unfiled_passes,
        }
    }

    pub(super) fn has_selected_albums(&self) -> bool {
        !self.selected_album_passes.is_empty()
    }

    pub(super) fn selected_container_refs(&self) -> Vec<&str> {
        self.selected_container_ids
            .iter()
            .map(String::as_str)
            .collect()
    }

    pub(super) fn album_passes_for_container(&self, container_id: &str) -> Option<&[usize]> {
        self.selected_album_passes
            .get(container_id)
            .map(Vec::as_slice)
    }
}

pub(super) async fn determine_incremental_routing_decision(
    passes: &[crate::commands::AlbumPass],
    config: &DownloadConfig,
    controls: DownloadControls,
) -> IncrementalRoutingDecision {
    let has_unmapped_album_pass = passes.iter().any(|pass| {
        pass.kind == crate::commands::PassKind::Album && pass.album.container_id().is_none()
    });
    if has_unmapped_album_pass {
        return IncrementalRoutingDecision::NeedsFull {
            reason: FullEnumerationReason::AlbumRelationHydrationIncomplete,
        };
    }

    let routing = IncrementalPassRouting::from_passes(passes);
    if !routing.has_selected_albums() {
        return IncrementalRoutingDecision::Safe;
    }

    let Some(db) = &config.state_db else {
        return IncrementalRoutingDecision::NeedsFull {
            reason: FullEnumerationReason::AlbumRelationHydrationIncomplete,
        };
    };

    let mut backfill_pass_indices = Vec::new();
    for (index, pass) in passes.iter().enumerate() {
        if pass.kind != crate::commands::PassKind::Album {
            continue;
        }
        let Some(container_id) = pass.album.container_id() else {
            continue;
        };
        match db
            .selected_album_containers_have_complete_snapshots(&config.library, &[container_id])
            .await
        {
            Ok(true) => {}
            Ok(false) => backfill_pass_indices.push(index),
            Err(e) => {
                tracing::warn!(
                    container_id,
                    error = %e,
                    "Failed to verify album membership snapshot for incremental routing"
                );
                return IncrementalRoutingDecision::NeedsFull {
                    reason: FullEnumerationReason::OtherStaticReason,
                };
            }
        }
    }

    if backfill_pass_indices.is_empty() {
        IncrementalRoutingDecision::Safe
    } else if should_record_album_snapshots(passes, config, controls) {
        IncrementalRoutingDecision::TargetedAlbumBackfill {
            pass_indices: backfill_pass_indices,
            reason: FullEnumerationReason::AlbumRelationHydrationIncomplete,
        }
    } else {
        IncrementalRoutingDecision::NeedsFull {
            reason: FullEnumerationReason::AlbumRelationHydrationIncomplete,
        }
    }
}

pub(super) async fn route_incremental_asset_to_passes(
    asset: &PhotoAsset,
    routing: &IncrementalPassRouting,
    selected_container_ids: &[&str],
    config: &DownloadConfig,
) -> Result<Vec<usize>> {
    if !routing.has_selected_albums() {
        return Ok(routing.unfiled_passes.clone());
    }

    let db = config
        .state_db
        .as_ref()
        .context("Album-aware incremental routing requires a state database")?;
    let memberships = db
        .get_live_selected_album_memberships_for_asset(
            &config.library,
            asset.asset_record_name(),
            selected_container_ids,
        )
        .await
        .with_context(|| {
            format!(
                "Could not look up album memberships for asset {}",
                asset.asset_record_name()
            )
        })?;

    let mut pass_indices = FxHashSet::default();
    for membership in &memberships {
        if let Some(indices) = routing.album_passes_for_container(&membership.container_id) {
            pass_indices.extend(indices.iter().copied());
        }
    }
    if memberships.is_empty() {
        pass_indices.extend(routing.unfiled_passes.iter().copied());
    }

    let mut pass_indices: Vec<usize> = pass_indices.into_iter().collect();
    pass_indices.sort_unstable();
    Ok(pass_indices)
}

pub(super) fn split_incremental_and_smart_folder_passes(
    passes: &[crate::commands::AlbumPass],
) -> (
    Vec<crate::commands::AlbumPass>,
    Vec<crate::commands::AlbumPass>,
) {
    passes
        .iter()
        .cloned()
        .partition(|pass| pass.kind != crate::commands::PassKind::SmartFolder)
}

async fn collect_pass_asset_ids(pass: &crate::commands::AlbumPass) -> Result<FxHashSet<String>> {
    let count = pass
        .album
        .len()
        .await
        .with_context(|| format!("Could not count assets in album `{}`", pass.album.name))?;
    let (stream, _token_rx) = pass.album.photo_stream_with_token(None, Some(count), 1);
    tokio::pin!(stream);
    let mut ids = FxHashSet::default();
    while let Some(item) = stream.next().await {
        let asset = item?;
        ids.insert(asset.asset_record_name().to_string());
    }
    Ok(ids)
}

pub(in crate::download) async fn build_pass_configs_resolving_deferred_excludes(
    passes: &[crate::commands::AlbumPass],
    base: &DownloadConfig,
) -> Result<Vec<Arc<DownloadConfig>>> {
    let mut pass_configs = build_pass_configs(passes, base);
    let Some(unfiled_index) = deferred_unfiled_index(passes) else {
        return Ok(pass_configs);
    };

    let per_album: Vec<Result<FxHashSet<String>>> = stream::iter(
        passes
            .iter()
            .filter(|pass| pass.kind == crate::commands::PassKind::Album),
    )
    .map(collect_pass_asset_ids)
    .buffer_unordered(base.concurrent_downloads.max(1))
    .collect()
    .await;

    let mut exclude_ids = FxHashSet::default();
    for ids in per_album {
        exclude_ids.extend(ids?);
    }

    if let (Some(pass), Some(slot)) = (
        passes.get(unfiled_index),
        pass_configs.get_mut(unfiled_index),
    ) {
        let mut unfiled_config = base.with_pass(pass);
        unfiled_config.exclude_asset_ids = Arc::new(exclude_ids);
        *slot = Arc::new(unfiled_config);
    }
    Ok(pass_configs)
}

pub(super) fn should_record_album_snapshots(
    passes: &[crate::commands::AlbumPass],
    config: &DownloadConfig,
    controls: DownloadControls,
) -> bool {
    !controls.run_mode.is_dry_run()
        && !controls.run_mode.only_print_filenames()
        && config.state_db.is_some()
        && config.recent.is_none()
        && config.skip_created_before.is_none()
        && passes.iter().any(|pass| {
            pass.kind == crate::commands::PassKind::Album && pass.album.container_id().is_some()
        })
}

pub(super) fn deferred_unfiled_index(passes: &[crate::commands::AlbumPass]) -> Option<usize> {
    let has_album_pass = passes
        .iter()
        .any(|pass| pass.kind == crate::commands::PassKind::Album);
    if !has_album_pass {
        return None;
    }
    passes.iter().position(|pass| {
        pass.kind == crate::commands::PassKind::Unfiled && pass.exclude_ids.is_empty()
    })
}

#[cfg(test)]
mod tests;
