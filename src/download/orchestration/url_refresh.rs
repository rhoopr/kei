//! Exact-task retry matching and stale download URL hydration.

use crate::icloud::photos::{
    ProviderRecordId, RecordLookupRequest, RecordResolution, RecordResolutionBatch,
};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use rustc_hash::{FxHashMap, FxHashSet};
use tokio_util::sync::CancellationToken;

use crate::download::filter::DownloadTask;
use crate::download::pipeline::PassResult;
use crate::download::planner;
use crate::state::VersionSizeKey;

use super::config::DownloadConfig;
use super::selection::build_pass_configs_resolving_deferred_excludes;

pub(super) const INCREMENTAL_PREFLIGHT_URL_REFRESH_AFTER: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(in crate::download) struct RetryTaskKey {
    pub(in crate::download) asset_id: Arc<str>,
    pub(in crate::download) version_size: VersionSizeKey,
    pub(in crate::download) download_path: std::path::PathBuf,
}

#[derive(Debug, Clone)]
pub(in crate::download) struct UrlRetrySource {
    pub(in crate::download) asset_record_name: Arc<str>,
    pub(in crate::download) pass_index: usize,
}

impl From<&DownloadTask> for RetryTaskKey {
    fn from(task: &DownloadTask) -> Self {
        Self {
            asset_id: Arc::clone(&task.asset_id),
            version_size: task.version_size,
            download_path: task.download_path.clone(),
        }
    }
}

fn retry_state_ids_by_asset_record(tasks: &[DownloadTask]) -> FxHashMap<Arc<str>, Arc<str>> {
    tasks
        .iter()
        .map(|task| {
            (
                Arc::clone(&task.asset_record_name),
                Arc::clone(&task.asset_id),
            )
        })
        .collect()
}

fn retry_hydrator_pass_index(
    passes: &[crate::commands::AlbumPass],
    pass_indices: &FxHashSet<usize>,
) -> Option<usize> {
    pass_indices
        .iter()
        .copied()
        .filter(|pass_index| {
            passes
                .get(*pass_index)
                .is_some_and(|pass| pass.kind != crate::commands::PassKind::Unfiled)
        })
        .min()
        .or_else(|| (!passes.is_empty()).then_some(0))
}

fn take_matching_retry_tasks<I>(
    tasks: I,
    pending_keys: &mut FxHashSet<RetryTaskKey>,
    out: &mut Vec<DownloadTask>,
) where
    I: IntoIterator<Item = DownloadTask>,
{
    for task in tasks {
        let key = RetryTaskKey::from(&task);
        if pending_keys.remove(&key) {
            out.push(task);
            if pending_keys.is_empty() {
                break;
            }
        }
    }
}

#[derive(Clone, Copy)]
pub(in crate::download) enum CleanupUrlRefresh {
    Enumerate,
    Lookup,
}

#[derive(Default)]
pub(in crate::download) struct CleanupRetryPlan {
    pub(in crate::download) tasks: Vec<DownloadTask>,
    pub(in crate::download) provider_auth_errors: usize,
    pub(in crate::download) rate_limit_observations: usize,
}

impl CleanupRetryPlan {
    fn observe_lookup(&mut self, batch: &RecordResolutionBatch) {
        self.rate_limit_observations = self
            .rate_limit_observations
            .saturating_add(batch.rate_limit_observations);
        let mut failed_records = 0usize;
        let mut authentication_failures = 0usize;
        for (_, resolution) in &batch.results {
            if let RecordResolution::TransientFailure(error) = resolution {
                failed_records += 1;
                authentication_failures += usize::from(error.is_authentication());
            }
        }
        self.provider_auth_errors += authentication_failures;
        if failed_records > 0 {
            tracing::warn!(
                diagnostic = "expired_url_refresh_failed",
                failed_records,
                authentication_failures,
                rate_limit_observations = batch.rate_limit_observations,
                "Provider lookup failed while refreshing expired download URLs"
            );
        }
    }
}

/// Refresh only the URL on already-selected tasks. Their original destination,
/// rendition, metadata and publication authorization retain the first pass's
/// selection evidence, including deferred unfiled exclusions.
async fn refresh_failed_download_urls(
    passes: &[crate::commands::AlbumPass],
    failed_tasks: &[DownloadTask],
    shutdown_token: &CancellationToken,
) -> CleanupRetryPlan {
    let mut retry = CleanupRetryPlan::default();
    let mut pending_keys: FxHashSet<_> = failed_tasks.iter().map(RetryTaskKey::from).collect();
    let requested_count = pending_keys.len();
    let mut refreshed_zones = FxHashSet::default();
    for pass in passes {
        if shutdown_token.is_cancelled() || pending_keys.is_empty() {
            break;
        }
        let zone = pass.album.zone_name();
        if !refreshed_zones.insert(zone) {
            continue;
        }
        let requests: Vec<_> = failed_tasks
            .iter()
            .filter(|task| task.library.as_ref() == zone)
            .map(|task| task.asset_record_name.as_ref())
            .collect::<FxHashSet<_>>()
            .into_iter()
            .map(|name| RecordLookupRequest::asset_only(ProviderRecordId::new(name)))
            .collect();
        let resolutions = pass.album.resolve_records(&requests).await;
        retry.observe_lookup(&resolutions);
        if retry.provider_auth_errors > 0 {
            retry.tasks.clear();
            break;
        }
        let paired: Vec<_> = resolutions
            .results
            .into_iter()
            .filter_map(|(source, resolution)| match resolution {
                RecordResolution::AssetPresent { master_record_name } => Some(
                    RecordLookupRequest::paired(source.clone(), master_record_name, source),
                ),
                _ => None,
            })
            .collect();
        if shutdown_token.is_cancelled() {
            break;
        }
        let resolutions = pass.album.resolve_records(&paired).await;
        retry.observe_lookup(&resolutions);
        if retry.provider_auth_errors > 0 {
            retry.tasks.clear();
            break;
        }
        for (_, resolution) in resolutions.results {
            if shutdown_token.is_cancelled() {
                break;
            }
            let RecordResolution::Present(asset) = resolution else {
                continue;
            };
            for task in failed_tasks.iter().filter(|task| {
                task.library.as_ref() == zone
                    && task.asset_record_name.as_ref() == asset.asset_record_name()
            }) {
                let Some((_, version)) = asset.versions().iter().find(|(size, version)| {
                    VersionSizeKey::from(*size) == task.version_size
                        && version.checksum == task.checksum
                        && version.size == task.size
                }) else {
                    continue;
                };
                if pending_keys.remove(&RetryTaskKey::from(task)) {
                    retry.tasks.push(DownloadTask {
                        url: version.url.clone(),
                        ..task.clone()
                    });
                }
            }
        }
    }
    let missing = requested_count.saturating_sub(retry.tasks.len());
    if missing > 0 {
        tracing::warn!(
            requested = requested_count,
            refreshed = retry.tasks.len(),
            missing,
            "Cleanup pass could not refresh every failed task; unmatched failures remain pending"
        );
    }
    retry
}

/// Rebuild failed tasks with fresh CDN URLs, using targeted lookups after expiry.
///
/// The first pass may fail because signed content URLs expired before the
/// worker reached them. Retrying the complete library after that is both slow
/// and risky: newly-issued URLs for early tasks can age again while unrelated
/// albums are planned. Limit cleanup to the exact asset/version/path tuples
/// that failed so the retry pass starts consuming refreshed URLs quickly.
pub(in crate::download) async fn build_retry_download_tasks(
    passes: &[crate::commands::AlbumPass],
    config: &DownloadConfig,
    failed_tasks: &[DownloadTask],
    refresh: CleanupUrlRefresh,
    shutdown_token: CancellationToken,
) -> Result<CleanupRetryPlan> {
    if failed_tasks.is_empty() || shutdown_token.is_cancelled() {
        return Ok(CleanupRetryPlan::default());
    }

    if matches!(refresh, CleanupUrlRefresh::Lookup) {
        return Ok(refresh_failed_download_urls(passes, failed_tasks, &shutdown_token).await);
    }

    let mut pending_keys: FxHashSet<RetryTaskKey> =
        failed_tasks.iter().map(RetryTaskKey::from).collect();
    let retry_state_ids = retry_state_ids_by_asset_record(failed_tasks);
    let requested_count = pending_keys.len();
    let pass_configs = build_pass_configs_resolving_deferred_excludes(passes, config).await?;
    let mut retry = CleanupRetryPlan {
        tasks: Vec::with_capacity(requested_count),
        ..CleanupRetryPlan::default()
    };
    let mut task_planner = planner::TaskPlanner::for_download(config.state_db.as_deref()).await?;

    for (pass_index, pass) in passes.iter().enumerate() {
        if pending_keys.is_empty() || shutdown_token.is_cancelled() {
            break;
        }

        let assets = pass.album.photos(config.recent).await?;
        #[allow(
            clippy::indexing_slicing,
            reason = "pass_index comes from enumerate() over `passes`; pass_configs is \
                      built 1:1 from the same slice"
        )]
        let pass_config = &pass_configs[pass_index];

        for asset in &assets {
            if pending_keys.is_empty() || shutdown_token.is_cancelled() {
                break;
            }
            let Some(state_id) = retry_state_ids.get(asset.asset_record_name()) else {
                continue;
            };
            let asset = asset.clone().with_state_record_name(Arc::clone(state_id));
            let plan = task_planner
                .plan_download_asset(&asset, pass_config)
                .await?;
            if plan.filter_reason.is_some() {
                continue;
            }
            take_matching_retry_tasks(plan.tasks, &mut pending_keys, &mut retry.tasks);
        }
    }

    if !pending_keys.is_empty() {
        tracing::warn!(
            requested = requested_count,
            refreshed = retry.tasks.len(),
            missing = pending_keys.len(),
            "Cleanup pass could not refresh every failed task; unmatched failures remain pending"
        );
    }

    Ok(retry)
}

/// Hydrate current incremental asset records and rebuild only the task tuples
/// that actually failed with expired CDN URLs.
///
/// Replaying the same token-bounded `/changes/zone` delta can return the same
/// signed URLs that just aged out. Hydration scans the current zone state
/// without that old sync token so the retry pass starts from newly-issued URLs
/// instead of immediately retrying the stale batch.
pub(super) async fn build_incremental_expired_url_retry_tasks(
    passes: &[crate::commands::AlbumPass],
    pass_configs: &[Arc<DownloadConfig>],
    retry_sources: &FxHashMap<RetryTaskKey, UrlRetrySource>,
    failed_tasks: &[DownloadTask],
    shutdown_token: CancellationToken,
) -> Result<Vec<DownloadTask>> {
    if failed_tasks.is_empty() {
        return Ok(Vec::new());
    }

    let mut pending_keys: FxHashSet<RetryTaskKey> =
        failed_tasks.iter().map(RetryTaskKey::from).collect();
    let retry_state_ids = retry_state_ids_by_asset_record(failed_tasks);
    let requested_count = pending_keys.len();
    let mut pass_indices_by_asset: FxHashMap<String, FxHashSet<usize>> = FxHashMap::default();

    for task in failed_tasks {
        let key = RetryTaskKey::from(task);
        let Some(source) = retry_sources.get(&key) else {
            tracing::warn!(
                asset_id = %task.asset_id,
                version_size = %task.version_size.as_str(),
                path = %task.download_path.display(),
                "Could not map expired incremental URL back to its source asset"
            );
            continue;
        };
        pass_indices_by_asset
            .entry(source.asset_record_name.to_string())
            .or_default()
            .insert(source.pass_index);
    }

    let mut tasks = Vec::with_capacity(requested_count);
    let mut task_planner = planner::TaskPlanner::for_download(
        pass_configs
            .first()
            .and_then(|config| config.state_db.as_deref()),
    )
    .await?;
    if !pending_keys.is_empty()
        && !pass_indices_by_asset.is_empty()
        && !shutdown_token.is_cancelled()
    {
        let mut missing_by_hydrator: FxHashMap<usize, FxHashSet<String>> = FxHashMap::default();
        for (asset_record_name, pass_indices) in &pass_indices_by_asset {
            let Some(pass_index) = retry_hydrator_pass_index(passes, pass_indices) else {
                continue;
            };
            missing_by_hydrator
                .entry(pass_index)
                .or_default()
                .insert(asset_record_name.clone());
        }

        let mut hydrated_asset_record_names = FxHashSet::default();
        for (pass_index, mut missing) in missing_by_hydrator {
            missing.retain(|asset_record_name| {
                !hydrated_asset_record_names.contains(asset_record_name.as_str())
            });
            if missing.is_empty() || pending_keys.is_empty() || shutdown_token.is_cancelled() {
                continue;
            }

            let Some(pass) = passes.get(pass_index) else {
                continue;
            };
            let assets = match pass
                .album
                .hydrate_matching_assets_from_changes(&mut missing)
                .await
            {
                Ok(assets) => assets,
                Err(e) => {
                    tracing::warn!(
                        pass_index,
                        error = %e,
                        "Failed to hydrate expired incremental retry assets"
                    );
                    continue;
                }
            };

            for asset in assets {
                let asset_record_name = asset.asset_record_name().to_string();
                hydrated_asset_record_names.insert(asset_record_name.clone());
                let Some(pass_indices) = pass_indices_by_asset.remove(asset_record_name.as_str())
                else {
                    continue;
                };
                let Some(state_id) = retry_state_ids.get(asset_record_name.as_str()) else {
                    continue;
                };
                let asset = asset.with_state_record_name(Arc::clone(state_id));

                for pass_index in pass_indices {
                    let Some(pass_config) = pass_configs.get(pass_index) else {
                        continue;
                    };
                    let plan = task_planner
                        .plan_download_asset(&asset, pass_config)
                        .await?;
                    if plan.filter_reason.is_some() {
                        continue;
                    }
                    take_matching_retry_tasks(plan.tasks, &mut pending_keys, &mut tasks);
                    if pending_keys.is_empty() || shutdown_token.is_cancelled() {
                        break;
                    }
                }
            }
        }
    }

    if !pending_keys.is_empty() {
        tracing::warn!(
            requested = requested_count,
            refreshed = tasks.len(),
            missing = pending_keys.len(),
            "Incremental expired-URL retry could not refresh every failed task"
        );
    }

    Ok(tasks)
}

pub(super) fn merge_expired_url_retry_result(
    pass_result: &mut PassResult,
    expired_retry_candidates: Vec<DownloadTask>,
    refreshed_keys: FxHashSet<RetryTaskKey>,
    retry_result: PassResult,
) {
    let mut still_failed: Vec<DownloadTask> = expired_retry_candidates
        .into_iter()
        .filter(|task| !refreshed_keys.contains(&RetryTaskKey::from(task)))
        .collect();
    let unrefreshed_expired_failures = still_failed.len();
    still_failed.extend(retry_result.failed);
    pass_result.failed = still_failed;
    pass_result.downloaded += retry_result.downloaded;
    pass_result
        .downloaded_tasks
        .extend(retry_result.downloaded_tasks);
    pass_result.auth_errors += retry_result.auth_errors;
    pass_result.exif_failures += retry_result.exif_failures;
    pass_result.state_write_failures += retry_result.state_write_failures;
    pass_result.bytes_downloaded += retry_result.bytes_downloaded;
    pass_result.disk_bytes_written += retry_result.disk_bytes_written;
    pass_result.rate_limit_observations += retry_result.rate_limit_observations;
    pass_result.photos_downloaded += retry_result.photos_downloaded;
    pass_result.videos_downloaded += retry_result.videos_downloaded;
    pass_result.recap.merge(retry_result.recap);
    pass_result.url_expired_abort =
        retry_result.url_expired_abort || unrefreshed_expired_failures > 0;
}

pub(super) async fn refresh_stale_incremental_tasks_before_download(
    passes: &[crate::commands::AlbumPass],
    pass_configs: &[Arc<DownloadConfig>],
    retry_sources: &FxHashMap<RetryTaskKey, UrlRetrySource>,
    tasks: Vec<DownloadTask>,
    urls_obtained_at: Option<Instant>,
    refresh_after: Duration,
    shutdown_token: CancellationToken,
) -> Vec<DownloadTask> {
    if tasks.is_empty() || shutdown_token.is_cancelled() {
        return tasks;
    }
    let Some(urls_obtained_at) = urls_obtained_at else {
        return tasks;
    };
    let url_age = urls_obtained_at.elapsed();
    if url_age < refresh_after {
        return tasks;
    }

    let requested = tasks.len();
    tracing::info!(
        requested,
        url_age_secs = url_age.as_secs_f64(),
        threshold_secs = refresh_after.as_secs_f64(),
        "Refreshing incremental download URLs before starting downloads"
    );

    let refreshed_tasks = match build_incremental_expired_url_retry_tasks(
        passes,
        pass_configs,
        retry_sources,
        &tasks,
        shutdown_token,
    )
    .await
    {
        Ok(tasks) => tasks,
        Err(e) => {
            tracing::warn!(
                requested,
                error = %e,
                "Could not refresh incremental download URLs before starting downloads; using original URLs"
            );
            return tasks;
        }
    };

    if refreshed_tasks.is_empty() {
        tracing::warn!(
            requested,
            "Pre-download incremental URL refresh returned no tasks; using original URLs"
        );
        return tasks;
    }

    let refreshed_count = refreshed_tasks.len();
    let mut refreshed_by_key: FxHashMap<RetryTaskKey, DownloadTask> = refreshed_tasks
        .into_iter()
        .map(|task| (RetryTaskKey::from(&task), task))
        .collect();
    let mut ordered_tasks = Vec::with_capacity(requested);
    let mut unrefreshed = 0usize;
    for task in tasks {
        let key = RetryTaskKey::from(&task);
        if let Some(refreshed_task) = refreshed_by_key.remove(&key) {
            ordered_tasks.push(refreshed_task);
        } else {
            unrefreshed += 1;
            ordered_tasks.push(task);
        }
    }

    if unrefreshed > 0 {
        tracing::warn!(
            requested,
            refreshed = refreshed_count,
            unrefreshed,
            "Pre-download incremental URL refresh could not refresh every task; preserving original URLs for the rest"
        );
    } else {
        tracing::info!(
            requested,
            refreshed = refreshed_count,
            "Pre-download incremental URL refresh completed"
        );
    }
    ordered_tasks
}

#[cfg(test)]
mod tests;
