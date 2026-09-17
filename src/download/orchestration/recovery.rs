//! Durable pending recovery without replaying completed source enumeration.

use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use reqwest::Client;
use rustc_hash::FxHashSet;
use tokio_util::sync::CancellationToken;

use crate::download::filter::DownloadTask;
use crate::download::metadata_rewrite::CaptureTimestampRepair;
use crate::download::pipeline::{
    AUTH_ERROR_THRESHOLD, MetadataFlags, PassConfig, run_download_pass,
};
use crate::download::retry::{PendingRetryPlan, build_pending_retry_download_tasks};
use crate::icloud::photos::ProviderRecordId;

use super::config::DownloadConfig;
use super::models::{
    DownloadControls, DownloadOutcome, PENDING_RETRY_UNMATCHED_REASON, SyncResult, SyncStats,
    merge_download_outcomes,
};
use super::url_refresh::{
    RetryTaskKey, build_incremental_expired_url_retry_tasks, merge_expired_url_retry_result,
};

async fn run_targeted_recovery_pass(
    download_client: &Client,
    passes: &[crate::commands::AlbumPass],
    config: &Arc<DownloadConfig>,
    controls: DownloadControls,
    shutdown_token: CancellationToken,
) -> Result<SyncResult> {
    let started = Instant::now();
    let plan = build_pending_retry_download_tasks(
        passes,
        config,
        controls.run_mode,
        shutdown_token.clone(),
    )
    .await?;
    let PendingRetryPlan {
        tasks,
        retry_sources,
        pass_configs,
        unmatched_targets,
        requested,
    } = plan;
    let unmatched = unmatched_targets.len();

    if requested == 0 {
        return Ok(pending_retry_no_download_result(
            &started,
            &shutdown_token,
            0,
            0,
        ));
    }

    tracing::info!(
        requested,
        refreshed = tasks.len(),
        unmatched,
        "Retrying pending assets with targeted enumeration"
    );

    if controls.run_mode.only_print_filenames() {
        #[allow(
            clippy::print_stdout,
            reason = "--only-print-filenames writes target paths to stdout so callers can pipe to xargs/etc"
        )]
        for task in &tasks {
            println!("{}", task.download_path.display());
        }
        return Ok(pending_retry_no_download_result(
            &started,
            &shutdown_token,
            unmatched,
            0,
        ));
    }

    if controls.run_mode.is_dry_run() {
        return Ok(pending_retry_no_download_result(
            &started,
            &shutdown_token,
            unmatched,
            tasks.len(),
        ));
    }

    if tasks.is_empty() {
        return Ok(pending_retry_no_download_result(
            &started,
            &shutdown_token,
            unmatched,
            0,
        ));
    }

    let pass_config = PassConfig {
        client: download_client,
        retry_config: &config.retry,
        metadata: MetadataFlags::from(config.as_ref()),
        mark_capture_repair_after_download: matches!(
            config.capture_timestamp_repair,
            CaptureTimestampRepair::ReplaceWithCaptureLocal
        ),
        concurrency: config.concurrent_downloads,
        reporting: controls.reporting,
        temp_suffix: Arc::clone(&config.temp_suffix),
        shutdown_token: shutdown_token.clone(),
        state_db: config.state_db.clone(),
        rate_limit_counter: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        bandwidth_limiter: config.bandwidth_limiter.clone(),
        library: Arc::clone(&config.library),
    };
    let planned_tasks = tasks.clone();
    let mut pass_result = run_download_pass(pass_config, tasks).await;
    if pass_result.url_expired_abort
        && pass_result.auth_errors < AUTH_ERROR_THRESHOLD
        && !shutdown_token.is_cancelled()
    {
        let downloaded_keys: FxHashSet<RetryTaskKey> = pass_result
            .downloaded_tasks
            .iter()
            .map(RetryTaskKey::from)
            .collect();
        let expired_retry_candidates: Vec<DownloadTask> = planned_tasks
            .iter()
            .filter(|task| !downloaded_keys.contains(&RetryTaskKey::from(*task)))
            .cloned()
            .collect();
        let retry_tasks = match build_incremental_expired_url_retry_tasks(
            passes,
            &pass_configs,
            &retry_sources,
            &expired_retry_candidates,
            shutdown_token.clone(),
        )
        .await
        {
            Ok(tasks) => tasks,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "Could not refresh expired pending-retry download URLs; leaving failures for replay"
                );
                Vec::new()
            }
        };
        if !retry_tasks.is_empty() {
            let retry_task_count = retry_tasks.len();
            tracing::info!(
                count = retry_task_count,
                "Refreshing expired pending-retry download URLs and retrying failed tasks"
            );
            let refreshed_keys: FxHashSet<RetryTaskKey> =
                retry_tasks.iter().map(RetryTaskKey::from).collect();
            let retry_pass_config = PassConfig {
                client: download_client,
                retry_config: &config.retry,
                metadata: MetadataFlags::from(config.as_ref()),
                mark_capture_repair_after_download: matches!(
                    config.capture_timestamp_repair,
                    CaptureTimestampRepair::ReplaceWithCaptureLocal
                ),
                concurrency: config.concurrent_downloads,
                reporting: controls.reporting,
                temp_suffix: Arc::clone(&config.temp_suffix),
                shutdown_token: shutdown_token.clone(),
                state_db: config.state_db.clone(),
                rate_limit_counter: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                bandwidth_limiter: config.bandwidth_limiter.clone(),
                library: Arc::clone(&config.library),
            };
            let retry_result = run_download_pass(retry_pass_config, retry_tasks).await;
            merge_expired_url_retry_result(
                &mut pass_result,
                expired_retry_candidates,
                refreshed_keys,
                retry_result,
            );
        } else if !expired_retry_candidates.is_empty() {
            pass_result.failed = expired_retry_candidates;
        }
    }
    let failed = pass_result.failed.len();
    if failed > 0 {
        for task in &pass_result.failed {
            tracing::error!(asset_id = %task.asset_id, path = %task.download_path.display(), "Targeted retry failed");
        }
    }
    let remaining_unmatched = unmatched;
    let checkpoint_revalidate_records = unmatched_targets
        .iter()
        .map(|target| ProviderRecordId::new(target.asset_id.to_string()))
        .collect();

    let stats = SyncStats {
        downloaded: pass_result.downloaded,
        failed: failed.saturating_add(remaining_unmatched),
        bytes_downloaded: pass_result.bytes_downloaded,
        disk_bytes_written: pass_result.disk_bytes_written,
        exif_failures: pass_result.exif_failures,
        state_write_failures: pass_result.state_write_failures,
        elapsed_secs: started.elapsed().as_secs_f64(),
        interrupted: shutdown_token.is_cancelled()
            || pass_result.auth_errors >= AUTH_ERROR_THRESHOLD,
        rate_limited: pass_result.rate_limit_observations,
        photos_downloaded: pass_result.photos_downloaded,
        videos_downloaded: pass_result.videos_downloaded,
        recap: pass_result.recap.clone(),
        checkpoint_revalidate_records,
        ..SyncStats::default()
    };
    if pass_result.auth_errors >= AUTH_ERROR_THRESHOLD {
        return Ok(SyncResult {
            outcome: DownloadOutcome::SessionExpired {
                auth_error_count: pass_result.auth_errors,
            },
            sync_token: None,
            stats,
            full_enumeration_ran: false,
        });
    }

    let failed_count =
        failed + remaining_unmatched + pass_result.exif_failures + pass_result.state_write_failures;
    Ok(SyncResult {
        outcome: if failed_count > 0 {
            DownloadOutcome::PartialFailure { failed_count }
        } else {
            DownloadOutcome::Success
        },
        sync_token: None,
        stats,
        full_enumeration_ran: false,
    })
}

fn pending_retry_no_download_result(
    started: &Instant,
    shutdown_token: &CancellationToken,
    unmatched: usize,
    downloaded: usize,
) -> SyncResult {
    let remaining_unmatched = unmatched;
    let stats = SyncStats {
        downloaded,
        failed: remaining_unmatched,
        elapsed_secs: started.elapsed().as_secs_f64(),
        interrupted: shutdown_token.is_cancelled(),
        ..SyncStats::default()
    };
    SyncResult {
        outcome: if remaining_unmatched > 0 {
            DownloadOutcome::PartialFailure {
                failed_count: remaining_unmatched,
            }
        } else {
            DownloadOutcome::Success
        },
        sync_token: None,
        stats,
        full_enumeration_ran: false,
    }
}

pub(super) async fn append_targeted_recovery_to_sync_result(
    download_client: &Client,
    passes: &[crate::commands::AlbumPass],
    config: &Arc<DownloadConfig>,
    controls: DownloadControls,
    shutdown_token: CancellationToken,
    sync_result: SyncResult,
) -> Result<SyncResult> {
    if !matches!(sync_result.outcome, DownloadOutcome::Success)
        || sync_result.stats.interrupted
        || shutdown_token.is_cancelled()
    {
        return Ok(sync_result);
    }

    let retry_result = match run_targeted_recovery_pass(
        download_client,
        passes,
        config,
        controls,
        shutdown_token.clone(),
    )
    .await
    {
        Ok(result) => result,
        Err(e) => {
            let stats = SyncStats {
                failed: 1,
                elapsed_secs: 0.0,
                ..SyncStats::default()
            };
            tracing::warn!(
                error = %e,
                diagnostic = PENDING_RETRY_UNMATCHED_REASON,
                "Targeted recovery failed before downloads; retaining durable state for a later cycle"
            );
            SyncResult {
                outcome: DownloadOutcome::PartialFailure { failed_count: 1 },
                sync_token: None,
                stats,
                full_enumeration_ran: false,
            }
        }
    };

    let SyncResult {
        outcome: source_outcome,
        sync_token: source_sync_token,
        stats: mut combined_stats,
        full_enumeration_ran: source_full_enumeration_ran,
    } = sync_result;
    let outcome = merge_download_outcomes(&source_outcome, &retry_result.outcome);
    combined_stats.accumulate(&retry_result.stats);
    let sync_token = if !combined_stats.sync_token_blocked
        && !combined_stats.interrupted
        && !controls.run_mode.is_dry_run()
        && !controls.run_mode.only_print_filenames()
    {
        source_sync_token
    } else {
        None
    };

    Ok(SyncResult {
        outcome,
        sync_token,
        stats: combined_stats,
        full_enumeration_ran: source_full_enumeration_ran || retry_result.full_enumeration_ran,
    })
}

#[cfg(test)]
mod tests;
