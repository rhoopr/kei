//! Durable pending recovery without replaying completed source enumeration.

use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use reqwest::Client;
use tokio_util::sync::CancellationToken;

use crate::download::metadata_rewrite::CaptureTimestampRepair;
use crate::download::pipeline::{
    AUTH_ERROR_THRESHOLD, MetadataFlags, PassConfig, run_download_pass,
};
use crate::download::retry::{PendingRetryPlan, build_pending_retry_download_tasks};
use crate::icloud::photos::ProviderRecordId;

use super::config::DownloadConfig;
use super::models::{
    CheckpointEvidence, DownloadControls, DownloadOutcome, PENDING_RETRY_UNMATCHED_REASON,
    SyncResult, SyncStats, merge_download_outcomes,
};
use super::url_refresh::{
    build_incremental_expired_url_retry_tasks, merge_expired_url_retry_result,
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
        unmatched_targets,
        requested,
        identity_incomplete,
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
        let mut result = pending_retry_no_download_result(&started, &shutdown_token, unmatched, 0);
        result.checkpoint.identity_incomplete = identity_incomplete;
        result.checkpoint.project(&mut result.stats);
        return Ok(result);
    }

    let pass_config = PassConfig {
        prior_auth_errors: 0,
        url_obtained_at: Default::default(),
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
    let mut provider_auth_errors = 0usize;
    let mut pass_result = run_download_pass(pass_config, tasks).await;
    if pass_result.url_expired
        && pass_result.auth_errors < AUTH_ERROR_THRESHOLD
        && provider_auth_errors == 0
        && pass_result.state_write_failures == 0
        && !shutdown_token.is_cancelled()
    {
        let expired_retry_candidates = pass_result.expired_tasks.clone();
        let retry_plan = build_incremental_expired_url_retry_tasks(
            passes,
            &retry_sources,
            &expired_retry_candidates,
            shutdown_token.clone(),
        )
        .await;
        provider_auth_errors += retry_plan.provider_auth_errors;
        pass_result.rate_limit_observations += retry_plan.rate_limit_observations;
        let retry_tasks = retry_plan.tasks;
        if !retry_tasks.is_empty() {
            let retry_task_count = retry_tasks.len();
            tracing::info!(
                count = retry_task_count,
                "Retrying tasks after targeted pending URL refresh"
            );
            let retry_pass_config = PassConfig {
                prior_auth_errors: pass_result.auth_errors,
                url_obtained_at: retry_plan.url_obtained_at,
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
                retry_result,
            );
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

    let checkpoint = CheckpointEvidence {
        identity_incomplete,
        state_write_failures: pass_result.state_write_failures,
        interrupted: shutdown_token.is_cancelled()
            || (pass_result.auth_errors >= AUTH_ERROR_THRESHOLD || provider_auth_errors > 0),
        revalidate_records: checkpoint_revalidate_records,
        ..CheckpointEvidence::default()
    };
    let mut stats = SyncStats {
        downloaded: pass_result.downloaded,
        failed: failed.saturating_add(remaining_unmatched),
        bytes_downloaded: pass_result.bytes_downloaded,
        disk_bytes_written: pass_result.disk_bytes_written,
        exif_failures: pass_result.exif_failures,
        elapsed_secs: started.elapsed().as_secs_f64(),
        rate_limited: pass_result.rate_limit_observations,
        photos_downloaded: pass_result.photos_downloaded,
        videos_downloaded: pass_result.videos_downloaded,
        recap: pass_result.recap.clone(),
        ..SyncStats::default()
    };
    let failed_count =
        failed + remaining_unmatched + pass_result.exif_failures + pass_result.state_write_failures;
    let outcome = if pass_result.auth_errors >= AUTH_ERROR_THRESHOLD || provider_auth_errors > 0 {
        DownloadOutcome::SessionExpired {
            auth_error_count: pass_result.auth_errors + provider_auth_errors,
        }
    } else if failed_count > 0 {
        DownloadOutcome::PartialFailure { failed_count }
    } else {
        DownloadOutcome::Success
    };
    checkpoint.project(&mut stats);
    Ok(SyncResult {
        outcome,
        sync_token: None,
        stats,
        checkpoint,
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
    let checkpoint = CheckpointEvidence {
        interrupted: shutdown_token.is_cancelled(),
        ..CheckpointEvidence::default()
    };
    let mut stats = SyncStats {
        downloaded,
        failed: remaining_unmatched,
        elapsed_secs: started.elapsed().as_secs_f64(),
        ..SyncStats::default()
    };
    let outcome = if remaining_unmatched > 0 {
        DownloadOutcome::PartialFailure {
            failed_count: remaining_unmatched,
        }
    } else {
        DownloadOutcome::Success
    };
    checkpoint.project(&mut stats);
    SyncResult {
        outcome,
        sync_token: None,
        stats,
        checkpoint,
        full_enumeration_ran: false,
    }
}

pub(super) async fn append_targeted_recovery_to_sync_result(
    download_client: &Client,
    passes: &[crate::commands::AlbumPass],
    config: &Arc<DownloadConfig>,
    controls: DownloadControls,
    shutdown_token: CancellationToken,
    mut sync_result: SyncResult,
) -> Result<SyncResult> {
    if !matches!(sync_result.outcome, DownloadOutcome::Success)
        || sync_result.checkpoint.interrupted
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
            SyncResult::from_execution(
                DownloadOutcome::PartialFailure { failed_count: 1 },
                None,
                stats,
            )
        }
    };

    sync_result.outcome = merge_download_outcomes(&sync_result.outcome, &retry_result.outcome);
    sync_result.accumulate(&retry_result);
    if sync_result.checkpoint.sync_token_blocked
        || sync_result.checkpoint.interrupted
        || controls.run_mode.is_dry_run()
        || controls.run_mode.only_print_filenames()
    {
        sync_result.sync_token = None;
    }
    Ok(sync_result)
}

#[cfg(test)]
mod tests;
