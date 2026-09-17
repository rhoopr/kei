//! Cycle ledger projection and refresh-tail reporting.

#[cfg(debug_assertions)]
use crate::notifications::Notifier;
use crate::state;
#[cfg(debug_assertions)]
use crate::{config, download, health};

pub(super) fn merge_refresh_tail_outcome(
    cycle_result: &mut crate::sync_cycle::CycleResult,
    failed: usize,
    interrupted: bool,
) {
    cycle_result.failed_count = cycle_result.failed_count.saturating_add(failed);
    cycle_result.stats.failed = cycle_result.stats.failed.saturating_add(failed);
    cycle_result.stats.interrupted |= interrupted;
}

fn sync_run_stats_from_cycle(cycle_result: &crate::sync_cycle::CycleResult) -> state::SyncRunStats {
    state::SyncRunStats {
        assets_seen: cycle_result.stats.assets_seen,
        assets_downloaded: u64::try_from(cycle_result.stats.downloaded).unwrap_or(u64::MAX),
        assets_failed: u64::try_from(cycle_result.stats.failed).unwrap_or(u64::MAX),
        enumeration_errors: u64::try_from(cycle_result.stats.enumeration_errors)
            .unwrap_or(u64::MAX),
        interrupted: cycle_result.stats.interrupted,
        api_total_at_start: cycle_result.stats.api_total_at_start,
        api_total_at_start_partial: cycle_result.stats.api_total_at_start_partial,
        inventory_drop_warnings: u64::try_from(cycle_result.stats.inventory_drop_warnings)
            .unwrap_or(u64::MAX),
        inventory_drop_previous_total: cycle_result.stats.inventory_drop_previous_total,
        inventory_drop_current_total: cycle_result.stats.inventory_drop_current_total,
        inventory_drop_library: cycle_result.stats.inventory_drop_library.clone(),
    }
}

// Debug-only seam for the offline binary-boundary report test. Release builds
// must never skip authentication or CloudKit work from an environment variable.
#[cfg(debug_assertions)]
pub(super) async fn maybe_write_offline_fake_sync_report(
    config: &config::Config,
    notifier: &Notifier,
) -> anyhow::Result<bool> {
    const ENV: &str = "KEI_UNSTABLE_FAKE_SYNC_REPORT_FOR_TESTS";
    if std::env::var(ENV).as_deref() != Ok("1") {
        return Ok(false);
    }

    let Some(report_path) = config.report.json.as_deref() else {
        anyhow::bail!("{ENV}=1 requires [report].json");
    };

    tracing::warn!(
        env = ENV,
        "Offline fake sync report test seam enabled; exiting before authentication"
    );

    let reporter = crate::cycle_reporter::CycleReporter::<state::SqliteStateDb>::new(
        crate::cycle_reporter::CycleReporterConfig {
            watch_mode: config.watch.interval.is_some(),
            report_path: Some(report_path),
            run_options: crate::report::RunOptions::from_config(config),
            health_dir: &config.auth.cookie_directory,
            personality_mode: config.ui.personality_mode,
            state_db: None,
            metrics_handle: None,
            notifier,
        },
    );
    let stats = download::SyncStats {
        assets_seen: 3,
        downloaded: 2,
        skipped: download::SkipBreakdown {
            by_state: 1,
            ..download::SkipBreakdown::default()
        },
        bytes_downloaded: 4096,
        disk_bytes_written: 4096,
        elapsed_secs: 0.125,
        photos_downloaded: 1,
        videos_downloaded: 1,
        ..download::SyncStats::default()
    };
    let mut health = health::HealthStatus::new();
    reporter
        .report_completed_cycle(
            &mut health,
            crate::cycle_reporter::CycleFacts::new(
                &stats,
                0,
                false,
                std::time::Duration::from_millis(125),
            ),
        )
        .await;

    if !report_path.is_file() {
        anyhow::bail!("offline fake sync did not write {}", report_path.display());
    }

    Ok(true)
}

/// Record completed cycle facts without changing the provider checkpoint gate.
pub(super) async fn record_cycle_run(
    state_db: Option<&dyn crate::download::DownloadStore>,
    cycle_result: &crate::sync_cycle::CycleResult,
    cycle_wall_started_at: chrono::DateTime<chrono::Utc>,
) {
    if let Some(db) = state_db {
        let stats = sync_run_stats_from_cycle(cycle_result);
        match db.start_sync_run_at(cycle_wall_started_at).await {
            Ok(run_id) => {
                if let Err(e) = db.complete_sync_run(run_id, &stats).await {
                    tracing::warn!(
                        error = %e,
                        run_id,
                        "Failed to complete sync_runs ledger row"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to start sync_runs ledger row");
            }
        }
    }
}

#[cfg(test)]
mod tests;
