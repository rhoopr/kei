//! Drive sync core cycles for the `sync` command and watch mode.

use std::path::PathBuf;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::auth::SharedSession;
use crate::config::Config;
use crate::download::filter::PathDerivationConfig;
use crate::icloud::photos::{PRIMARY_ZONE_NAME, PhotoLibrary};
use crate::retry::RetryConfig;
use crate::state::db::account::AccountOwner;

use super::{CoreConfig, CycleReport, run_cycle};

pub(crate) struct Run<'a> {
    pub(crate) config: &'a Config,
    pub(crate) libraries: Vec<PhotoLibrary>,
    pub(crate) session: SharedSession,
    pub(crate) owner: &'a AccountOwner,
    pub(crate) retry: RetryConfig,
    pub(crate) shutdown: &'a CancellationToken,
    pub(crate) notifier: &'a crate::notifications::Notifier,
    /// Present in watch mode, where `/metrics` and `/healthz` are served.
    pub(crate) metrics: Option<&'a crate::metrics::MetricsHandle>,
    /// Used to sign in again when iCloud refuses the session.
    pub(crate) password: &'a crate::password::PasswordProvider,
    pub(crate) input_mode: crate::InputMode,
    pub(crate) systemd: crate::systemd::SystemdNotifier,
}

/// Run one cycle, or cycles at the watch interval until shutdown.
pub(crate) async fn run(run: Run<'_>) -> anyhow::Result<()> {
    let config = run.config;
    let location = super::access::Location::resolve(
        &config.auth.cookie_directory,
        &config.auth.username,
        config.auth.domain.as_str(),
    )
    .await?;
    if config.runtime.dry_run {
        return preview(&run, &location).await;
    }
    let state =
        match super::access::open(&location, run.owner, Some(&config.download.directory)).await? {
            Some(state) => state,
            None => super::access::create(&location, run.owner).await?,
        };
    let core = core_config(config, run.retry);
    anyhow::ensure!(!run.libraries.is_empty(), "No library is selected.");

    let mut health = crate::health::HealthStatus::new();
    let mut reauth_attempts = 0;
    loop {
        run.notifier.notify(
            crate::notifications::Event::SyncStarted,
            "Sync cycle starting",
            &config.auth.username,
            None,
            None,
        );
        crate::support::begin(crate::support::runtime_configuration(config));
        let started = std::time::Instant::now();
        let client = run.session.read().await.download_client().clone();
        let report = run_cycle(&state, &run.libraries, &core, &client, run.shutdown).await?;
        log_report(&report);
        publish(&run, &state, &report, started.elapsed(), &mut health).await;
        if report.session_expired && !run.shutdown.is_cancelled() {
            if let Some(metrics) = run.metrics {
                metrics.record_session_expiration();
            }
            // Everything observed so far is recorded; sign in again and
            // repeat the cycle from the saved positions.
            crate::sync_loop::session::recover_expired_cycle(
                &run.session,
                config,
                run.password,
                run.notifier,
                &mut reauth_attempts,
                run.input_mode,
            )
            .await?;
            continue;
        }
        reauth_attempts = 0;
        let listed_everywhere = !run.shutdown.is_cancelled()
            && report
                .zones
                .iter()
                .all(|zone| zone.caught_up && zone.error.is_none());
        if listed_everywhere && let Some(freed) = super::upgrade::finish(&state).await? {
            tracing::info!(
                freed_bytes = freed,
                "Removed the previous state file and its backups"
            );
        }
        let Some(interval) = config.watch.interval else {
            if run.shutdown.is_cancelled() {
                return Ok(());
            }
            anyhow::ensure!(
                report.completeness.failed == 0
                    && report.zones.iter().all(|zone| zone.error.is_none()),
                "Sync finished with {} failed downloads; the next run retries them",
                report.completeness.failed
            );
            return Ok(());
        };
        // Let another kei process use the session while this one sleeps.
        if let Err(error) = run.session.read().await.release_lock() {
            tracing::warn!(error = %error, "Could not release the session lock before waiting");
        }
        run.systemd
            .notify_status(&format!("Waiting {interval} seconds..."));
        tracing::info!(interval_secs = interval, "Waiting before next cycle");
        tokio::select! {
            () = run.shutdown.cancelled() => return Ok(()),
            () = tokio::time::sleep(Duration::from_secs(interval)) => {}
        }
        crate::sync_loop::session::reacquire_session(
            &run.session,
            config,
            run.password,
            run.input_mode,
        )
        .await?;
    }
}

/// Report one cycle to the health file, metrics, the notification script,
/// the JSON report and support history. Failures here are logged, never fatal.
async fn publish(
    run: &Run<'_>,
    state: &super::State,
    report: &CycleReport,
    elapsed: Duration,
    health: &mut crate::health::HealthStatus,
) {
    let config = run.config;
    let interrupted = run.shutdown.is_cancelled();
    let error = report.zones.iter().find_map(|zone| zone.error.clone());
    let ok = !interrupted && error.is_none() && report.completeness.failed == 0;
    let assets_seen = match state.asset_count().await {
        Ok(count) => count,
        Err(error) => {
            tracing::warn!(error = %format!("{error:#}"), "Could not count saved assets for the report");
            if let Some(metrics) = run.metrics {
                metrics.record_db_summary_failure();
            }
            0
        }
    };
    let stats = crate::sync_stats::SyncStats {
        assets_seen,
        downloaded: usize::try_from(
            report
                .zones
                .iter()
                .map(|zone| zone.drain.downloaded)
                .sum::<u64>(),
        )
        .unwrap_or(usize::MAX),
        failed: usize::try_from(report.completeness.failed).unwrap_or(usize::MAX),
        elapsed_secs: elapsed.as_secs_f64(),
        interrupted,
        full_enumeration_reason: if report.zones.iter().any(|zone| zone.position_reset) {
            Some(crate::sync_stats::FullEnumerationReason::PositionRejected)
        } else if report.zones.iter().any(|zone| zone.full_listing) {
            Some(crate::sync_stats::FullEnumerationReason::NoStoredToken)
        } else {
            None
        },
        ..crate::sync_stats::SyncStats::default()
    };
    if ok {
        health.record_success();
    } else {
        health.record_failure(error.as_deref().unwrap_or("Some downloads failed"));
    }
    health.write(&config.auth.cookie_directory);
    if let Some(metrics) = run.metrics {
        metrics.update(&stats, health).await;
        metrics.update_db_stats(&report.completeness, assets_seen);
    }
    let status = if interrupted {
        "interrupted"
    } else if ok {
        "success"
    } else {
        "partial_failure"
    };
    if let Some(path) = &config.report.json {
        let failed = state
            .list_files(&[super::state::FileState::Failed])
            .await
            .unwrap_or_default();
        write_report(config, path, &stats, status, &failed).await;
    }
    run.notifier.notify(
        if ok {
            crate::notifications::Event::SyncComplete
        } else {
            crate::notifications::Event::SyncFailed
        },
        if ok {
            "Sync cycle completed"
        } else {
            "Sync cycle finished with failures"
        },
        &config.auth.username,
        Some(&crate::notifications::SyncNotificationData::from(&stats)),
        config.report.json.as_deref(),
    );
    crate::support::complete(&stats, status);
}

/// Plan a cycle against a scratch copy of the state and report what a sync
/// would download. Real state and media are not changed.
#[allow(
    clippy::print_stdout,
    reason = "--only-print-filenames prints the planned files to stdout"
)]
async fn preview(run: &Run<'_>, location: &super::access::Location) -> anyhow::Result<()> {
    let config = run.config;
    let scratch = location.current.with_extension("preview.db");
    super::upgrade::remove_sqlite_files(&scratch).await?;
    if location.current.exists() {
        let state = super::access::open(location, run.owner, Some(&config.download.directory))
            .await?
            .ok_or_else(|| anyhow::anyhow!("Sync state disappeared"))?;
        state.copy_to(&scratch).await?;
        state.close().await?;
    } else if location.legacy.exists() {
        super::upgrade::prepare(
            &location.legacy,
            &scratch,
            run.owner,
            &config.download.directory,
        )
        .await?;
    }
    let state = super::State::open(&scratch).await?;
    let mut core = core_config(config, run.retry);
    core.transfer = super::Transfer::None;
    let client = run.session.read().await.download_client().clone();
    let result = run_cycle(&state, &run.libraries, &core, &client, run.shutdown).await;
    let owed = match &result {
        Ok(_) => state.list_files(&[super::state::FileState::Pending]).await,
        Err(_) => Ok(Vec::new()),
    };
    state.close().await?;
    super::upgrade::remove_sqlite_files(&scratch).await?;
    let report = result?;
    let owed: Vec<_> = owed?.into_iter().filter(|file| file.wanted).collect();
    if config.runtime.only_print_filenames {
        for file in &owed {
            println!("{}", config.download.directory.join(&file.path).display());
        }
    } else {
        log_report(&report);
        tracing::info!(
            would_download = owed.len(),
            "Dry run: no files or state were changed"
        );
    }
    Ok(())
}

/// Write the JSON report for one cycle. Failures are logged, never fatal.
async fn write_report(
    config: &Config,
    path: &std::path::Path,
    stats: &crate::sync_stats::SyncStats,
    status: &str,
    failed: &[super::state::ListedFile],
) {
    let shown = failed.len().min(crate::report::FAILED_ASSETS_CAP);
    let document = crate::report::SyncReport {
        version: "5",
        kei_version: env!("CARGO_PKG_VERSION"),
        timestamp: chrono::Utc::now().to_rfc3339(),
        status: status.to_owned(),
        options: crate::report::RunOptions::from_config(config),
        stats: stats.clone(),
        failed_assets: failed
            .iter()
            .take(shown)
            .map(crate::report::FailedAssetEntry::from_listed)
            .collect(),
        failed_assets_truncated: failed.len() - shown,
    };
    if let Err(error) = crate::report::write_report(path, &document).await {
        tracing::warn!(error = %format!("{error:#}"), "Could not write the sync report");
    }
}

/// Debug-only seam for the offline binary-boundary report test: write a
/// fixed report and exit before authentication. Release builds never skip
/// authentication from an environment variable.
#[cfg(debug_assertions)]
pub(crate) async fn maybe_write_offline_fake_report(config: &Config) -> anyhow::Result<bool> {
    const ENV: &str = "KEI_UNSTABLE_FAKE_SYNC_REPORT_FOR_TESTS";
    if std::env::var(ENV).as_deref() != Ok("1") {
        return Ok(false);
    }
    let Some(path) = config.report.json.as_deref() else {
        anyhow::bail!("{ENV}=1 requires [report].json");
    };
    tracing::warn!(
        env = ENV,
        "Offline fake sync report test seam enabled; exiting before authentication"
    );
    crate::support::begin(crate::support::runtime_configuration(config));
    let stats = crate::sync_stats::SyncStats {
        assets_seen: 3,
        downloaded: 2,
        bytes_downloaded: 4096,
        elapsed_secs: 0.125,
        ..crate::sync_stats::SyncStats::default()
    };
    write_report(config, path, &stats, "success", &[]).await;
    crate::support::complete(&stats, "success");
    anyhow::ensure!(
        path.is_file(),
        "offline fake sync did not write {}",
        path.display()
    );
    Ok(true)
}

fn core_config(config: &Config, retry: RetryConfig) -> CoreConfig {
    let directory: PathBuf = config.download.directory.clone();
    let paths = PathDerivationConfig {
        directory: std::sync::Arc::from(directory.as_path()),
        folder_structure: config.download.folder_structure.clone(),
        folder_structure_albums: config.download.folder_structure_albums.as_str().into(),
        folder_structure_smart_folders: config
            .download
            .folder_structure_smart_folders
            .as_str()
            .into(),
        media: config.filters.media,
        skip_created_before: config.filters.skip_created_before,
        skip_created_after: config.filters.skip_created_after,
        live_photo_mode: config.photos.live_photo_mode,
        live_photo_mov_filename_policy: config.photos.live_photo_mov_filename_policy,
        edited_naming: config.photos.edited_naming,
        raw_policy: config.photos.raw_policy,
        file_match_policy: config.photos.file_match_policy,
        keep_unicode_in_filenames: config.photos.keep_unicode_in_filenames,
        album_name: None,
        library: PRIMARY_ZONE_NAME.into(),
        exclude_asset_ids: std::sync::Arc::default(),
    };
    CoreConfig {
        directory,
        paths,
        albums: config.filters.selection.albums.clone(),
        smart_folders: config.filters.selection.smart_folders.clone(),
        unfiled: config.filters.selection.unfiled,
        temp_suffix: config.download.temp_suffix.clone(),
        concurrency: usize::from(config.download.threads_num.max(1)),
        retry,
        transfer: super::Transfer::All,
        bandwidth: config
            .download
            .bandwidth_limit
            .map(crate::download::BandwidthLimiter::new),
    }
}

fn log_report(report: &CycleReport) {
    for zone in &report.zones {
        tracing::info!(
            zone = %zone.zone,
            pages = zone.pages,
            caught_up = zone.caught_up,
            listed_again = zone.position_reset,
            planned = zone.plan.planned,
            downloaded = zone.drain.downloaded,
            adopted = zone.drain.adopted,
            failed = zone.drain.failed,
            locally_modified = zone.drain.modified,
            "Library synced"
        );
    }
    if report.unselected_shared > 0 {
        tracing::warn!(
            photos = report.unselected_shared,
            "Some photos were moved to a Shared Library that is not selected, so they are not \
             backed up. Add it with [filters] libraries = [\"all\"] in the config file."
        );
    }
    let c = report.completeness;
    tracing::info!(
        saved = c.done,
        owed = c.pending,
        failed = c.failed,
        locally_modified = c.modified,
        complete = report.complete(),
        "Backup status"
    );
}
