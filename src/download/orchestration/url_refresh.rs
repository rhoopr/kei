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
    pub(in crate::download) library: Arc<str>,
    pub(in crate::download) version_size: VersionSizeKey,
    pub(in crate::download) download_path: std::path::PathBuf,
}

#[derive(Debug, Clone)]
pub(in crate::download) struct UrlRetrySource {
    pub(in crate::download) asset_record_name: Arc<str>,
    pub(in crate::download) pass_index: usize,
    pub(in crate::download) master_record_name: Arc<str>,
    pub(in crate::download) provider_version: VersionSizeKey,
}

impl From<&DownloadTask> for RetryTaskKey {
    fn from(task: &DownloadTask) -> Self {
        Self {
            asset_id: Arc::clone(&task.asset_id),
            library: Arc::clone(&task.library),
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
    pub(in crate::download) unrefreshed: Vec<DownloadTask>,
    pub(in crate::download) url_obtained_at: FxHashMap<RetryTaskKey, Instant>,
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
    retry_sources: Option<&FxHashMap<RetryTaskKey, UrlRetrySource>>,
) -> CleanupRetryPlan {
    let started = Instant::now();
    let mut retry = CleanupRetryPlan::default();
    let source_matches = |task: &DownloadTask| {
        retry_sources.is_none_or(|sources| {
            sources
                .get(&RetryTaskKey::from(task))
                .is_some_and(|source| {
                    source.asset_record_name == task.asset_record_name
                        && passes
                            .get(source.pass_index)
                            .is_some_and(|pass| pass.album.zone_name() == task.library.as_ref())
                })
        })
    };
    let mut pending_keys: FxHashSet<_> = failed_tasks.iter().map(RetryTaskKey::from).collect();
    let requested_count = pending_keys.len();
    let mut planned_unique_child_requests = 0usize;
    let mut child_master_references = 0usize;
    let mut child_lookup_results = 0usize;
    let mut paired_lookup_results = 0usize;
    let mut planned_paired_requests = 0usize;
    let mut present_pairs = 0usize;
    let mut rejected_children = std::collections::BTreeMap::<&'static str, usize>::new();
    let mut rejected_tasks =
        std::collections::BTreeMap::<&'static str, FxHashSet<RetryTaskKey>>::new();
    for task in failed_tasks.iter().filter(|task| !source_matches(task)) {
        rejected_tasks
            .entry("selection_source_mismatch")
            .or_default()
            .insert(RetryTaskKey::from(task));
    }
    tracing::info!(
        requested = requested_count,
        "Starting targeted download URL refresh"
    );
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
            .filter(|task| task.library.as_ref() == zone && source_matches(task))
            .map(|task| task.asset_record_name.as_ref())
            .collect::<FxHashSet<_>>()
            .into_iter()
            .map(|name| RecordLookupRequest::asset_only(ProviderRecordId::new(name)))
            .collect();
        planned_unique_child_requests += requests.len();
        let resolutions = tokio::select! {
            biased;
            () = shutdown_token.cancelled() => break,
            batch = pass.album.resolve_records(&requests) => batch,
        };
        retry.observe_lookup(&resolutions);
        child_lookup_results += resolutions.results.len();
        child_master_references += resolutions
            .results
            .iter()
            .filter(|(_, resolution)| matches!(resolution, RecordResolution::AssetPresent { .. }))
            .count();
        if retry.provider_auth_errors > 0 {
            retry.tasks.clear();
            retry.url_obtained_at.clear();
            break;
        }
        let paired: Vec<_> = resolutions
            .results
            .into_iter()
            .filter_map(|(source, resolution)| match resolution {
                RecordResolution::AssetPresent { master_record_name }
                    if retry_sources.is_none_or(|sources| {
                        failed_tasks.iter().any(|task| {
                            task.library.as_ref() == zone
                                && task.asset_record_name.as_ref() == source.as_str()
                                && sources
                                    .get(&RetryTaskKey::from(task))
                                    .is_some_and(|expected| {
                                        expected.master_record_name.as_ref()
                                            == master_record_name.as_str()
                                    })
                        })
                    }) =>
                {
                    Some(RecordLookupRequest::paired(
                        source.clone(),
                        master_record_name,
                        source,
                    ))
                }
                RecordResolution::AssetPresent { .. } => {
                    *rejected_children
                        .entry("selected_master_mismatch")
                        .or_default() += 1;
                    None
                }
                RecordResolution::Deleted { .. } => {
                    *rejected_children.entry("child_deleted").or_default() += 1;
                    None
                }
                RecordResolution::TransientFailure(_) => {
                    *rejected_children.entry("child_lookup_failed").or_default() += 1;
                    None
                }
                _ => {
                    *rejected_children
                        .entry("child_identity_unresolved")
                        .or_default() += 1;
                    None
                }
            })
            .collect();
        if shutdown_token.is_cancelled() {
            break;
        }
        planned_paired_requests += paired.len();
        let resolutions = tokio::select! {
            biased;
            () = shutdown_token.cancelled() => break,
            batch = pass.album.resolve_records(&paired) => batch,
        };
        retry.observe_lookup(&resolutions);
        paired_lookup_results += resolutions.results.len();
        present_pairs += resolutions
            .results
            .iter()
            .filter(|(_, resolution)| matches!(resolution, RecordResolution::Present(_)))
            .count();
        if retry.provider_auth_errors > 0 {
            retry.tasks.clear();
            retry.url_obtained_at.clear();
            break;
        }
        for (source, resolution) in resolutions.results {
            if shutdown_token.is_cancelled() {
                break;
            }
            let RecordResolution::Present(asset) = resolution else {
                continue;
            };
            for task in failed_tasks.iter().filter(|task| {
                task.library.as_ref() == zone
                    && source_matches(task)
                    && task.asset_record_name.as_ref() == asset.asset_record_name()
            }) {
                if retry_sources.is_some_and(|sources| {
                    sources
                        .get(&RetryTaskKey::from(task))
                        .is_none_or(|source| source.master_record_name.as_ref() != asset.id())
                }) {
                    rejected_tasks
                        .entry("selected_master_mismatch")
                        .or_default()
                        .insert(RetryTaskKey::from(task));
                    continue;
                }
                let provider_version = retry_sources
                    .and_then(|sources| sources.get(&RetryTaskKey::from(task)))
                    .map_or(task.version_size, |source| source.provider_version);
                let Some((_, version)) = asset.versions().iter().find(|(size, version)| {
                    VersionSizeKey::from(*size) == provider_version
                        && version.checksum == task.checksum
                        && version.size == task.size
                }) else {
                    let reason = asset
                        .versions()
                        .iter()
                        .find(|(size, _)| VersionSizeKey::from(*size) == provider_version)
                        .map_or("rendition_missing", |(_, version)| {
                            match (version.checksum == task.checksum, version.size == task.size) {
                                (false, false) => "checksum_and_size_mismatch",
                                (false, true) => "checksum_mismatch",
                                (true, false) => "size_mismatch",
                                (true, true) => "resource_unresolved",
                            }
                        });
                    rejected_tasks
                        .entry(reason)
                        .or_default()
                        .insert(RetryTaskKey::from(task));
                    continue;
                };
                if pending_keys.remove(&RetryTaskKey::from(task)) {
                    if let Some(observed_at) = resolutions.url_observed_at.get(&source) {
                        retry
                            .url_obtained_at
                            .insert(RetryTaskKey::from(task), *observed_at);
                    }
                    retry.tasks.push(DownloadTask {
                        url: version.url.clone(),
                        ..task.clone()
                    });
                }
            }
        }
    }
    let refreshed_keys: FxHashSet<_> = retry.tasks.iter().map(RetryTaskKey::from).collect();
    for (reason, rejected_child_requests) in rejected_children {
        tracing::info!(
            diagnostic = "exact_refresh_child_rejection_v1",
            reason,
            rejected_child_requests,
            "Exact URL refresh retained child lookup work"
        );
    }
    for (reason, keys) in rejected_tasks {
        tracing::info!(
            diagnostic = "exact_refresh_task_rejection_v1",
            reason,
            rejected_task_keys = keys.len(),
            "Exact URL refresh retained selected task work"
        );
    }
    tracing::info!(
        diagnostic = "exact_refresh_outcomes_v1",
        requested_task_keys = requested_count,
        planned_unique_child_requests,
        child_master_references,
        child_lookup_results,
        planned_paired_requests,
        paired_lookup_results,
        present_pairs,
        refreshed_task_keys = refreshed_keys.len(),
        remaining_task_keys = requested_count.saturating_sub(refreshed_keys.len()),
        cancellation_observed = shutdown_token.is_cancelled(),
        authentication_failed = retry.provider_auth_errors > 0,
        "Exact URL refresh request and task counts"
    );
    retry.unrefreshed = failed_tasks
        .iter()
        .filter(|task| !refreshed_keys.contains(&RetryTaskKey::from(*task)))
        .cloned()
        .collect();
    let missing = retry.unrefreshed.len();
    if missing > 0 {
        tracing::warn!(
            requested = requested_count,
            refreshed = retry.tasks.len(),
            missing,
            "Cleanup pass could not refresh every failed task; unmatched failures remain pending"
        );
    }
    tracing::info!(
        requested = requested_count,
        refreshed = retry.tasks.len(),
        missing,
        phase_elapsed_secs = started.elapsed().as_secs_f64(),
        oldest_refreshed_url_observed_age_secs = ?retry.url_obtained_at.values().map(|at| at.elapsed()).max().map(|age| age.as_secs_f64()),
        "Targeted download URL refresh completed"
    );
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
        return Ok(refresh_failed_download_urls(passes, failed_tasks, &shutdown_token, None).await);
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

/// Refresh exact selected resources through the authenticated source zone's
/// bounded child/master lookup. Replanning and zone enumeration cannot change
/// the task's path, rendition, metadata or publication authorization.
pub(in crate::download) async fn build_incremental_expired_url_retry_tasks(
    passes: &[crate::commands::AlbumPass],
    retry_sources: &FxHashMap<RetryTaskKey, UrlRetrySource>,
    failed_tasks: &[DownloadTask],
    shutdown_token: CancellationToken,
) -> CleanupRetryPlan {
    refresh_failed_download_urls(passes, failed_tasks, &shutdown_token, Some(retry_sources)).await
}

pub(super) fn merge_expired_url_retry_result(
    pass_result: &mut PassResult,
    expired_retry_candidates: Vec<DownloadTask>,
    retry_result: PassResult,
) {
    let downloaded_keys: FxHashSet<_> = retry_result
        .downloaded_tasks
        .iter()
        .map(RetryTaskKey::from)
        .collect();
    let mut still_failed: Vec<_> = pass_result
        .failed
        .drain(..)
        .chain(expired_retry_candidates)
        .filter(|task| !downloaded_keys.contains(&RetryTaskKey::from(task)))
        .collect();
    still_failed.extend(retry_result.failed);
    let mut seen = FxHashSet::default();
    still_failed.retain(|task| seen.insert(RetryTaskKey::from(task)));
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
    pass_result.url_expired = retry_result.url_expired;
}

pub(super) async fn refresh_stale_incremental_tasks_before_download(
    passes: &[crate::commands::AlbumPass],
    retry_sources: &FxHashMap<RetryTaskKey, UrlRetrySource>,
    tasks: Vec<DownloadTask>,
    urls_obtained_at: Option<Instant>,
    refresh_after: Duration,
    shutdown_token: CancellationToken,
) -> CleanupRetryPlan {
    if tasks.is_empty() || shutdown_token.is_cancelled() {
        return CleanupRetryPlan {
            tasks,
            ..CleanupRetryPlan::default()
        };
    }
    let Some(urls_obtained_at) = urls_obtained_at else {
        return CleanupRetryPlan {
            tasks,
            ..CleanupRetryPlan::default()
        };
    };
    let url_age = urls_obtained_at.elapsed();
    if url_age < refresh_after {
        return CleanupRetryPlan {
            tasks,
            ..CleanupRetryPlan::default()
        };
    }

    let requested = tasks.len();
    tracing::info!(
        requested,
        first_url_observed_age_secs = url_age.as_secs_f64(),
        threshold_secs = refresh_after.as_secs_f64(),
        "Refreshing incremental download URLs before starting downloads"
    );

    build_incremental_expired_url_retry_tasks(passes, retry_sources, &tasks, shutdown_token).await
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod bounded_tests;

#[cfg(test)]
mod diagnostics_tests;
