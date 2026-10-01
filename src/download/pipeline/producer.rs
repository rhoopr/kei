//! Asset planning, pending adoption, disk forecasting, and task dispatch.

use std::sync::Arc;

use anyhow::Result;
use futures_util::StreamExt;
use rustc_hash::{FxHashMap, FxHashSet};
use tokio::sync::mpsc;

use crate::download::filter::{
    DownloadTask, FilterReason, extract_skip_candidates, is_asset_filtered,
};
use crate::download::finalize::finalize_failed;
use crate::download::metadata_rewrite::MetadataFlags;
use crate::download::planner::{ExistingPathMatch, TaskPlanner};
use crate::download::{
    ClaimedLegacyMasterStates, DownloadConfig, DownloadContext, DownloadStore, metadata_rewrite,
    planner,
};
use crate::icloud::photos::PhotoAsset;
use crate::icloud::photos::session::is_session_error as is_provider_session_error;

use super::StreamPipelineShared;
use super::adoption::{
    PendingOnDiskAdoption, adopt_pending_on_disk_skip, adopt_pending_on_disk_task,
    effective_asset_library, effective_asset_library_arc, state_confirmed_current_path_exists,
};
use super::task::capture_repair_requested;

/// Outcome of `batch_forecast_decision` — either keep queueing, emit a
/// one-shot warn, or stop enqueuing so the caller cancels the sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BatchForecast {
    Continue,
    Warn,
    Bail,
}

/// Re-snapshot the free-disk probe every this many bytes queued. The
/// producer caches `initial_free` at enumeration start so per-task probes
/// don't hammer `statvfs` on every asset, but a long sync can run for hours
/// while another process fills the FS. Without periodic refresh the bail
/// decision rides on a stale snapshot; downloads still fail loudly with
/// ENOSPC, but the sync would have been cancelled earlier with up-to-date
/// data. 10 GiB is small enough to catch a fast-filling FS quickly and
/// large enough that the periodic stat call is cheap relative to bytes
/// downloaded.
pub(in crate::download) const FREE_SPACE_RESNAPSHOT_INTERVAL_BYTES: u64 = 10 * 1024 * 1024 * 1024;

/// Classify the impact of adding `size` bytes to the running queued total
/// against the free-space snapshot captured at enumeration start.
///
/// Side-effects: `fetch_add`s `size` into `queued_bytes` so concurrent
/// callers see a consistent total. The caller is responsible for emitting
/// the log line and/or cancelling.
fn batch_forecast_decision(
    size: u64,
    initial_free: Option<u64>,
    queued_bytes: &std::sync::atomic::AtomicU64,
    warn_emitted: &std::sync::atomic::AtomicBool,
) -> (BatchForecast, u64) {
    let total = queued_bytes.fetch_add(size, std::sync::atomic::Ordering::Relaxed) + size;
    let Some(free) = initial_free else {
        return (BatchForecast::Continue, total);
    };
    if total >= free {
        return (BatchForecast::Bail, total);
    }
    let warn_threshold = free.saturating_mul(9) / 10;
    if total >= warn_threshold && !warn_emitted.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return (BatchForecast::Warn, total);
    }
    (BatchForecast::Continue, total)
}

/// Decide whether the producer should re-snapshot free disk space on
/// this `forecast_check` call.
///
/// `total` is the cumulative queued-bytes value just returned by
/// `batch_forecast_decision`; `last_snapshot_total` is the queued-bytes
/// value at the previous re-snapshot (initially zero, since the first
/// snapshot happens at enumeration start before any bytes are queued).
/// Returns `true` when the gap is at or above `interval`.
///
/// Pure helper so the cadence is testable without spinning up the producer
/// loop or touching the filesystem.
fn should_resnapshot_free_space(total: u64, last_snapshot_total: u64, interval: u64) -> bool {
    interval > 0 && total.saturating_sub(last_snapshot_total) >= interval
}

/// Per-asset outcome in the producer's task loop. Ordered by ascending
/// priority so `.max()` picks the winner when an asset has tasks with
/// mixed outcomes (e.g. one version on disk, another sent for download).
#[derive(Debug, Clone, Copy, PartialOrd, Ord, PartialEq, Eq)]
enum AssetDisposition {
    Unresolved,
    RetryOnly,
    RetryExhausted,
    StateSkip,
    AmpmVariant,
    OnDisk,
    Forwarded,
}

/// Breakdown of assets skipped during the producer phase.
///
/// Every asset from the API stream must be accounted for: either it ends up
/// in one of these skip buckets, gets sent for download (showing up in
/// `downloaded` / `failed`), or was an enumeration error.
#[derive(Debug, Default, Clone)]
pub(in crate::download) struct ProducerSkipSummary {
    pub(in crate::download) by_state: usize,
    pub(in crate::download) on_disk: usize,
    pub(in crate::download) ampm_variant: usize,
    pub(in crate::download) by_media_type: usize,
    pub(in crate::download) by_date_range: usize,
    pub(in crate::download) by_live_photo: usize,
    pub(in crate::download) by_filename: usize,
    pub(in crate::download) by_excluded_album: usize,
    pub(in crate::download) duplicates: usize,
    pub(in crate::download) retry_exhausted: usize,
    pub(in crate::download) retry_only: usize,
}

impl ProducerSkipSummary {
    pub(in crate::download) fn total(&self) -> usize {
        self.by_state
            + self.on_disk
            + self.ampm_variant
            + self.by_media_type
            + self.by_date_range
            + self.by_live_photo
            + self.by_filename
            + self.by_excluded_album
            + self.duplicates
            + self.retry_exhausted
            + self.retry_only
    }

    fn record_filter_reason(&mut self, reason: crate::download::filter::FilterReason) {
        match reason {
            crate::download::filter::FilterReason::MalformedAsset => self.by_filename += 1,
            crate::download::filter::FilterReason::ExcludedAlbum => self.by_excluded_album += 1,
            crate::download::filter::FilterReason::MediaType => self.by_media_type += 1,
            crate::download::filter::FilterReason::LivePhoto => self.by_live_photo += 1,
            crate::download::filter::FilterReason::DateRange => self.by_date_range += 1,
            crate::download::filter::FilterReason::Filename => self.by_filename += 1,
        }
    }
}

impl std::ops::AddAssign for ProducerSkipSummary {
    fn add_assign(&mut self, rhs: Self) {
        self.by_state += rhs.by_state;
        self.on_disk += rhs.on_disk;
        self.ampm_variant += rhs.ampm_variant;
        self.by_media_type += rhs.by_media_type;
        self.by_date_range += rhs.by_date_range;
        self.by_live_photo += rhs.by_live_photo;
        self.by_filename += rhs.by_filename;
        self.by_excluded_album += rhs.by_excluded_album;
        self.duplicates += rhs.duplicates;
        self.retry_exhausted += rhs.retry_exhausted;
        self.retry_only += rhs.retry_only;
    }
}

impl From<ProducerSkipSummary> for crate::download::SkipBreakdown {
    fn from(s: ProducerSkipSummary) -> Self {
        Self {
            by_state: s.by_state,
            on_disk: s.on_disk,
            by_media_type: s.by_media_type,
            by_date_range: s.by_date_range,
            by_live_photo: s.by_live_photo,
            by_filename: s.by_filename,
            by_excluded_album: s.by_excluded_album,
            ampm_variant: s.ampm_variant,
            duplicates: s.duplicates,
            retry_exhausted: s.retry_exhausted,
            retry_only: s.retry_only,
        }
    }
}

async fn record_seen_for_forwarded_task(
    db: &dyn DownloadStore,
    config: &DownloadConfig,
    asset: &PhotoAsset,
    task: &DownloadTask,
) -> Result<(), crate::state::error::StateError> {
    let result = planner::upsert_seen_for_task(db, config, asset, task).await;
    if let Err(e) = &result {
        tracing::warn!(target: "kei::download::pipeline",
            asset_id = %task.asset_id,
            error = %e,
            "Failed to record asset"
        );
    }
    result
}

#[derive(Clone, Default)]
pub(super) struct StreamProducerMetrics {
    pub(super) assets_seen: Arc<std::sync::atomic::AtomicU64>,
    pub(super) enum_errors: Arc<std::sync::atomic::AtomicUsize>,
    pub(super) provider_auth_errors: Arc<std::sync::atomic::AtomicUsize>,
    pub(super) state_write_failures: Arc<std::sync::atomic::AtomicUsize>,
    pub(super) enumeration_complete: Arc<std::sync::atomic::AtomicBool>,
}

pub(super) struct StreamProducer {
    pub(super) handle: tokio::task::JoinHandle<ProducerSkipSummary>,
    pub(super) metrics: StreamProducerMetrics,
}

pub(super) fn spawn_stream_download_producer<S>(
    combined: S,
    download_ctx: Arc<DownloadContext>,
    task_tx: mpsc::Sender<DownloadTask>,
    initial_free_at_start: Option<u64>,
    shared: StreamPipelineShared,
) -> StreamProducer
where
    S: futures_util::Stream<Item = anyhow::Result<crate::icloud::photos::PhotoAsset>>
        + Send
        + 'static,
{
    let metrics = StreamProducerMetrics::default();
    let producer_config = shared.config;
    let producer_state_db = shared.state_db;
    let producer_shutdown = shared.pipeline_shutdown;
    let producer_pb = shared.pb;
    let assets_seen_producer = Arc::clone(&metrics.assets_seen);
    let enum_errors_producer = Arc::clone(&metrics.enum_errors);
    let provider_auth_errors = Arc::clone(&metrics.provider_auth_errors);
    let state_write_failures_producer = Arc::clone(&metrics.state_write_failures);
    let enumeration_complete_producer = Arc::clone(&metrics.enumeration_complete);
    let queued_bytes_producer = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let space_warn_emitted_producer = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let handle = tokio::spawn(async move {
        let config = &producer_config;
        let metadata_writers_enabled = MetadataFlags::from(config.as_ref()).has_any_write();
        let mut task_planner = match TaskPlanner::for_download(producer_state_db.as_deref()).await {
            Ok(planner) => planner,
            Err(error) => {
                state_write_failures_producer.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                tracing::error!(target: "kei::download::pipeline", %error, "Failed to load download path reservations");
                return ProducerSkipSummary::default();
            }
        };
        let mut seen_asset_record_names: FxHashSet<Arc<str>> = FxHashSet::default();
        let mut claimed_legacy_master_states = ClaimedLegacyMasterStates::default();
        // Skipped-asset IDs accumulated across the producer run and
        // flushed to the DB in a single transaction at the end. This
        // collapses N UPDATE statements (one per fast-skip / on-disk
        // skip) into one batched UPDATE so the producer loop doesn't
        // serialize behind an fsync-per-asset under WAL mode.
        //
        // Vec is sufficient: every push is inside a branch predicated on
        // `seen_asset_record_names.insert(asset.asset_record_name_arc())`
        // returning true, so IDs are already unique at this point.
        let mut touched_assets: Vec<(Arc<str>, Arc<str>)> = Vec::new();
        let mut skips = ProducerSkipSummary::default();
        let mut assets_forwarded = 0u64;
        // Free-space probe lives in an `AtomicU64` (sentinel
        // `u64::MAX` = "no probe available") so the producer task can
        // refresh it every FREE_SPACE_RESNAPSHOT_INTERVAL_BYTES queued
        // without breaking `Send`. The producer is a single task, so atomic
        // ordering can stay Relaxed.
        const FREE_PROBE_NONE_SENTINEL: u64 = u64::MAX;
        let initial_free_atomic = std::sync::atomic::AtomicU64::new(
            initial_free_at_start.unwrap_or(FREE_PROBE_NONE_SENTINEL),
        );
        let last_resnapshot_total = std::sync::atomic::AtomicU64::new(0);
        let directory_for_resnapshot = config.directory.clone();
        let forecast_check = |size: u64| -> bool {
            // Re-snapshot before classifying so the decision uses the
            // freshest data on the boundary call.
            let total_so_far = queued_bytes_producer.load(std::sync::atomic::Ordering::Relaxed);
            if should_resnapshot_free_space(
                total_so_far + size,
                last_resnapshot_total.load(std::sync::atomic::Ordering::Relaxed),
                FREE_SPACE_RESNAPSHOT_INTERVAL_BYTES,
            ) && let Some(refreshed) = crate::available_disk_space(&directory_for_resnapshot)
            {
                let prior = initial_free_atomic.load(std::sync::atomic::Ordering::Relaxed);
                initial_free_atomic.store(refreshed, std::sync::atomic::Ordering::Relaxed);
                last_resnapshot_total
                    .store(total_so_far + size, std::sync::atomic::Ordering::Relaxed);
                tracing::debug!(target: "kei::download::pipeline",
                    prior_free_bytes = if prior == FREE_PROBE_NONE_SENTINEL {
                        0
                    } else {
                        prior
                    },
                    refreshed_free_bytes = refreshed,
                    queued_bytes = total_so_far + size,
                    "Refreshed free-disk snapshot"
                );
            }
            let raw = initial_free_atomic.load(std::sync::atomic::Ordering::Relaxed);
            let initial_free = if raw == FREE_PROBE_NONE_SENTINEL {
                None
            } else {
                Some(raw)
            };
            let (decision, total) = batch_forecast_decision(
                size,
                initial_free,
                &queued_bytes_producer,
                &space_warn_emitted_producer,
            );
            match decision {
                BatchForecast::Continue => false,
                BatchForecast::Warn => {
                    if let Some(free) = initial_free {
                        #[allow(
                            clippy::cast_precision_loss,
                            clippy::cast_possible_truncation,
                            clippy::cast_sign_loss,
                            reason = "percent is 0..=100 after ratio; logged as a diagnostic, not used for control flow"
                        )]
                        let percent = (total as f64 * 100.0 / free as f64) as u64;
                        tracing::warn!(target: "kei::download::pipeline",
                            queued_bytes = total,
                            initial_free_bytes = free,
                            percent_of_free = percent,
                            "Queued download batch approaching 90% of initial free disk space"
                        );
                    }
                    false
                }
                BatchForecast::Bail => {
                    tracing::error!(target: "kei::download::pipeline",
                        queued_bytes = total,
                        initial_free_bytes = initial_free.unwrap_or(0),
                        "Queued download batch would exceed initial free disk space; cancelling sync"
                    );
                    producer_shutdown.cancel();
                    true
                }
            }
        };
        tokio::pin!(combined);
        while let Some(result) = combined.next().await {
            if producer_shutdown.is_cancelled() {
                break;
            }
            match result {
                Ok(mut asset) => {
                    if !seen_asset_record_names.insert(asset.asset_record_name_arc()) {
                        tracing::debug!(target: "kei::download::pipeline",
                            asset_id = %asset.id(),
                            asset_record_name = %asset.asset_record_name(),
                            "Duplicate CPLAsset record from API, skipping"
                        );
                        skips.duplicates += 1;
                        continue;
                    }

                    assets_seen_producer.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    // The pre-plan refresh has already committed the marker,
                    // so the skip sites must not re-stamp it from the stale
                    // context.
                    let mut metadata_refresh_attempted = false;
                    if let Some(db) = &producer_state_db {
                        let library = effective_asset_library(&asset, config).to_owned();
                        if let Err(e) =
                            planner::upsert_asset_master_mapping(db.as_ref(), &library, &asset)
                                .await
                        {
                            state_write_failures_producer
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            tracing::warn!(target: "kei::download::pipeline",
                                asset_id = %asset.id(),
                                asset_record_name = %asset.asset_record_name(),
                                library = %library,
                                error = %e,
                                "Failed to record asset/master mapping"
                            );
                        }
                        if !matches!(
                            is_asset_filtered(&asset, config.as_ref()),
                            Some(FilterReason::ExcludedAlbum)
                        ) {
                            match download_ctx
                                .select_asset_state_record_name_for_download(
                                    Some(db.as_ref()),
                                    &library,
                                    &asset,
                                    &mut claimed_legacy_master_states,
                                )
                                .await
                            {
                                Ok(state_record_name) => {
                                    asset = asset.with_state_record_name(state_record_name);
                                }
                                Err(e) => {
                                    state_write_failures_producer
                                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                    tracing::warn!(target: "kei::download::pipeline",
                                        asset_id = %asset.id(),
                                        asset_record_name = %asset.asset_record_name(),
                                        library = %library,
                                        error = %e,
                                        "Failed to claim legacy master state owner"
                                    );
                                    producer_pb.inc(1);
                                    continue;
                                }
                            }
                        }
                        // Apply changed provider metadata before filtering and
                        // path planning, so a metadata-only edit reaches the
                        // catalogue even when the media task is filtered or
                        // skipped as already on disk.
                        let capture = crate::download::filter::metadata_capture(&asset);
                        let drift = download_ctx.has_provider_metadata_drift(
                            &library,
                            asset.state_id(),
                            &capture,
                        );
                        // A marker outlives a stale row whose stored hash still
                        // matches the provider, which drift alone cannot see.
                        let retry_marker = !drift
                            && download_ctx
                                .has_downloaded_metadata_retry_marker(&library, asset.state_id());
                        if config.refresh_metadata || drift || retry_marker {
                            metadata_refresh_attempted = true;
                            // A marker-only refresh must not queue more work:
                            // the marker is already visible for retry.
                            let mark_for_rewrite =
                                metadata_writers_enabled && (config.refresh_metadata || drift);
                            match db
                                .refresh_downloaded_asset_metadata(
                                    &library,
                                    asset.state_id(),
                                    (&capture, asset.created(), Some(asset.added_date())),
                                    mark_for_rewrite,
                                    capture_repair_requested(config),
                                    crate::state::METADATA_CAPTURE_REVISION,
                                )
                                .await
                            {
                                Ok(updated) if updated > 0 => {}
                                Ok(_) => {
                                    // A forced sweep legitimately visits assets
                                    // with no live downloaded row; drift and
                                    // markers imply one exists.
                                    if drift || retry_marker {
                                        state_write_failures_producer
                                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                        tracing::warn!(target: "kei::download::pipeline",
                                            asset_id = %asset.id(),
                                            library = %library,
                                            "Changed provider metadata matched no downloaded state row"
                                        );
                                    }
                                }
                                Err(e) => {
                                    state_write_failures_producer
                                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                    tracing::warn!(target: "kei::download::pipeline",
                                        asset_id = %asset.id(),
                                        library = %library,
                                        error = %e,
                                        "Failed to refresh downloaded asset metadata"
                                    );
                                }
                            }
                        }
                    }

                    // Persist membership even when planning skips already-landed media.
                    if let Some(db) = &producer_state_db
                        && let Err(e) =
                            planner::record_album_membership_if_named(db.as_ref(), config, &asset)
                                .await
                        && let Some(album) = config.album_name.as_deref()
                    {
                        state_write_failures_producer
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        tracing::warn!(target: "kei::download::pipeline",
                            asset_id = %asset.id(),
                            album = %album,
                            error = %e,
                            "Failed to record album membership after retries"
                        );
                    }

                    let plan = match task_planner.plan_download_asset(&asset, config).await {
                        Ok(plan) => plan,
                        Err(error) => {
                            state_write_failures_producer
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            tracing::error!(target: "kei::download::pipeline", %error, "Failed to plan reserved download paths");
                            return skips;
                        }
                    };
                    if let Some(db) = &producer_state_db
                        && let Err(error) = task_planner
                            .persist_download_reservations(db.as_ref(), &plan.tasks)
                            .await
                    {
                        state_write_failures_producer
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        tracing::error!(target: "kei::download::pipeline", %error, "Failed to reserve download paths before publication");
                        return skips;
                    }
                    if let Some(reason) = plan.filter_reason {
                        skips.record_filter_reason(reason);
                        producer_pb.inc(1);
                        continue;
                    }
                    if let Some(resource) = &plan.malformed_resource {
                        enum_errors_producer.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        tracing::error!(target: "kei::download::pipeline",
                            asset_id = %asset.id(),
                            field = %resource.field,
                            reason = %resource.reason,
                            "Malformed CloudKit resource prevented download planning"
                        );
                        producer_pb.inc(1);
                        continue;
                    }

                    if plan.tasks.is_empty() {
                        // No-op for status='downloaded' rows (the common
                        // case). A pending row from a prior failed or
                        // interrupted sync is adopted when the matching file
                        // is already on disk; if adoption fails, the touched
                        // flush still lets stuck-pipeline recovery promote it.
                        let candidates = extract_skip_candidates(&asset, config.as_ref());
                        if !metadata_refresh_attempted {
                            metadata_rewrite::tag_if_needed(
                                producer_state_db.as_deref(),
                                config,
                                &asset,
                                &candidates,
                                &download_ctx,
                            )
                            .await;
                        }
                        let adoption = adopt_pending_on_disk_skip(
                            producer_state_db.as_deref(),
                            config,
                            &asset,
                            &download_ctx,
                            &mut task_planner,
                        )
                        .await;
                        if adoption.state_write_failures > 0 {
                            state_write_failures_producer.fetch_add(
                                adoption.state_write_failures,
                                std::sync::atomic::Ordering::Relaxed,
                            );
                        }
                        if producer_state_db.is_some() {
                            let library = effective_asset_library_arc(&asset, config);
                            touched_assets.push((library, asset.state_id_arc()));
                        }
                        skips.on_disk += 1;
                        producer_pb.inc(1);
                    } else {
                        let mut disposition = AssetDisposition::Unresolved;

                        for task in plan.tasks {
                            // Mark assets that have exceeded the retry limit as failed.
                            if let Some(attempts) =
                                download_ctx.attempt_count(&task.library, &task.asset_id)
                                && config.max_download_attempts > 0
                                && attempts >= config.max_download_attempts
                            {
                                tracing::warn!(target: "kei::download::pipeline",
                                    asset_id = %task.asset_id,
                                    attempts,
                                    max = config.max_download_attempts,
                                    "Asset exceeded max download attempts, marking as failed"
                                );
                                if let Some(db) = &producer_state_db {
                                    let error = format!(
                                        "Exceeded max download attempts ({attempts}/{})",
                                        config.max_download_attempts
                                    );
                                    if let Err(e) =
                                        finalize_failed(db.as_ref(), &task.library, &task, &error)
                                            .await
                                    {
                                        tracing::warn!(target: "kei::download::pipeline",
                                            asset_id = %task.asset_id,
                                            error = %e,
                                            "Failed to mark asset as failed"
                                        );
                                    }
                                }
                                disposition = disposition.max(AssetDisposition::RetryExhausted);
                                continue;
                            }

                            if config.retry_only
                                && !download_ctx.is_known(&task.library, &task.asset_id)
                            {
                                tracing::debug!(target: "kei::download::pipeline",
                                    asset_id = %task.asset_id,
                                    "Skipping new asset in retry-only mode"
                                );
                                disposition = disposition.max(AssetDisposition::RetryOnly);
                                continue;
                            }

                            if let Some(db) = &producer_state_db {
                                if let Some(adoption) = adopt_pending_on_disk_task(
                                    producer_state_db.as_deref(),
                                    config,
                                    &asset,
                                    &download_ctx,
                                    &mut task_planner,
                                    &task,
                                )
                                .await
                                {
                                    disposition = disposition.max(AssetDisposition::OnDisk);
                                    match adoption {
                                        PendingOnDiskAdoption::Adopted(existing_path) => {
                                            tracing::debug!(target: "kei::download::pipeline",
                                                asset_id = %task.asset_id,
                                                path = %existing_path.display(),
                                                "Skipping (pending state adopted existing file)"
                                            );
                                        }
                                        PendingOnDiskAdoption::StateWriteFailed(existing_path) => {
                                            state_write_failures_producer
                                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                            tracing::debug!(target: "kei::download::pipeline",
                                                asset_id = %task.asset_id,
                                                path = %existing_path.display(),
                                                "Skipping re-download after pending on-disk state write failed"
                                            );
                                        }
                                    }
                                    continue;
                                }

                                match download_ctx.should_download_fast(
                                    &task.library,
                                    &task.asset_id,
                                    task.version_size,
                                    &task.checksum,
                                    false,
                                ) {
                                    Some(true) => {
                                        disposition = disposition.max(AssetDisposition::Forwarded);
                                        if record_seen_for_forwarded_task(
                                            db.as_ref(),
                                            config,
                                            &asset,
                                            &task,
                                        )
                                        .await
                                        .is_err()
                                        {
                                            state_write_failures_producer
                                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                            continue;
                                        }
                                        let size = task.size;
                                        if task_tx.send(task).await.is_err() {
                                            return skips;
                                        }
                                        if forecast_check(size) {
                                            return skips;
                                        }
                                    }
                                    Some(false) => {
                                        disposition = disposition.max(AssetDisposition::StateSkip);
                                        tracing::debug!(target: "kei::download::pipeline",
                                            asset_id = %task.asset_id,
                                            "Skipping (state confirms no download needed)"
                                        );
                                    }
                                    None => {
                                        if let Some(existing_path) =
                                            state_confirmed_current_path_exists(
                                                &download_ctx,
                                                config,
                                                &asset,
                                                &task,
                                                &mut task_planner,
                                            )
                                            .await
                                        {
                                            disposition = disposition.max(AssetDisposition::OnDisk);
                                            tracing::debug!(target: "kei::download::pipeline",
                                                asset_id = %task.asset_id,
                                                path = %existing_path.display(),
                                                "Skipping (state path exists on disk)"
                                            );
                                            let candidates =
                                                extract_skip_candidates(&asset, config.as_ref());
                                            if !metadata_refresh_attempted {
                                                metadata_rewrite::tag_if_needed(
                                                    producer_state_db.as_deref(),
                                                    config,
                                                    &asset,
                                                    &candidates,
                                                    &download_ctx,
                                                )
                                                .await;
                                            }
                                            continue;
                                        }

                                        match task_planner.existing_path_match(&task.download_path)
                                        {
                                            ExistingPathMatch::Exact => {
                                                disposition =
                                                    disposition.max(AssetDisposition::OnDisk);
                                                tracing::debug!(target: "kei::download::pipeline",
                                                    asset_id = %task.asset_id,
                                                    path = %task.download_path.display(),
                                                    "Skipping (already downloaded)"
                                                );
                                            }
                                            ExistingPathMatch::AmpmVariant => {
                                                disposition =
                                                    disposition.max(AssetDisposition::AmpmVariant);
                                                tracing::debug!(target: "kei::download::pipeline",
                                                    asset_id = %task.asset_id,
                                                    path = %task.download_path.display(),
                                                    "Skipping (AM/PM variant exists on disk)"
                                                );
                                            }
                                            ExistingPathMatch::Missing => {
                                                tracing::debug!(target: "kei::download::pipeline",
                                                    asset_id = %task.asset_id,
                                                    path = %task.download_path.display(),
                                                    "File missing, will re-download"
                                                );
                                                disposition =
                                                    disposition.max(AssetDisposition::Forwarded);
                                                if record_seen_for_forwarded_task(
                                                    db.as_ref(),
                                                    config,
                                                    &asset,
                                                    &task,
                                                )
                                                .await
                                                .is_err()
                                                {
                                                    state_write_failures_producer.fetch_add(
                                                        1,
                                                        std::sync::atomic::Ordering::Relaxed,
                                                    );
                                                    continue;
                                                }
                                                let size = task.size;
                                                if task_tx.send(task).await.is_err() {
                                                    return skips;
                                                }
                                                if forecast_check(size) {
                                                    return skips;
                                                }
                                            }
                                        }
                                    }
                                }
                            } else {
                                disposition = disposition.max(AssetDisposition::Forwarded);
                                let size = task.size;
                                if task_tx.send(task).await.is_err() {
                                    return skips;
                                }
                                if forecast_check(size) {
                                    return skips;
                                }
                            }
                        }

                        match disposition {
                            AssetDisposition::Forwarded => assets_forwarded += 1,
                            AssetDisposition::OnDisk => skips.on_disk += 1,
                            AssetDisposition::AmpmVariant => skips.ampm_variant += 1,
                            AssetDisposition::StateSkip => skips.by_state += 1,
                            AssetDisposition::RetryExhausted => skips.retry_exhausted += 1,
                            AssetDisposition::RetryOnly => skips.retry_only += 1,
                            AssetDisposition::Unresolved => {
                                tracing::warn!(target: "kei::download::pipeline",
                                    asset_id = %asset.id(),
                                    "Asset with non-empty tasks had no disposition"
                                );
                            }
                        }

                        producer_pb.inc(1);
                    }
                }
                Err(e) => {
                    enum_errors_producer.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if is_provider_session_error(&e) {
                        provider_auth_errors.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        tracing::warn!(target: "kei::download::pipeline",
                            "Provider session expired during enumeration; retaining checkpoint for reauthentication"
                        );
                        break;
                    }
                    // Single-line `tracing::error!` doesn't need the
                    // progress bar suspended; the bar redraws cleanly after
                    // a one-line emit and the suspend was just paying a
                    // hot-loop cost for no observable benefit.
                    tracing::error!(target: "kei::download::pipeline", error = %e, "Error fetching asset");
                }
            }
        }

        // At this point the outer `while let Some(...)` loop has
        // exited via stream-exhaustion (None) or via the in-loop shutdown
        // `break`. Channel-close early returns bypass this code path
        // entirely. Mark enumeration complete only when shutdown wasn't
        // the trigger so the cycle's `enum_in_progress:<zone>` marker is
        // cleared even when downstream downloads partially failed.
        if !producer_shutdown.is_cancelled()
            && provider_auth_errors.load(std::sync::atomic::Ordering::Relaxed) == 0
        {
            enumeration_complete_producer.store(true, std::sync::atomic::Ordering::Relaxed);
        }

        let total_skipped = skips.total();
        if total_skipped > 0 {
            // Single tracing event, runs once after the producer loop
            // exits — no suspend needed.
            tracing::debug!(target: "kei::download::pipeline",
                state = skips.by_state,
                on_disk = skips.on_disk,
                ampm_variant = skips.ampm_variant,
                media_type = skips.by_media_type,
                date_range = skips.by_date_range,
                live_photo = skips.by_live_photo,
                filename = skips.by_filename,
                excluded_album = skips.by_excluded_album,
                duplicates = skips.duplicates,
                retry_exhausted = skips.retry_exhausted,
                retry_only = skips.retry_only,
                total = total_skipped,
                "Skipped assets"
            );
        }

        // Invariant: every unique asset must be either skipped or forwarded.
        // Duplicates and enum errors are outside the unique-asset count.
        let seen = assets_seen_producer.load(std::sync::atomic::Ordering::Relaxed);
        let skipped_unique = (total_skipped - skips.duplicates) as u64;
        let accounted = skipped_unique + assets_forwarded;
        if accounted != seen {
            // Single tracing event; no suspend.
            tracing::warn!(target: "kei::download::pipeline",
                assets_seen = seen,
                accounted,
                forwarded = assets_forwarded,
                skipped = skipped_unique,
                duplicates = skips.duplicates,
                "Asset accounting mismatch -- some assets may be untracked"
            );
        }

        // Flush the accumulated last_seen_at updates in one transaction.
        // Running after the producer loop exits means we skip the fsync-
        // per-asset cost that dominated sync-start on mostly-synced
        // libraries.
        //
        // touched_assets contains assets the consumer will not finalize this
        // sync: trust-state fast-skips and on-disk skips. Bumping
        // last_seen_at is a no-op for terminal rows. For pending rows that
        // could not be adopted from disk, it is load-bearing for
        // stuck-pipeline recovery: promote_pending_to_failed promotes any
        // pending row whose last_seen_at >= sync_started_at.
        //
        // If we lose this flush (e.g. process killed between the producer
        // loop exiting and touch_last_seen_many returning), stuck-pipeline
        // promotion is delayed by exactly one sync. The same row hits the
        // same path next run and gets adopted or promoted then. No data loss.
        if let Some(db) = &producer_state_db
            && !touched_assets.is_empty()
        {
            let mut touched_by_library: FxHashMap<Arc<str>, Vec<Arc<str>>> = FxHashMap::default();
            for (library, id) in touched_assets {
                touched_by_library.entry(library).or_default().push(id);
            }
            for (library, ids) in touched_by_library {
                let touched_count = ids.len();
                let id_refs: Vec<&str> = ids.iter().map(AsRef::as_ref).collect();
                if let Err(e) = db.touch_last_seen_many(&library, &id_refs).await {
                    producer_pb.suspend(|| {
                        tracing::warn!(target: "kei::download::pipeline",
                            error = %e,
                            count = touched_count,
                            library = %library,
                            "Failed to batch-update last_seen_at for skipped assets"
                        );
                    });
                }
            }
        }

        skips
    });

    StreamProducer { handle, metrics }
}

#[cfg(test)]
mod tests;
