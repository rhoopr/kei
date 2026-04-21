//! Prometheus metrics and HTTP observability server.
//!
//! When `--metrics-port` is provided, spawns an axum HTTP server that serves:
//! - `GET /metrics` — Prometheus text format
//! - `GET /healthz`  — JSON health status (same data as `health.json`)
//!
//! Metrics are updated after every sync cycle by calling [`MetricsHandle::update`].
//! On skipped cycles (no changes detected), call [`MetricsHandle::update_health_only`]
//! to refresh the health gauges without clobbering cycle counters or duration.
//! All counters are cumulative across cycles (they never reset while the process
//! is running), matching Prometheus conventions.

use std::net::SocketAddr;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use chrono::Utc;

use axum::extract::State;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use prometheus_client::encoding::text::encode;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::download::SyncStats;
use crate::health::HealthStatus;
use crate::state::types::SyncSummary;

// ── Label types ──────────────────────────────────────────────────────────────

/// Label set for the `kei_sync_skipped_total` counter family.
#[derive(Debug, Clone, Hash, PartialEq, Eq, prometheus_client::encoding::EncodeLabelSet)]
struct SkipLabels {
    reason: &'static str,
}

/// Label set for the `kei_db_assets_total` and `kei_db_assets_size_bytes` gauge families.
#[derive(Clone, Debug, Hash, PartialEq, Eq, prometheus_client::encoding::EncodeLabelSet)]
struct StatusLabels {
    status: &'static str,
}

// ── State shared between the HTTP handlers and the sync loop ─────────────────

/// Health snapshot read by the /healthz handler. The registry is immutable
/// after construction so it lives directly on MetricsHandle behind an Arc,
/// letting /metrics encode without taking the lock.
struct Inner {
    health_snapshot: Option<HealthStatus>,
    /// Maximum age of `last_success_at` before /healthz returns 503. `None`
    /// disables the staleness check (e.g. one-shot syncs where a single
    /// success is final). Set at construction time from `watch_interval * 2`
    /// so a single missed cycle is tolerable but two consecutive misses flip
    /// the endpoint to failing.
    staleness_threshold: Option<chrono::Duration>,
}

/// Cheap-to-clone handle passed to the sync loop and into axum state.
#[derive(Clone)]
pub(crate) struct MetricsHandle {
    /// Prometheus registry — immutable after new(), so no lock needed for reads.
    registry: Arc<Registry>,
    /// Protects the /healthz snapshot only.
    inner: Arc<Mutex<Inner>>,
    // Metric handles use atomics internally; no lock needed for updates.
    assets_seen: Counter,
    downloaded: Counter,
    failed: Counter,
    skipped: Family<SkipLabels, Counter>,
    bytes_downloaded: Counter,
    disk_bytes_written: Counter,
    exif_failures: Counter,
    state_write_failures: Counter,
    enumeration_errors: Counter,
    session_expirations: Counter,
    cycle_duration_seconds: Gauge<f64, AtomicU64>,
    consecutive_failures: Gauge,
    last_success_timestamp: Gauge<f64, AtomicU64>,
    interrupted_cycles: Counter,
    // DB-backed gauges (only populated when --metrics-port is set).
    db_assets_total: Family<StatusLabels, Gauge>,
    db_assets_size_bytes: Family<StatusLabels, Gauge>,
    db_last_sync_assets_seen: Gauge,
    db_summary_read_failures: Counter,
}

impl MetricsHandle {
    /// Build the registry and register all metrics.
    pub(crate) fn new(staleness_threshold: Option<chrono::Duration>) -> Self {
        let mut registry = Registry::default();

        let assets_seen = Counter::default();
        registry.register(
            "kei_sync_assets_seen",
            "Total number of assets enumerated from iCloud across all sync cycles",
            assets_seen.clone(),
        );

        let downloaded = Counter::default();
        registry.register(
            "kei_sync_downloaded",
            "Total number of assets successfully downloaded",
            downloaded.clone(),
        );

        let failed = Counter::default();
        registry.register(
            "kei_sync_failed",
            "Total number of asset download failures",
            failed.clone(),
        );

        let skipped: Family<SkipLabels, Counter> = Family::default();
        registry.register(
            "kei_sync_skipped",
            "Total number of assets skipped, by reason",
            skipped.clone(),
        );

        let bytes_downloaded = Counter::default();
        registry.register(
            "kei_sync_bytes_downloaded",
            "Total bytes received over the network",
            bytes_downloaded.clone(),
        );

        let disk_bytes_written = Counter::default();
        registry.register(
            "kei_sync_disk_bytes_written",
            "Total bytes written to disk",
            disk_bytes_written.clone(),
        );

        let exif_failures = Counter::default();
        registry.register(
            "kei_sync_exif_failures",
            "Total number of EXIF stamping failures",
            exif_failures.clone(),
        );

        let state_write_failures = Counter::default();
        registry.register(
            "kei_sync_state_write_failures",
            "Total number of SQLite state write failures",
            state_write_failures.clone(),
        );

        let enumeration_errors = Counter::default();
        registry.register(
            "kei_sync_enumeration_errors",
            "Total number of iCloud API enumeration errors",
            enumeration_errors.clone(),
        );

        let session_expirations = Counter::default();
        registry.register(
            "kei_sync_session_expirations",
            "Total number of sync cycles aborted due to an expired iCloud session",
            session_expirations.clone(),
        );

        let cycle_duration_seconds: Gauge<f64, AtomicU64> = Gauge::default();
        registry.register(
            "kei_sync_cycle_duration_seconds",
            "Wall-clock duration of the most recent sync cycle in seconds",
            cycle_duration_seconds.clone(),
        );

        let consecutive_failures: Gauge = Gauge::default();
        registry.register(
            "kei_health_consecutive_failures",
            "Number of consecutive sync cycle failures",
            consecutive_failures.clone(),
        );

        let last_success_timestamp: Gauge<f64, AtomicU64> = Gauge::default();
        registry.register(
            "kei_health_last_success_timestamp_seconds",
            "Unix timestamp of the last successful sync cycle (0 if never succeeded)",
            last_success_timestamp.clone(),
        );

        let interrupted_cycles = Counter::default();
        registry.register(
            "kei_sync_interrupted_cycles",
            "Total number of sync cycles interrupted by a shutdown signal",
            interrupted_cycles.clone(),
        );

        let db_assets_total: Family<StatusLabels, Gauge> = Family::default();
        registry.register(
            "kei_db_assets_total",
            "Current number of assets in the database, by status",
            db_assets_total.clone(),
        );

        let db_assets_size_bytes: Family<StatusLabels, Gauge> = Family::default();
        registry.register(
            "kei_db_assets_size_bytes",
            "Current total size in bytes of assets in the database, by status",
            db_assets_size_bytes.clone(),
        );

        let db_last_sync_assets_seen: Gauge = Gauge::default();
        registry.register(
            "kei_db_last_sync_assets_seen",
            "Number of assets enumerated from iCloud in the most recent sync cycle",
            db_last_sync_assets_seen.clone(),
        );

        let db_summary_read_failures = Counter::default();
        registry.register(
            "kei_db_summary_read_failures",
            "Total number of failed attempts to read the DB summary for metrics",
            db_summary_read_failures.clone(),
        );

        Self {
            registry: Arc::new(registry),
            inner: Arc::new(Mutex::new(Inner {
                health_snapshot: None,
                staleness_threshold,
            })),
            assets_seen,
            downloaded,
            failed,
            skipped,
            bytes_downloaded,
            disk_bytes_written,
            exif_failures,
            state_write_failures,
            enumeration_errors,
            session_expirations,
            cycle_duration_seconds,
            consecutive_failures,
            last_success_timestamp,
            interrupted_cycles,
            db_assets_total,
            db_assets_size_bytes,
            db_last_sync_assets_seen,
            db_summary_read_failures,
        }
    }

    /// Update all metrics from the latest completed sync cycle.
    ///
    /// Counters are incremented by this cycle's values; gauges are set to the
    /// latest value. Call this after every cycle that actually ran.
    pub(crate) async fn update(&self, stats: &SyncStats, health: &HealthStatus) {
        // Counters — increment by this cycle's values.
        self.assets_seen.inc_by(stats.assets_seen);
        self.downloaded.inc_by(stats.downloaded as u64);
        self.failed.inc_by(stats.failed as u64);
        self.bytes_downloaded.inc_by(stats.bytes_downloaded);
        self.disk_bytes_written.inc_by(stats.disk_bytes_written);
        self.exif_failures.inc_by(stats.exif_failures as u64);
        self.state_write_failures
            .inc_by(stats.state_write_failures as u64);
        self.enumeration_errors
            .inc_by(stats.enumeration_errors as u64);

        if stats.interrupted {
            self.interrupted_cycles.inc();
        }

        // Skip breakdown counters with reason labels.
        self.inc_skip("by_state", stats.skipped.by_state);
        self.inc_skip("on_disk", stats.skipped.on_disk);
        self.inc_skip("by_media_type", stats.skipped.by_media_type);
        self.inc_skip("by_date_range", stats.skipped.by_date_range);
        self.inc_skip("by_live_photo", stats.skipped.by_live_photo);
        self.inc_skip("by_filename", stats.skipped.by_filename);
        self.inc_skip("by_excluded_album", stats.skipped.by_excluded_album);
        self.inc_skip("ampm_variant", stats.skipped.ampm_variant);
        self.inc_skip("duplicates", stats.skipped.duplicates);
        self.inc_skip("retry_exhausted", stats.skipped.retry_exhausted);
        self.inc_skip("retry_only", stats.skipped.retry_only);

        // Gauges — set to latest value.
        self.cycle_duration_seconds.set(stats.elapsed_secs);

        self.update_health_gauges(health).await;
    }

    /// Update only the health gauges and the /healthz snapshot.
    ///
    /// Use this on skipped cycles (no changes detected in watch mode) so that
    /// `cycle_duration_seconds` and download counters are not clobbered.
    pub(crate) async fn update_health_only(&self, health: &HealthStatus) {
        self.update_health_gauges(health).await;
    }

    /// Increment the session expiration counter.
    ///
    /// Call this whenever a sync cycle is aborted due to an expired iCloud
    /// session, in addition to the normal health update.
    pub(crate) fn record_session_expiration(&self) {
        self.session_expirations.inc();
    }

    /// Update DB-backed gauges from the current state database summary.
    ///
    /// Call this after [`Self::update`] on every real sync cycle, guarded by a
    /// `state_db.as_ref()` check so dry-run and metrics-disabled paths are
    /// no-ops. `assets_seen` comes from [`SyncStats`] already in hand at the
    /// call site, so no extra DB query is needed for that gauge.
    pub(crate) fn update_db_stats(&self, summary: &SyncSummary, assets_seen: u64) {
        self.db_assets_total
            .get_or_create(&StatusLabels {
                status: "downloaded",
            })
            .set(i64::try_from(summary.downloaded).unwrap_or(i64::MAX));
        self.db_assets_total
            .get_or_create(&StatusLabels { status: "pending" })
            .set(i64::try_from(summary.pending).unwrap_or(i64::MAX));
        self.db_assets_total
            .get_or_create(&StatusLabels { status: "failed" })
            .set(i64::try_from(summary.failed).unwrap_or(i64::MAX));
        self.db_assets_size_bytes
            .get_or_create(&StatusLabels {
                status: "downloaded",
            })
            .set(i64::try_from(summary.downloaded_bytes).unwrap_or(i64::MAX));
        self.db_last_sync_assets_seen
            .set(i64::try_from(assets_seen).unwrap_or(i64::MAX));
    }

    /// Increment the counter for failed DB summary reads.
    ///
    /// Call this whenever [`get_summary`](crate::state::StateDb::get_summary)
    /// returns an error during a metrics update so the failure is visible in
    /// the `/metrics` output rather than only in the log.
    pub(crate) fn record_db_summary_failure(&self) {
        self.db_summary_read_failures.inc();
    }

    async fn update_health_gauges(&self, health: &HealthStatus) {
        self.consecutive_failures
            .set(i64::from(health.consecutive_failures));
        let last_success_ts = health
            .last_success_at
            .map(|t| t.timestamp() as f64)
            .unwrap_or(0.0);
        self.last_success_timestamp.set(last_success_ts);

        let mut inner = self.inner.lock().await;
        inner.health_snapshot = Some(HealthStatus {
            last_sync_at: health.last_sync_at,
            last_success_at: health.last_success_at,
            consecutive_failures: health.consecutive_failures,
            last_error: health.last_error.clone(),
        });
    }

    fn inc_skip(&self, reason: &'static str, count: usize) {
        if count > 0 {
            self.skipped
                .get_or_create(&SkipLabels { reason })
                .inc_by(count as u64);
        }
    }
}

// ── HTTP handlers ─────────────────────────────────────────────────────────────

async fn handle_metrics(State(handle): State<MetricsHandle>) -> impl IntoResponse {
    let mut buf = String::new();
    encode(&mut buf, &handle.registry).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "Failed to encode Prometheus metrics");
    });

    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/openmetrics-text; version=1.0.0; charset=utf-8"),
        )],
        buf,
    )
}

async fn handle_healthz(State(handle): State<MetricsHandle>) -> impl IntoResponse {
    let inner = handle.inner.lock().await;
    let staleness_threshold = inner.staleness_threshold;
    match &inner.health_snapshot {
        Some(h) => {
            let consecutive_failures = h.consecutive_failures;
            let stale = match (staleness_threshold, h.last_success_at) {
                (Some(max_age), Some(last_success)) => (Utc::now() - last_success) > max_age,
                _ => false,
            };
            match serde_json::to_string_pretty(h) {
                Ok(json) => {
                    let status = if consecutive_failures >= 5 || stale {
                        StatusCode::SERVICE_UNAVAILABLE
                    } else {
                        StatusCode::OK
                    };
                    (
                        status,
                        [(
                            header::CONTENT_TYPE,
                            HeaderValue::from_static("application/json"),
                        )],
                        json,
                    )
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to serialize health status for /healthz");
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        [(
                            header::CONTENT_TYPE,
                            HeaderValue::from_static("application/json"),
                        )],
                        r#"{"error":"serialization failed"}"#.to_string(),
                    )
                }
            }
        }
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            [(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            )],
            r#"{"status":"no sync cycle completed yet"}"#.to_string(),
        ),
    }
}

// ── Server entrypoint ─────────────────────────────────────────────────────────

/// Bind and spawn the metrics HTTP server as a background tokio task.
///
/// Binds synchronously so that a misconfigured port fails at startup rather
/// than silently. Returns a `MetricsHandle` the sync loop uses to push metrics
/// after each cycle and a `JoinHandle` so the sync loop can await graceful
/// shutdown before the runtime drops. The server shuts down gracefully when
/// `shutdown_token` is cancelled.
pub(crate) fn spawn_server(
    port: u16,
    shutdown_token: CancellationToken,
    staleness_threshold: Option<chrono::Duration>,
) -> anyhow::Result<(MetricsHandle, tokio::task::JoinHandle<()>, SocketAddr)> {
    let handle = MetricsHandle::new(staleness_threshold);
    let app = Router::new()
        .route("/metrics", get(handle_metrics))
        .route("/healthz", get(handle_healthz))
        .with_state(handle.clone());

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let std_listener = std::net::TcpListener::bind(addr)
        .map_err(|e| anyhow::anyhow!("Failed to bind metrics server on port {port}: {e}"))?;
    let local_addr = std_listener.local_addr()?;
    std_listener.set_nonblocking(true)?;
    let listener = tokio::net::TcpListener::from_std(std_listener)?;

    tracing::info!(
        port = local_addr.port(),
        "Prometheus metrics server listening"
    );

    let task = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app)
            .with_graceful_shutdown(async move { shutdown_token.cancelled().await })
            .await
        {
            tracing::warn!(error = %e, "Metrics server error");
        }
        tracing::info!(
            port = local_addr.port(),
            "Prometheus metrics server stopped"
        );
    });

    Ok((handle, task, local_addr))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::State;

    fn healthy_status(consecutive_failures: u32) -> HealthStatus {
        let mut h = HealthStatus::new();
        if consecutive_failures == 0 {
            h.record_success();
        } else {
            for i in 0..consecutive_failures {
                h.record_failure(&format!("error {i}"));
            }
        }
        h
    }

    fn stats_with(downloaded: usize, failed: usize, bytes: u64) -> SyncStats {
        SyncStats {
            downloaded,
            failed,
            bytes_downloaded: bytes,
            ..SyncStats::default()
        }
    }

    async fn render_metrics(handle: &MetricsHandle) -> String {
        let response = handle_metrics(State(handle.clone())).await;
        let body = axum::body::to_bytes(
            axum::response::IntoResponse::into_response(response).into_body(),
            usize::MAX,
        )
        .await
        .unwrap();
        String::from_utf8(body.to_vec()).unwrap()
    }

    async fn render_healthz(handle: &MetricsHandle) -> (axum::http::StatusCode, String) {
        let response = axum::response::IntoResponse::into_response(
            handle_healthz(State(handle.clone())).await,
        );
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }

    // ── /metrics content-type ─────────────────────────────────────────────────

    #[tokio::test]
    async fn metrics_response_has_openmetrics_content_type() {
        let handle = MetricsHandle::new(None);
        let response =
            axum::response::IntoResponse::into_response(handle_metrics(State(handle)).await);
        let content_type = response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            content_type.contains("application/openmetrics-text"),
            "unexpected content-type: {content_type}"
        );
    }

    // ── counter accumulation ──────────────────────────────────────────────────

    #[tokio::test]
    async fn counters_reflect_single_cycle() {
        let handle = MetricsHandle::new(None);
        let stats = stats_with(5, 2, 1024);
        handle.update(&stats, &healthy_status(0)).await;

        let output = render_metrics(&handle).await;
        assert!(
            output.contains("kei_sync_downloaded_total 5"),
            "output:\n{output}"
        );
        assert!(
            output.contains("kei_sync_failed_total 2"),
            "output:\n{output}"
        );
        assert!(
            output.contains("kei_sync_bytes_downloaded_total 1024"),
            "output:\n{output}"
        );
    }

    #[tokio::test]
    async fn counters_accumulate_across_cycles() {
        let handle = MetricsHandle::new(None);
        handle
            .update(&stats_with(3, 1, 500), &healthy_status(0))
            .await;
        handle
            .update(&stats_with(4, 0, 300), &healthy_status(0))
            .await;

        let output = render_metrics(&handle).await;
        assert!(
            output.contains("kei_sync_downloaded_total 7"),
            "output:\n{output}"
        );
        assert!(
            output.contains("kei_sync_failed_total 1"),
            "output:\n{output}"
        );
        assert!(
            output.contains("kei_sync_bytes_downloaded_total 800"),
            "output:\n{output}"
        );
    }

    #[tokio::test]
    async fn gauges_reflect_only_the_latest_cycle() {
        let handle = MetricsHandle::new(None);
        let stats1 = SyncStats {
            elapsed_secs: 10.0,
            ..Default::default()
        };
        handle.update(&stats1, &healthy_status(0)).await;

        let stats2 = SyncStats {
            elapsed_secs: 25.0,
            ..Default::default()
        };
        handle.update(&stats2, &healthy_status(0)).await;

        let output = render_metrics(&handle).await;
        assert!(
            output.contains("kei_sync_cycle_duration_seconds 25"),
            "output:\n{output}"
        );
        assert!(
            !output.contains("kei_sync_cycle_duration_seconds 10"),
            "old gauge value should not appear:\n{output}"
        );
    }

    // ── update_health_only does not clobber cycle duration ────────────────────

    #[tokio::test]
    async fn cycle_duration_not_clobbered_by_health_only_update() {
        let handle = MetricsHandle::new(None);
        let stats = SyncStats {
            elapsed_secs: 25.0,
            ..Default::default()
        };
        handle.update(&stats, &healthy_status(0)).await;

        // Simulate a skipped cycle: should not reset duration to 0.
        handle.update_health_only(&healthy_status(0)).await;

        let output = render_metrics(&handle).await;
        assert!(
            output.contains("kei_sync_cycle_duration_seconds 25"),
            "cycle_duration_seconds should not be clobbered by update_health_only:\n{output}"
        );
    }

    #[tokio::test]
    async fn health_only_update_still_refreshes_health_gauges() {
        let handle = MetricsHandle::new(None);
        // First real cycle with 3 failures.
        handle
            .update(&SyncStats::default(), &healthy_status(3))
            .await;
        // Skipped cycle that resolves to healthy.
        handle.update_health_only(&healthy_status(0)).await;

        let output = render_metrics(&handle).await;
        assert!(
            output.contains("kei_health_consecutive_failures 0"),
            "health gauge should be updated by update_health_only:\n{output}"
        );
    }

    #[tokio::test]
    async fn health_only_update_refreshes_healthz_snapshot() {
        let handle = MetricsHandle::new(None);
        handle.update_health_only(&healthy_status(0)).await;

        let (status, _body) = render_healthz(&handle).await;
        assert_eq!(
            status,
            axum::http::StatusCode::OK,
            "/healthz should return 200 after update_health_only with no failures"
        );
    }

    // ── interrupted counter ───────────────────────────────────────────────────

    #[tokio::test]
    async fn interrupted_flag_increments_counter() {
        let handle = MetricsHandle::new(None);
        let stats = SyncStats {
            interrupted: true,
            ..Default::default()
        };
        handle.update(&stats, &healthy_status(0)).await;

        let output = render_metrics(&handle).await;
        assert!(
            output.contains("kei_sync_interrupted_cycles_total 1"),
            "output:\n{output}"
        );
    }

    #[tokio::test]
    async fn non_interrupted_cycle_does_not_increment_counter() {
        let handle = MetricsHandle::new(None);
        handle
            .update(&SyncStats::default(), &healthy_status(0))
            .await;

        let output = render_metrics(&handle).await;
        assert!(
            !output.contains("kei_sync_interrupted_cycles_total 1"),
            "output:\n{output}"
        );
    }

    // ── session expiration counter ────────────────────────────────────────────

    #[tokio::test]
    async fn session_expiration_counter_increments() {
        let handle = MetricsHandle::new(None);
        handle.record_session_expiration();
        handle.record_session_expiration();

        let output = render_metrics(&handle).await;
        assert!(
            output.contains("kei_sync_session_expirations_total 2"),
            "output:\n{output}"
        );
    }

    // ── skip breakdown labels ─────────────────────────────────────────────────

    #[tokio::test]
    async fn skip_breakdown_emits_labelled_counters() {
        let handle = MetricsHandle::new(None);
        let mut stats = SyncStats::default();
        stats.skipped.by_state = 10;
        stats.skipped.on_disk = 3;
        stats.skipped.retry_exhausted = 1;
        handle.update(&stats, &healthy_status(0)).await;

        let output = render_metrics(&handle).await;
        assert!(
            output.contains(r#"reason="by_state""#) && output.contains("10"),
            "by_state label missing:\n{output}"
        );
        assert!(
            output.contains(r#"reason="on_disk""#) && output.contains("3"),
            "on_disk label missing:\n{output}"
        );
        assert!(
            output.contains(r#"reason="retry_exhausted""#) && output.contains("1"),
            "retry_exhausted label missing:\n{output}"
        );
    }

    #[tokio::test]
    async fn zero_skips_do_not_create_label_series() {
        let handle = MetricsHandle::new(None);
        handle
            .update(&SyncStats::default(), &healthy_status(0))
            .await;

        let output = render_metrics(&handle).await;
        assert!(
            !output.contains(r#"reason="by_state""#),
            "zero-count skip series should not appear:\n{output}"
        );
    }

    // ── health gauges ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn consecutive_failures_gauge_tracks_health() {
        let handle = MetricsHandle::new(None);
        handle
            .update(&SyncStats::default(), &healthy_status(3))
            .await;

        let output = render_metrics(&handle).await;
        assert!(
            output.contains("kei_health_consecutive_failures 3"),
            "output:\n{output}"
        );
    }

    #[tokio::test]
    async fn consecutive_failures_gauge_resets_on_success() {
        let handle = MetricsHandle::new(None);
        handle
            .update(&SyncStats::default(), &healthy_status(3))
            .await;
        handle
            .update(&SyncStats::default(), &healthy_status(0))
            .await;

        let output = render_metrics(&handle).await;
        assert!(
            output.contains("kei_health_consecutive_failures 0"),
            "output:\n{output}"
        );
    }

    // ── /healthz endpoint ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn healthz_returns_503_before_first_cycle() {
        let handle = MetricsHandle::new(None);
        let (status, _body) = render_healthz(&handle).await;
        assert_eq!(status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn healthz_returns_200_after_successful_cycle() {
        let handle = MetricsHandle::new(None);
        handle
            .update(&SyncStats::default(), &healthy_status(0))
            .await;

        let (status, body) = render_healthz(&handle).await;
        assert_eq!(status, axum::http::StatusCode::OK);
        serde_json::from_str::<serde_json::Value>(&body)
            .expect("healthz body should be valid JSON");
    }

    #[tokio::test]
    async fn healthz_returns_503_when_consecutive_failures_reaches_threshold() {
        let handle = MetricsHandle::new(None);
        handle
            .update(&SyncStats::default(), &healthy_status(5))
            .await;

        let (status, _body) = render_healthz(&handle).await;
        assert_eq!(status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn healthz_returns_200_when_consecutive_failures_below_threshold() {
        let handle = MetricsHandle::new(None);
        handle
            .update(&SyncStats::default(), &healthy_status(4))
            .await;

        let (status, _body) = render_healthz(&handle).await;
        assert_eq!(status, axum::http::StatusCode::OK);
    }

    #[tokio::test]
    async fn healthz_body_contains_expected_fields() {
        let handle = MetricsHandle::new(None);
        handle
            .update(&SyncStats::default(), &healthy_status(0))
            .await;

        let (_status, body) = render_healthz(&handle).await;
        let json: serde_json::Value =
            serde_json::from_str(&body).expect("healthz body should be valid JSON");
        assert!(
            json.get("consecutive_failures").is_some(),
            "missing consecutive_failures"
        );
        assert!(json.get("last_sync_at").is_some(), "missing last_sync_at");
        assert!(
            json.get("last_success_at").is_some(),
            "missing last_success_at"
        );
    }

    // ── /healthz staleness threshold ───────────────────────────────────────

    async fn set_threshold(handle: &MetricsHandle, max_age: Option<chrono::Duration>) {
        handle.inner.lock().await.staleness_threshold = max_age;
    }

    async fn backdate_last_success(handle: &MetricsHandle, secs_ago: i64) {
        let mut inner = handle.inner.lock().await;
        if let Some(snap) = inner.health_snapshot.as_mut() {
            let past = Utc::now() - chrono::Duration::seconds(secs_ago);
            snap.last_sync_at = Some(past);
            snap.last_success_at = Some(past);
        }
    }

    #[tokio::test]
    async fn healthz_returns_503_when_last_success_is_stale() {
        let handle = MetricsHandle::new(None);
        handle
            .update(&SyncStats::default(), &healthy_status(0))
            .await;
        // 600s threshold; backdate last_success to 1200s ago
        set_threshold(&handle, Some(chrono::Duration::seconds(600))).await;
        backdate_last_success(&handle, 1200).await;

        let (status, _body) = render_healthz(&handle).await;
        assert_eq!(status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn healthz_returns_200_when_last_success_is_fresh() {
        let handle = MetricsHandle::new(None);
        handle
            .update(&SyncStats::default(), &healthy_status(0))
            .await;
        set_threshold(&handle, Some(chrono::Duration::seconds(600))).await;
        backdate_last_success(&handle, 100).await;

        let (status, _body) = render_healthz(&handle).await;
        assert_eq!(status, axum::http::StatusCode::OK);
    }

    #[tokio::test]
    async fn healthz_staleness_disabled_when_threshold_is_none() {
        let handle = MetricsHandle::new(None);
        handle
            .update(&SyncStats::default(), &healthy_status(0))
            .await;
        // No threshold set, last_success 10 years ago — must still be 200
        backdate_last_success(&handle, 10 * 365 * 24 * 3600).await;

        let (status, _body) = render_healthz(&handle).await;
        assert_eq!(status, axum::http::StatusCode::OK);
    }

    #[tokio::test]
    async fn healthz_staleness_tripped_also_when_failures_high() {
        // Both conditions at once -> 503
        let handle = MetricsHandle::new(None);
        handle
            .update(&SyncStats::default(), &healthy_status(5))
            .await;
        set_threshold(&handle, Some(chrono::Duration::seconds(60))).await;
        backdate_last_success(&handle, 120).await;

        let (status, _body) = render_healthz(&handle).await;
        assert_eq!(status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn healthz_staleness_ignored_when_last_success_never_set() {
        // First cycle failed -> no last_success_at. Staleness must not trip
        // because we have no anchor timestamp yet; failures alone drive 503.
        let handle = MetricsHandle::new(None);
        let mut h = HealthStatus::new();
        h.record_failure("first ever");
        handle.update(&SyncStats::default(), &h).await;
        set_threshold(&handle, Some(chrono::Duration::seconds(1))).await;

        let (status, _body) = render_healthz(&handle).await;
        // 1 consecutive failure is below 5, and last_success is None, so 200
        assert_eq!(status, axum::http::StatusCode::OK);
    }

    // ── DB-backed gauges ──────────────────────────────────────────────────────

    fn make_summary(
        downloaded: u64,
        pending: u64,
        failed: u64,
        downloaded_bytes: u64,
    ) -> SyncSummary {
        SyncSummary {
            total_assets: downloaded + pending + failed,
            downloaded,
            pending,
            failed,
            downloaded_bytes,
            last_sync_completed: None,
            last_sync_started: None,
        }
    }

    #[tokio::test]
    async fn update_db_stats_sets_asset_count_gauges() {
        let handle = MetricsHandle::new(None);
        let summary = make_summary(100, 5, 2, 1_000_000);
        handle.update_db_stats(&summary, 107);

        let output = render_metrics(&handle).await;
        assert!(
            output.contains(r#"kei_db_assets_total{status="downloaded"} 100"#),
            "downloaded count missing or wrong:\n{output}"
        );
        assert!(
            output.contains(r#"kei_db_assets_total{status="pending"} 5"#),
            "pending count missing or wrong:\n{output}"
        );
        assert!(
            output.contains(r#"kei_db_assets_total{status="failed"} 2"#),
            "failed count missing or wrong:\n{output}"
        );
    }

    #[tokio::test]
    async fn update_db_stats_sets_size_bytes_gauge() {
        let handle = MetricsHandle::new(None);
        let summary = make_summary(50, 0, 0, 2_048_000);
        handle.update_db_stats(&summary, 50);

        let output = render_metrics(&handle).await;
        assert!(
            output.contains(r#"kei_db_assets_size_bytes{status="downloaded"} 2048000"#),
            "downloaded_bytes gauge missing or wrong:\n{output}"
        );
    }

    #[tokio::test]
    async fn update_db_stats_sets_last_sync_assets_seen_gauge() {
        let handle = MetricsHandle::new(None);
        let summary = make_summary(28_000, 3_000, 71, 0);
        handle.update_db_stats(&summary, 31_071);

        let output = render_metrics(&handle).await;
        assert!(
            output.contains("kei_db_last_sync_assets_seen 31071"),
            "last_sync_assets_seen gauge missing or wrong:\n{output}"
        );
    }

    #[tokio::test]
    async fn update_db_stats_gauges_reflect_latest_call() {
        let handle = MetricsHandle::new(None);
        handle.update_db_stats(&make_summary(100, 10, 0, 500_000), 110);
        // Second call should overwrite (gauges, not counters).
        handle.update_db_stats(&make_summary(105, 5, 0, 525_000), 110);

        let output = render_metrics(&handle).await;
        assert!(
            !output.contains("kei_db_assets_total{status=\"pending\"} 10"),
            "stale pending value should not appear:\n{output}"
        );
        assert!(
            output.contains("kei_db_last_sync_assets_seen 110"),
            "assets_seen gauge wrong:\n{output}"
        );
    }

    #[tokio::test]
    async fn db_metric_names_present_in_output() {
        let handle = MetricsHandle::new(None);
        handle.update_db_stats(&make_summary(1, 0, 0, 1024), 1);

        let output = render_metrics(&handle).await;
        assert!(
            output.contains("kei_db_assets_total"),
            "kei_db_assets_total missing from /metrics output:\n{output}"
        );
        assert!(
            output.contains("kei_db_assets_size_bytes"),
            "kei_db_assets_size_bytes missing from /metrics output:\n{output}"
        );
        assert!(
            output.contains("kei_db_last_sync_assets_seen"),
            "kei_db_last_sync_assets_seen missing from /metrics output:\n{output}"
        );
    }

    #[tokio::test]
    async fn db_summary_failure_counter_increments() {
        let handle = MetricsHandle::new(None);
        handle.record_db_summary_failure();
        handle.record_db_summary_failure();

        let output = render_metrics(&handle).await;
        assert!(
            output.contains("kei_db_summary_read_failures_total 2"),
            "db_summary_read_failures counter missing or wrong:\n{output}"
        );
    }

    /// Regression test for https://github.com/rhoopr/kei/issues/248
    ///
    /// `spawn_server()` must not panic when called from within a tokio runtime
    /// with a `staleness_threshold` set. Before the fix, `blocking_lock()` was
    /// used inside the tokio runtime, which panics unconditionally regardless
    /// of lock contention.
    #[tokio::test]
    async fn spawn_server_with_staleness_threshold_does_not_panic_inside_runtime() {
        let token = CancellationToken::new();
        // Port 0 lets the OS pick a free ephemeral port.
        let result = spawn_server(0, token.clone(), Some(chrono::Duration::seconds(3600)));
        assert!(
            result.is_ok(),
            "spawn_server panicked or errored inside tokio runtime: {:?}",
            result.err()
        );
        token.cancel();
    }

    /// Full end-to-end smoke test: the real axum stack, a real TCP socket, and a
    /// real HTTP client. Catches regressions anywhere between `spawn_server` and
    /// the response body that the handler unit tests (which call `handle_*`
    /// directly against a `State`) cannot see.
    #[tokio::test]
    async fn spawn_server_serves_metrics_and_healthz_over_http() {
        let token = CancellationToken::new();
        let (handle, task, addr) =
            spawn_server(0, token.clone(), Some(chrono::Duration::seconds(3600)))
                .expect("spawn_server should bind and spawn");

        // Push a healthy cycle so /healthz returns 200 instead of the pre-first-cycle 503.
        handle
            .update(&SyncStats::default(), &healthy_status(0))
            .await;

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap();

        // spawn_server binds to 0.0.0.0; dial via 127.0.0.1 because Windows
        // treats 0.0.0.0 as AddrNotAvailable on connect.
        let base = format!("http://127.0.0.1:{}", addr.port());

        let metrics_resp = client
            .get(format!("{base}/metrics"))
            .send()
            .await
            .expect("GET /metrics should succeed");
        assert_eq!(metrics_resp.status(), reqwest::StatusCode::OK);
        let ct = metrics_resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        assert!(
            ct.contains("application/openmetrics-text"),
            "unexpected content-type: {ct}"
        );
        let body = metrics_resp.text().await.unwrap();
        assert!(
            body.contains("kei_sync_assets_seen_total"),
            "expected kei_sync_* counters in body:\n{body}"
        );

        let healthz_resp = client
            .get(format!("{base}/healthz"))
            .send()
            .await
            .expect("GET /healthz should succeed");
        assert_eq!(healthz_resp.status(), reqwest::StatusCode::OK);

        token.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), task).await;
    }
}
