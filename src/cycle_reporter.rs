//! Post-cycle reporting boundary for sync/watch cycles.
//!
//! The sync loop owns CloudKit/auth/download orchestration. This module owns
//! the side effects that happen after a cycle has produced facts: friendly
//! narration, health state, Prometheus metrics, JSON reports, and completion
//! notifications.

use std::path::Path;
use std::time::Duration;

use crate::download::SyncStats;
use crate::health::HealthStatus;
use crate::metrics::MetricsHandle;
use crate::notifications::{self, Notifier};
use crate::personality::Mode;
use crate::report::{self, RunOptions};
use crate::state::{self, ReportStateStore};

/// Stable dependencies and resolved options used by the post-cycle reporter.
pub(crate) struct CycleReporter<'a, D: ReportStateStore + ?Sized> {
    watch_mode: bool,
    report_path: Option<&'a Path>,
    run_options: RunOptions,
    health_dir: &'a Path,
    personality_mode: Mode,
    state_db: Option<&'a D>,
    metrics_handle: Option<&'a MetricsHandle>,
    notifier: &'a Notifier,
}

impl<D> std::fmt::Debug for CycleReporter<'_, D>
where
    D: ReportStateStore + ?Sized,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CycleReporter")
            .field("watch_mode", &self.watch_mode)
            .field("report_path", &self.report_path)
            .field("run_options", &self.run_options)
            .field("health_dir", &self.health_dir)
            .field("personality_mode", &self.personality_mode)
            .field("has_state_db", &self.state_db.is_some())
            .field("has_metrics_handle", &self.metrics_handle.is_some())
            .field("notifier", &self.notifier)
            .finish()
    }
}

/// Constructor input for [`CycleReporter`].
pub(crate) struct CycleReporterConfig<'a, D: ReportStateStore + ?Sized> {
    pub(crate) watch_mode: bool,
    pub(crate) report_path: Option<&'a Path>,
    pub(crate) run_options: RunOptions,
    pub(crate) health_dir: &'a Path,
    pub(crate) personality_mode: Mode,
    pub(crate) state_db: Option<&'a D>,
    pub(crate) metrics_handle: Option<&'a MetricsHandle>,
    pub(crate) notifier: &'a Notifier,
}

impl<D> std::fmt::Debug for CycleReporterConfig<'_, D>
where
    D: ReportStateStore + ?Sized,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CycleReporterConfig")
            .field("watch_mode", &self.watch_mode)
            .field("report_path", &self.report_path)
            .field("run_options", &self.run_options)
            .field("health_dir", &self.health_dir)
            .field("personality_mode", &self.personality_mode)
            .field("has_state_db", &self.state_db.is_some())
            .field("has_metrics_handle", &self.metrics_handle.is_some())
            .field("notifier", &self.notifier)
            .finish()
    }
}

/// Canonical facts produced by one sync cycle that reporters render or persist.
#[derive(Debug)]
pub(crate) struct CycleFacts<'a> {
    pub(crate) stats: &'a SyncStats,
    pub(crate) failed_count: usize,
    pub(crate) elapsed: Duration,
    pub(crate) status: CycleStatus,
}

impl<'a> CycleFacts<'a> {
    pub(crate) fn new(
        stats: &'a SyncStats,
        failed_count: usize,
        session_expired: bool,
        elapsed: Duration,
    ) -> Self {
        Self {
            stats,
            failed_count,
            elapsed,
            status: classify_cycle(stats, failed_count, session_expired),
        }
    }
}

/// Post-cycle outcome shared by report JSON, health, metrics, friendly output,
/// and notification rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CycleStatus {
    Success,
    Failed,
    SessionExpired,
    Interrupted,
}

impl CycleStatus {
    pub(crate) const fn as_report_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failed => "partial_failure",
            Self::SessionExpired => "session_expired",
            Self::Interrupted => "interrupted",
        }
    }
}

/// Classify the completed sync cycle once for every output surface.
///
/// Priority: `session_expired` > `interrupted` > `partial_failure` > `success`.
///
/// Session expiration is the actionable operator signal even if some downloads
/// also failed. Interruption beats partial failure because cancellation can
/// leave transient per-asset failures from in-flight work.
pub(crate) fn classify_cycle(
    stats: &SyncStats,
    failed_count: usize,
    session_expired: bool,
) -> CycleStatus {
    if session_expired {
        CycleStatus::SessionExpired
    } else if stats.interrupted {
        CycleStatus::Interrupted
    } else if failed_count > 0 || stats.identity_incomplete {
        CycleStatus::Failed
    } else {
        CycleStatus::Success
    }
}

impl<'a, D> CycleReporter<'a, D>
where
    D: ReportStateStore + ?Sized,
{
    pub(crate) fn new(config: CycleReporterConfig<'a, D>) -> Self {
        Self {
            watch_mode: config.watch_mode,
            report_path: config.report_path,
            run_options: config.run_options,
            health_dir: config.health_dir,
            personality_mode: config.personality_mode,
            state_db: config.state_db,
            metrics_handle: config.metrics_handle,
            notifier: config.notifier,
        }
    }

    /// Report a skipped watch cycle where `changes/database` found no selected
    /// library changes. This is intentionally health-only: Prometheus health
    /// gauges and `health.json` need fresh timestamps, but cycle counters,
    /// duration, reports, and completion notifications must not change.
    pub(crate) async fn report_skipped_watch_cycle(&self, health: &mut HealthStatus) {
        let unresolved = if let Some(db) = self.state_db {
            db.get_summary()
                .await
                .map(|summary| summary.unresolved_identity_zones > 0)
        } else {
            Ok(false)
        };
        match unresolved {
            Ok(false) => health.record_success(),
            Ok(true) => health
                .record_failure("unresolved asset identity; provider checkpoint replay required"),
            Err(error) => {
                tracing::warn!(error = %error, "Could not inspect durable identity evidence for idle health");
                health.record_failure("could not inspect unresolved asset identity evidence");
            }
        }
        health.write(self.health_dir);
        if let Some(handle) = self.metrics_handle {
            handle.update_health_only(health).await;
        }
    }

    /// Run all post-cycle reporting side effects for a completed sync cycle.
    pub(crate) async fn report_completed_cycle(
        &self,
        health: &mut HealthStatus,
        input: CycleFacts<'_>,
    ) {
        let friendly_summary = self.friendly_summary().await;
        self.report_friendly_output(&input, friendly_summary.as_ref());
        self.update_health(health, &input);
        self.update_metrics(health, &input, friendly_summary.as_ref())
            .await;
        self.write_json_report(&input).await;
        self.notify_cycle_outcome(&input);
    }

    async fn friendly_summary(&self) -> Option<state::types::SyncSummary> {
        if !self.personality_mode.is_friendly() {
            return None;
        }
        let db = self.state_db?;
        match db.get_summary().await {
            Ok(summary) => Some(summary),
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    "post-cycle summary unavailable; rendering friendly summary without library totals"
                );
                None
            }
        }
    }

    fn report_friendly_output(
        &self,
        input: &CycleFacts<'_>,
        library_after_summary: Option<&state::types::SyncSummary>,
    ) {
        let downloaded_u64 = u64::try_from(input.stats.downloaded).unwrap_or(u64::MAX);
        let skipped_total = input
            .stats
            .skipped
            .total()
            .saturating_sub(input.stats.skipped.duplicates);
        let library_totals =
            library_after_summary.map(|summary| crate::personality::summary::LibraryTotals {
                files: summary.downloaded,
                bytes: summary.downloaded_bytes,
            });
        let summary = crate::personality::summary::FinalSummary {
            downloaded: downloaded_u64,
            skipped_total: u64::try_from(skipped_total).unwrap_or(u64::MAX),
            failed: u64::try_from(input.failed_count).unwrap_or(u64::MAX),
            elapsed: input.elapsed,
            library_totals,
        };
        summary.print_to_stderr(self.personality_mode);
    }

    fn update_health(&self, health: &mut HealthStatus, input: &CycleFacts<'_>) {
        match input.status {
            CycleStatus::Success => health.record_success(),
            CycleStatus::Failed if input.stats.identity_incomplete => {
                health.record_failure(
                    "unresolved asset identity; provider checkpoint replay required",
                );
            }
            CycleStatus::Failed => {
                health.record_failure(&format!("{} sync failures", input.failed_count));
            }
            CycleStatus::SessionExpired => health.record_failure("session expired"),
            CycleStatus::Interrupted => health.record_failure("sync interrupted"),
        }
        health.write(self.health_dir);
    }

    async fn update_metrics(
        &self,
        health: &HealthStatus,
        input: &CycleFacts<'_>,
        friendly_summary: Option<&state::types::SyncSummary>,
    ) {
        let Some(handle) = self.metrics_handle else {
            return;
        };
        if input.status == CycleStatus::SessionExpired {
            handle.record_session_expiration();
        }
        handle.update(input.stats, health).await;

        if let Some(summary) = friendly_summary {
            handle.update_db_stats(summary, input.stats.assets_seen);
        } else if let Some(db) = self.state_db {
            match db.get_summary().await {
                Ok(summary) => {
                    handle.update_db_stats(&summary, input.stats.assets_seen);
                }
                Err(e) => {
                    handle.record_db_summary_failure();
                    tracing::warn!(
                        error = %e,
                        "Failed to fetch DB summary for metrics; skipping DB gauge update"
                    );
                }
            }
        }
    }

    async fn write_json_report(&self, input: &CycleFacts<'_>) {
        let Some(report_path) = self.report_path else {
            return;
        };
        let (failed_assets, failed_assets_truncated) = self.failed_asset_sample().await;
        let report = report::SyncReport {
            version: "3",
            kei_version: env!("CARGO_PKG_VERSION"),
            timestamp: chrono::Utc::now().to_rfc3339(),
            status: input.status.as_report_str().to_string(),
            options: self.run_options.clone(),
            stats: input.stats.clone(),
            failed_assets,
            failed_assets_truncated,
        };
        if let Err(e) = report::write_report(report_path, &report).await {
            tracing::warn!(
                error = %e,
                path = %report_path.display(),
                "Failed to write JSON report"
            );
        }
    }

    async fn failed_asset_sample(&self) -> (Vec<report::FailedAssetEntry>, usize) {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "FAILED_ASSETS_CAP is a small compile-time constant well under u32::MAX"
        )]
        let cap_u32 = report::FAILED_ASSETS_CAP as u32;
        match self.state_db {
            Some(db) => match db.get_failed_sample(cap_u32).await {
                Ok((records, total)) => {
                    #[allow(
                        clippy::cast_possible_truncation,
                        reason = "failed-asset totals are persisted counts of per-sync failures, comfortably below usize::MAX on 64-bit"
                    )]
                    let total_usize = total as usize;
                    let truncated = total_usize.saturating_sub(report::FAILED_ASSETS_CAP);
                    let entries = records
                        .iter()
                        .map(report::FailedAssetEntry::from_record)
                        .collect();
                    (entries, truncated)
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "Failed to load failed_assets for sync_report.json"
                    );
                    (Vec::new(), 0)
                }
            },
            None => (Vec::new(), 0),
        }
    }

    fn notify_cycle_outcome(&self, input: &CycleFacts<'_>) {
        let notification_data = notifications::SyncNotificationData::from(input.stats);
        match input.status {
            CycleStatus::Success => self.notifier.notify(
                notifications::Event::SyncComplete,
                "Sync completed successfully",
                &self.run_options.username,
                Some(&notification_data),
                self.report_path,
            ),
            CycleStatus::Failed => self.notifier.notify(
                notifications::Event::SyncFailed,
                &format!("{} sync failures", input.failed_count),
                &self.run_options.username,
                Some(&notification_data),
                self.report_path,
            ),
            CycleStatus::Interrupted => self.notifier.notify(
                notifications::Event::SyncFailed,
                "Sync interrupted",
                &self.run_options.username,
                Some(&notification_data),
                self.report_path,
            ),
            CycleStatus::SessionExpired => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_options() -> RunOptions {
        let download_dir = tempfile::tempdir().unwrap();
        let globals = crate::config::GlobalArgs {
            username: Some("reporter@example.com".to_string()),
            domain: None,
            data_dir: None,
        };
        let sync = crate::cli::SyncArgs {
            config_overrides: crate::config::SyncConfigOverrides {
                download_dir: Some(download_dir.path().display().to_string()),
                ..Default::default()
            },
            ..crate::cli::SyncArgs::default()
        };
        let config = crate::config::Config::build(
            &globals,
            &crate::cli::PasswordArgs::default(),
            sync,
            None,
        )
        .unwrap();
        RunOptions::from_config(&config)
    }

    fn reporter_with_surfaces<'a>(
        dir: &'a Path,
        report_path: Option<&'a Path>,
        notifier: &'a Notifier,
        state_db: Option<&'a state::SqliteStateDb>,
        metrics_handle: Option<&'a MetricsHandle>,
    ) -> CycleReporter<'a, state::SqliteStateDb> {
        CycleReporter::new(CycleReporterConfig {
            watch_mode: false,
            report_path,
            run_options: run_options(),
            health_dir: dir,
            personality_mode: Mode::Off,
            state_db,
            metrics_handle,
            notifier,
        })
    }

    fn reporter<'a>(
        dir: &'a Path,
        report_path: Option<&'a Path>,
        notifier: &'a Notifier,
    ) -> CycleReporter<'a, state::SqliteStateDb> {
        reporter_with_surfaces(dir, report_path, notifier, None, None)
    }

    fn reporter_with_db<'a>(
        dir: &'a Path,
        report_path: Option<&'a Path>,
        notifier: &'a Notifier,
        db: &'a state::SqliteStateDb,
    ) -> CycleReporter<'a, state::SqliteStateDb> {
        reporter_with_surfaces(dir, report_path, notifier, Some(db), None)
    }

    #[cfg(unix)]
    fn reporter_with_db_and_metrics<'a>(
        dir: &'a Path,
        report_path: Option<&'a Path>,
        notifier: &'a Notifier,
        db: &'a state::SqliteStateDb,
        metrics_handle: &'a MetricsHandle,
    ) -> CycleReporter<'a, state::SqliteStateDb> {
        reporter_with_surfaces(dir, report_path, notifier, Some(db), Some(metrics_handle))
    }

    fn parse_json(path: &Path) -> serde_json::Value {
        let contents = std::fs::read_to_string(path).unwrap();
        serde_json::from_str(&contents).unwrap()
    }

    #[tokio::test]
    async fn unresolved_identity_does_not_advance_health_success() {
        let dir = tempfile::tempdir().unwrap();
        let notifier = Notifier::new(None);
        let reporter = reporter_with_surfaces(dir.path(), None, &notifier, None, None);
        let mut health = HealthStatus::new();
        health.record_success();
        let previous = health.last_success_at;
        let mut stats = SyncStats {
            identity_incomplete: true,
            sync_token_blocked: true,
            ..SyncStats::default()
        };
        report_cycle(&reporter, &mut health, &stats, 0, false).await;
        assert_eq!(health.last_success_at, previous);
        assert_eq!(health.consecutive_failures, 1);
        assert!(
            health
                .last_error
                .as_deref()
                .unwrap()
                .contains("unresolved asset identity")
        );
        // Intentional bounded holds remain successful.
        stats.identity_incomplete = false;
        report_cycle(&reporter, &mut health, &stats, 0, false).await;
        assert_eq!(health.consecutive_failures, 0);
        assert!(health.last_error.is_none());
    }

    #[test]
    fn classify_cycle_zero_assets_no_failures_is_success() {
        assert_eq!(
            classify_cycle(&SyncStats::default(), 0, false),
            CycleStatus::Success
        );
    }

    #[test]
    fn classify_cycle_any_failure_is_failed() {
        assert_eq!(
            classify_cycle(&SyncStats::default(), 1, false),
            CycleStatus::Failed
        );
        assert_eq!(
            classify_cycle(&SyncStats::default(), 999, false),
            CycleStatus::Failed
        );
    }

    #[test]
    fn classify_cycle_session_expired_dominates_failure_count() {
        assert_eq!(
            classify_cycle(&SyncStats::default(), 42, true),
            CycleStatus::SessionExpired
        );
    }

    #[test]
    fn classify_cycle_interrupted_beats_partial_failure() {
        let stats = SyncStats {
            interrupted: true,
            ..SyncStats::default()
        };
        assert_eq!(classify_cycle(&stats, 5, false), CycleStatus::Interrupted);
    }

    #[test]
    fn classify_cycle_session_expired_beats_interrupted() {
        let stats = SyncStats {
            interrupted: true,
            ..SyncStats::default()
        };
        assert_eq!(classify_cycle(&stats, 7, true), CycleStatus::SessionExpired);
    }

    #[cfg(unix)]
    fn shell_quote(path: &Path) -> String {
        format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
    }

    #[cfg(unix)]
    fn write_notification_capture_script(dir: &Path, output_path: &Path) -> std::path::PathBuf {
        let script_path = dir.join("notify.sh");
        let output_path = shell_quote(output_path);
        let body = format!(
            "#!/bin/sh\nprintf '%s|%s|%s|%s|%s|%s\\n' \"$KEI_EVENT\" \"$KEI_MESSAGE\" \"$KEI_ICLOUD_USERNAME\" \"${{KEI_DOWNLOADED:-}}\" \"${{KEI_FAILED:-}}\" \"${{KEI_REPORT_JSON:-}}\" > {output_path}\n"
        );
        std::fs::write(&script_path, body).unwrap();
        script_path
    }

    #[cfg(unix)]
    async fn wait_for_notification_output(path: &Path) -> String {
        for _ in 0..100 {
            match std::fs::read_to_string(path) {
                Ok(output) if !output.is_empty() => return output,
                Ok(_) | Err(_) => {}
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("notification script did not write {}", path.display());
    }

    fn assert_sync_token_observation_fields(
        stats_json: &serde_json::Value,
        expected_receivers: usize,
        with_token: usize,
        missing: usize,
        blank: usize,
        dropped: usize,
        unique: usize,
    ) {
        assert_eq!(
            stats_json["sync_token_expected_receivers"],
            expected_receivers
        );
        assert_eq!(stats_json["sync_token_receivers_with_token"], with_token);
        assert_eq!(stats_json["sync_token_receivers_missing"], missing);
        assert_eq!(stats_json["sync_token_receivers_blank"], blank);
        assert_eq!(stats_json["sync_token_receivers_dropped"], dropped);
        assert_eq!(stats_json["sync_token_unique_values"], unique);
    }

    async fn report_cycle(
        reporter: &CycleReporter<'_, state::SqliteStateDb>,
        health: &mut HealthStatus,
        stats: &SyncStats,
        failed_count: usize,
        session_expired: bool,
    ) {
        reporter
            .report_completed_cycle(
                health,
                CycleFacts::new(stats, failed_count, session_expired, Duration::from_secs(7)),
            )
            .await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn sync_report_outcome_matrix_keeps_db_metrics_and_notifications_consistent() {
        struct OutcomeCase {
            name: &'static str,
            stats: SyncStats,
            failed_count: usize,
            session_expired: bool,
            expected_report_status: &'static str,
            expected_health_failures: u64,
            expected_health_error: Option<&'static str>,
            expected_notification: &'static str,
            expected_notification_message: &'static str,
            expected_db_status: &'static str,
            expected_db_assets_failed: i64,
            expected_db_enumeration_errors: i64,
            expected_metrics: &'static [&'static str],
        }

        let cases = [
            OutcomeCase {
                name: "clean success",
                stats: SyncStats {
                    assets_seen: 4,
                    downloaded: 2,
                    bytes_downloaded: 2048,
                    ..SyncStats::default()
                },
                failed_count: 0,
                session_expired: false,
                expected_report_status: "success",
                expected_health_failures: 0,
                expected_health_error: None,
                expected_notification: "sync_complete",
                expected_notification_message: "Sync completed successfully",
                expected_db_status: "complete",
                expected_db_assets_failed: 0,
                expected_db_enumeration_errors: 0,
                expected_metrics: &[
                    "kei_sync_downloaded_total 2",
                    "kei_sync_failed_total 0",
                    "kei_sync_enumeration_errors_total 0",
                ],
            },
            OutcomeCase {
                name: "warning-only token-blocked success",
                stats: SyncStats {
                    assets_seen: 1533,
                    pagination_shortfall_warnings: 1,
                    pagination_shortfall_assets: 45,
                    sync_token_blocked: true,
                    sync_token_blocked_reason: Some("pagination_shortfall"),
                    sync_token_blocked_source: Some("icloud"),
                    sync_token_blocked_explanation: Some(
                        crate::download::sync_token_blocked_explanation("pagination_shortfall"),
                    ),
                    sync_token_blocked_zone: Some("PrimarySync".to_string()),
                    full_enumeration_reason: Some(
                        crate::download::FullEnumerationReason::NoStoredToken,
                    ),
                    ..SyncStats::default()
                },
                failed_count: 0,
                session_expired: false,
                expected_report_status: "success",
                expected_health_failures: 0,
                expected_health_error: None,
                expected_notification: "sync_complete",
                expected_notification_message: "Sync completed successfully",
                expected_db_status: "complete",
                expected_db_assets_failed: 0,
                expected_db_enumeration_errors: 0,
                expected_metrics: &[
                    "kei_sync_failed_total 0",
                    "kei_sync_enumeration_errors_total 0",
                    "kei_sync_pagination_shortfall_warnings_total 1",
                    "kei_sync_token_blocked_cycles_total 1",
                    "kei_sync_full_enumeration_reason_total{reason=\"no_stored_token\"} 1",
                ],
            },
            OutcomeCase {
                name: "sync-token blocked without transfer failure",
                stats: SyncStats {
                    assets_seen: 10,
                    sync_token_blocked: true,
                    sync_token_blocked_reason: Some("icloud_blank_sync_token"),
                    sync_token_blocked_source: Some("icloud"),
                    sync_token_blocked_explanation: Some(
                        crate::download::sync_token_blocked_explanation("icloud_blank_sync_token"),
                    ),
                    sync_token_blocked_zone: Some("PrimarySync".to_string()),
                    sync_token_expected_receivers: Some(1),
                    sync_token_receivers_with_token: Some(0),
                    sync_token_receivers_missing: Some(0),
                    sync_token_receivers_blank: Some(1),
                    sync_token_receivers_dropped: Some(0),
                    sync_token_unique_values: Some(0),
                    ..SyncStats::default()
                },
                failed_count: 0,
                session_expired: false,
                expected_report_status: "success",
                expected_health_failures: 0,
                expected_health_error: None,
                expected_notification: "sync_complete",
                expected_notification_message: "Sync completed successfully",
                expected_db_status: "complete",
                expected_db_assets_failed: 0,
                expected_db_enumeration_errors: 0,
                expected_metrics: &[
                    "kei_sync_failed_total 0",
                    "kei_sync_enumeration_errors_total 0",
                    "kei_sync_token_blocked_cycles_total 1",
                ],
            },
            OutcomeCase {
                name: "enumeration partial failure",
                stats: SyncStats {
                    assets_seen: 8,
                    enumeration_errors: 2,
                    ..SyncStats::default()
                },
                failed_count: 2,
                session_expired: false,
                expected_report_status: "partial_failure",
                expected_health_failures: 1,
                expected_health_error: Some("2 sync failures"),
                expected_notification: "sync_failed",
                expected_notification_message: "2 sync failures",
                expected_db_status: "complete",
                expected_db_assets_failed: 0,
                expected_db_enumeration_errors: 2,
                expected_metrics: &[
                    "kei_sync_failed_total 0",
                    "kei_sync_enumeration_errors_total 2",
                ],
            },
            OutcomeCase {
                name: "real download failure",
                stats: SyncStats {
                    assets_seen: 12,
                    failed: 3,
                    ..SyncStats::default()
                },
                failed_count: 3,
                session_expired: false,
                expected_report_status: "partial_failure",
                expected_health_failures: 1,
                expected_health_error: Some("3 sync failures"),
                expected_notification: "sync_failed",
                expected_notification_message: "3 sync failures",
                expected_db_status: "complete",
                expected_db_assets_failed: 3,
                expected_db_enumeration_errors: 0,
                expected_metrics: &[
                    "kei_sync_failed_total 3",
                    "kei_sync_enumeration_errors_total 0",
                ],
            },
        ];

        for case in cases {
            let dir = tempfile::tempdir().unwrap();
            let report_path = dir.path().join("sync_report.json");
            let notification_output = dir.path().join("notification.txt");
            let script_path = write_notification_capture_script(dir.path(), &notification_output);
            let notifier = Notifier::new(Some(script_path));
            let db = state::SqliteStateDb::open_in_memory().unwrap();
            let metrics_handle = MetricsHandle::new(None);
            let reporter = reporter_with_db_and_metrics(
                dir.path(),
                Some(&report_path),
                &notifier,
                &db,
                &metrics_handle,
            );
            let mut health = HealthStatus::new();

            let run_id = db.start_sync_run().await.unwrap();
            db.complete_sync_run(
                run_id,
                &state::SyncRunStats {
                    assets_seen: case.stats.assets_seen,
                    assets_downloaded: u64::try_from(case.stats.downloaded).unwrap(),
                    assets_failed: u64::try_from(case.stats.failed).unwrap(),
                    enumeration_errors: u64::try_from(case.stats.enumeration_errors).unwrap(),
                    interrupted: case.stats.interrupted,
                    api_total_at_start: case.stats.api_total_at_start,
                    api_total_at_start_partial: case.stats.api_total_at_start_partial,
                    inventory_drop_warnings: u64::try_from(case.stats.inventory_drop_warnings)
                        .unwrap(),
                    inventory_drop_previous_total: case.stats.inventory_drop_previous_total,
                    inventory_drop_current_total: case.stats.inventory_drop_current_total,
                    inventory_drop_library: case.stats.inventory_drop_library.clone(),
                },
            )
            .await
            .unwrap();

            report_cycle(
                &reporter,
                &mut health,
                &case.stats,
                case.failed_count,
                case.session_expired,
            )
            .await;

            let report_json = parse_json(&report_path);
            assert_eq!(
                report_json["status"], case.expected_report_status,
                "{} report status",
                case.name
            );
            assert_eq!(
                report_json["stats"]["failed"], case.stats.failed,
                "{} JSON failed counter",
                case.name
            );
            assert_eq!(
                report_json["stats"]["enumeration_errors"], case.stats.enumeration_errors,
                "{} JSON enumeration_errors counter",
                case.name
            );
            assert_eq!(
                report_json["stats"]["pagination_shortfall_warnings"],
                case.stats.pagination_shortfall_warnings,
                "{} JSON shortfall warnings",
                case.name
            );
            assert_eq!(
                report_json["stats"]["sync_token_blocked"], case.stats.sync_token_blocked,
                "{} JSON token-blocked flag",
                case.name
            );
            if let Some(reason) = case.stats.sync_token_blocked_reason {
                assert_eq!(
                    report_json["stats"]["sync_token_blocked_reason"], reason,
                    "{} JSON token-blocked reason",
                    case.name
                );
            } else {
                assert!(
                    report_json["stats"]["sync_token_blocked_reason"].is_null(),
                    "{} JSON token-blocked reason should be absent",
                    case.name
                );
            }
            if let Some(reason) = case.stats.full_enumeration_reason {
                assert_eq!(
                    report_json["stats"]["full_enumeration_reason"],
                    reason.as_str(),
                    "{} JSON full-enumeration reason",
                    case.name
                );
            } else {
                assert!(
                    report_json["stats"]["full_enumeration_reason"].is_null(),
                    "{} JSON full-enumeration reason should be absent",
                    case.name
                );
            }

            let health_json = parse_json(&dir.path().join("health.json"));
            assert_eq!(
                health_json["consecutive_failures"], case.expected_health_failures,
                "{} health failure count",
                case.name
            );
            match case.expected_health_error {
                Some(expected) => assert_eq!(
                    health_json["last_error"], expected,
                    "{} health last_error",
                    case.name
                ),
                None => assert!(
                    health_json["last_error"].is_null(),
                    "{} health last_error should be null",
                    case.name
                ),
            }

            let (status, assets_seen, assets_failed, enumeration_errors, interrupted) =
                db.sync_run_snapshot_for_test(run_id).unwrap();
            assert_eq!(status, case.expected_db_status, "{} DB status", case.name);
            assert_eq!(
                assets_seen,
                i64::try_from(case.stats.assets_seen).unwrap(),
                "{} DB assets_seen",
                case.name
            );
            assert_eq!(
                assets_failed, case.expected_db_assets_failed,
                "{} DB assets_failed",
                case.name
            );
            assert_eq!(
                enumeration_errors, case.expected_db_enumeration_errors,
                "{} DB enumeration_errors",
                case.name
            );
            assert_eq!(interrupted, 0, "{} DB interrupted flag", case.name);

            let metrics = crate::metrics::render_metrics_for_test(&metrics_handle).await;
            for expected in case.expected_metrics {
                assert!(
                    metrics.contains(expected),
                    "{} metrics missing {expected}; output:\n{metrics}",
                    case.name
                );
            }

            let notification = wait_for_notification_output(&notification_output).await;
            let expected_notification = format!(
                "{}|{}|reporter@example.com|{}|{}|{}",
                case.expected_notification,
                case.expected_notification_message,
                case.stats.downloaded,
                case.stats.failed,
                report_path.display()
            );
            assert_eq!(
                notification.trim(),
                expected_notification,
                "{} notification env",
                case.name
            );
        }
    }

    #[tokio::test]
    async fn success_updates_health_and_writes_success_report() {
        let dir = tempfile::tempdir().unwrap();
        let report_path = dir.path().join("sync_report.json");
        let notifier = Notifier::new(None);
        let reporter = reporter(dir.path(), Some(&report_path), &notifier);
        let mut health = HealthStatus::new();
        let stats = SyncStats {
            downloaded: 2,
            bytes_downloaded: 1024,
            ..SyncStats::default()
        };

        report_cycle(&reporter, &mut health, &stats, 0, false).await;

        let health_json = parse_json(&dir.path().join("health.json"));
        assert_eq!(health_json["consecutive_failures"], 0);
        assert!(health_json["last_error"].is_null());
        let report_json = parse_json(&report_path);
        assert_eq!(report_json["status"], "success");
        assert_eq!(report_json["stats"]["downloaded"], 2);
    }

    #[tokio::test]
    async fn success_report_can_include_preexisting_failed_assets_sample() {
        let dir = tempfile::tempdir().unwrap();
        let report_path = dir.path().join("sync_report.json");
        let notifier = Notifier::new(None);
        let db = state::SqliteStateDb::open_in_memory().expect("open db");
        let failed_record = state::AssetRecord::new_pending(
            std::sync::Arc::from("PrimarySync"),
            "FAILED_OLD".to_string(),
            crate::state::VersionSizeKey::Original,
            "checksum".to_string(),
            "failed_old.jpg".to_string(),
            chrono::Utc::now(),
            None,
            1_024,
            crate::state::MediaType::Photo,
        );
        db.upsert_seen(&failed_record)
            .await
            .expect("upsert failed row");
        db.mark_failed("PrimarySync", "FAILED_OLD", "original", "old failure")
            .await
            .expect("mark failed");
        let reporter = reporter_with_db(dir.path(), Some(&report_path), &notifier, &db);
        let mut health = HealthStatus::new();
        let stats = SyncStats {
            downloaded: 1,
            failed: 0,
            ..SyncStats::default()
        };

        report_cycle(&reporter, &mut health, &stats, 0, false).await;

        let report_json = parse_json(&report_path);
        assert_eq!(report_json["status"], "success");
        assert_eq!(report_json["stats"]["failed"], 0);
        assert_eq!(report_json["failed_assets"][0]["id"], "FAILED_OLD");
        assert_eq!(
            report_json["failed_assets"][0]["error_message"],
            "old failure"
        );
    }

    #[tokio::test]
    async fn partial_failure_updates_health_and_writes_partial_report() {
        let dir = tempfile::tempdir().unwrap();
        let report_path = dir.path().join("sync_report.json");
        let notifier = Notifier::new(None);
        let reporter = reporter(dir.path(), Some(&report_path), &notifier);
        let mut health = HealthStatus::new();
        let stats = SyncStats {
            failed: 3,
            ..SyncStats::default()
        };

        report_cycle(&reporter, &mut health, &stats, 3, false).await;

        let health_json = parse_json(&dir.path().join("health.json"));
        assert_eq!(health_json["consecutive_failures"], 1);
        assert_eq!(health_json["last_error"], "3 sync failures");
        let report_json = parse_json(&report_path);
        assert_eq!(report_json["status"], "partial_failure");
        assert_eq!(report_json["stats"]["failed"], 3);
    }

    #[tokio::test]
    async fn session_expiry_updates_health_and_writes_session_expired_report() {
        let dir = tempfile::tempdir().unwrap();
        let report_path = dir.path().join("sync_report.json");
        let notifier = Notifier::new(None);
        let reporter = reporter(dir.path(), Some(&report_path), &notifier);
        let mut health = HealthStatus::new();

        report_cycle(&reporter, &mut health, &SyncStats::default(), 0, true).await;

        let health_json = parse_json(&dir.path().join("health.json"));
        assert_eq!(health_json["consecutive_failures"], 1);
        assert_eq!(health_json["last_error"], "session expired");
        let report_json = parse_json(&report_path);
        assert_eq!(report_json["status"], "session_expired");
    }

    #[tokio::test]
    async fn interrupted_cycle_writes_interrupted_report() {
        let dir = tempfile::tempdir().unwrap();
        let report_path = dir.path().join("sync_report.json");
        let notifier = Notifier::new(None);
        let reporter = reporter(dir.path(), Some(&report_path), &notifier);
        let mut health = HealthStatus::new();
        let stats = SyncStats {
            interrupted: true,
            ..SyncStats::default()
        };

        report_cycle(&reporter, &mut health, &stats, 0, false).await;

        let health_json = parse_json(&dir.path().join("health.json"));
        assert_eq!(health_json["consecutive_failures"], 1);
        assert_eq!(health_json["last_error"], "sync interrupted");
        let report_json = parse_json(&report_path);
        assert_eq!(report_json["status"], "interrupted");
    }

    #[tokio::test]
    async fn skipped_watch_cycle_retains_unselected_zone_identity_failure() {
        let dir = tempfile::tempdir().unwrap();
        let db = state::SqliteStateDb::open_in_memory().unwrap();
        db.set_metadata(&state::unresolved_identity_key("unselected-zone"), "1")
            .await
            .unwrap();
        let notifier = Notifier::new(None);
        let reporter = reporter_with_surfaces(dir.path(), None, &notifier, Some(&db), None);
        let mut health = HealthStatus::new();
        health.record_success();
        let previous = health.last_success_at;
        reporter.report_skipped_watch_cycle(&mut health).await;
        assert_eq!(health.last_success_at, previous);
        assert_eq!(health.consecutive_failures, 1);
        assert!(
            health
                .last_error
                .as_deref()
                .unwrap()
                .contains("unresolved asset identity")
        );
    }

    #[tokio::test]
    async fn skipped_watch_cycle_is_health_only() {
        let dir = tempfile::tempdir().unwrap();
        let report_path = dir.path().join("sync_report.json");
        let notifier = Notifier::new(None);
        let reporter = reporter(dir.path(), Some(&report_path), &notifier);
        let mut health = HealthStatus::new();

        reporter.report_skipped_watch_cycle(&mut health).await;

        let health_json = parse_json(&dir.path().join("health.json"));
        assert_eq!(health_json["consecutive_failures"], 0);
        assert!(
            !report_path.exists(),
            "skipped cycles should not overwrite sync_report.json"
        );
    }

    #[tokio::test]
    async fn report_write_failure_does_not_block_health_update() {
        let dir = tempfile::tempdir().unwrap();
        let notifier = Notifier::new(None);
        let reporter = reporter(dir.path(), Some(dir.path()), &notifier);
        let mut health = HealthStatus::new();

        report_cycle(&reporter, &mut health, &SyncStats::default(), 0, false).await;

        let health_json = parse_json(&dir.path().join("health.json"));
        assert_eq!(health_json["consecutive_failures"], 0);
    }

    #[tokio::test]
    async fn pagination_shortfall_warning_does_not_mark_cycle_failed() {
        let dir = tempfile::tempdir().unwrap();
        let report_path = dir.path().join("sync_report.json");
        let notifier = Notifier::new(None);
        let reporter = reporter(dir.path(), Some(&report_path), &notifier);
        let mut health = HealthStatus::new();
        let stats = SyncStats {
            failed: 0,
            enumeration_errors: 0,
            pagination_shortfall_warnings: 1,
            pagination_shortfall_assets: 45,
            sync_token_blocked: true,
            sync_token_blocked_reason: Some("pagination_shortfall"),
            ..SyncStats::default()
        };

        report_cycle(&reporter, &mut health, &stats, 0, false).await;

        let health_json = parse_json(&dir.path().join("health.json"));
        assert_eq!(health_json["consecutive_failures"], 0);
        assert!(health_json["last_error"].is_null());

        let report_json = parse_json(&report_path);
        assert_eq!(report_json["status"], "success");
        assert_eq!(report_json["stats"]["failed"], 0);
        assert_eq!(report_json["stats"]["enumeration_errors"], 0);
        assert_eq!(report_json["stats"]["pagination_shortfall_warnings"], 1);
        assert_eq!(report_json["stats"]["pagination_shortfall_assets"], 45);
        assert_eq!(report_json["stats"]["sync_token_blocked"], true);
        assert_eq!(
            report_json["stats"]["sync_token_blocked_reason"],
            "pagination_shortfall"
        );
    }

    #[tokio::test]
    async fn sync_token_blocked_diagnostics_serialize_to_report_json() {
        let dir = tempfile::tempdir().unwrap();
        let report_path = dir.path().join("sync_report.json");
        let notifier = Notifier::new(None);
        let reporter = reporter(dir.path(), Some(&report_path), &notifier);
        let mut health = HealthStatus::new();
        let stats = SyncStats {
            sync_token_blocked: true,
            sync_token_blocked_reason: Some("icloud_sync_token_missing"),
            sync_token_blocked_source: Some("icloud"),
            sync_token_blocked_explanation: Some(
                "iCloud did not return a sync token for this full enumeration",
            ),
            sync_token_blocked_zone: Some("PrimarySync".to_string()),
            sync_token_expected_receivers: Some(3),
            sync_token_receivers_with_token: Some(0),
            sync_token_receivers_missing: Some(3),
            sync_token_receivers_blank: Some(0),
            sync_token_receivers_dropped: Some(0),
            sync_token_unique_values: Some(0),
            ..SyncStats::default()
        };

        report_cycle(&reporter, &mut health, &stats, 0, false).await;

        let report_json = parse_json(&report_path);
        let stats_json = &report_json["stats"];
        assert_eq!(
            stats_json["sync_token_blocked_reason"],
            "icloud_sync_token_missing"
        );
        assert_eq!(stats_json["sync_token_blocked_source"], "icloud");
        assert_eq!(
            stats_json["sync_token_blocked_explanation"],
            "iCloud did not return a sync token for this full enumeration"
        );
        assert_eq!(stats_json["sync_token_blocked_zone"], "PrimarySync");
        assert_sync_token_observation_fields(stats_json, 3, 0, 3, 0, 0, 0);
    }

    #[tokio::test]
    async fn sync_token_observation_fields_serialize_even_when_not_blocked() {
        let dir = tempfile::tempdir().unwrap();
        let report_path = dir.path().join("sync_report.json");
        let notifier = Notifier::new(None);
        let reporter = reporter(dir.path(), Some(&report_path), &notifier);
        let mut health = HealthStatus::new();
        let stats = SyncStats {
            sync_token_blocked: false,
            sync_token_expected_receivers: Some(2),
            sync_token_receivers_with_token: Some(2),
            sync_token_receivers_missing: Some(0),
            sync_token_receivers_blank: Some(0),
            sync_token_receivers_dropped: Some(0),
            sync_token_unique_values: Some(1),
            ..SyncStats::default()
        };

        report_cycle(&reporter, &mut health, &stats, 0, false).await;

        let report_json = parse_json(&report_path);
        let stats_json = &report_json["stats"];
        assert_eq!(stats_json["sync_token_blocked"], false);
        assert_sync_token_observation_fields(stats_json, 2, 2, 0, 0, 0, 1);
        assert!(
            stats_json["sync_token_blocked_reason"].is_null(),
            "reason should stay absent when sync token was not blocked"
        );
    }
}
