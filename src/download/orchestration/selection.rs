//! Pass configuration and incremental routing eligibility.

use std::sync::Arc;

use anyhow::{Context, Result};
use futures_util::{StreamExt, stream};
use rustc_hash::{FxHashMap, FxHashSet};

use super::config::DownloadConfig;
use super::models::DownloadControls;

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

#[derive(Debug)]
pub(super) struct IncrementalPassRouting {
    pub(super) selected_album_passes: FxHashMap<String, Vec<usize>>,
}

impl IncrementalPassRouting {
    pub(super) fn from_passes(passes: &[crate::commands::AlbumPass]) -> Self {
        let mut selected_album_passes: FxHashMap<String, Vec<usize>> = FxHashMap::default();
        for (index, pass) in passes.iter().enumerate() {
            if pass.kind == crate::commands::PassKind::Album
                && let Some(container_id) = pass.album.container_id()
            {
                selected_album_passes
                    .entry(container_id.to_string())
                    .or_default()
                    .push(index);
            }
        }
        Self {
            selected_album_passes,
        }
    }

    pub(super) fn album_passes_for_container(&self, container_id: &str) -> Option<&[usize]> {
        self.selected_album_passes
            .get(container_id)
            .map(Vec::as_slice)
    }
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
    !config.provisional
        && !controls.run_mode.is_dry_run()
        && !controls.run_mode.only_print_filenames()
        && config.state_db.is_some()
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
