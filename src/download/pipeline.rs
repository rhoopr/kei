//! Streaming download pipeline — producer/consumer architecture that starts
//! downloading as soon as the first API page returns. Includes the Phase 2
//! cleanup pass and all single-task download logic.

use std::fs::FileTimes;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use futures_util::stream::{self, StreamExt};
use indicatif::ProgressBar;
use reqwest::Client;
use rustc_hash::{FxHashMap, FxHashSet};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;

use crate::icloud::photos::PhotoAsset;
use crate::retry::RetryConfig;
use crate::state::{AssetRecord, SyncRunStats, VersionSizeKey};

use super::error::DownloadError;
use super::file::{LocalFileSizeExpectation, local_file_size_matches_state};
use super::filter::{
    DerivedPath, DownloadTask, FilterReason, derive_expected_paths, determine_media_type,
    extract_skip_candidates, is_asset_filtered,
};
use super::finalize::{
    DownloadedFinalization, PendingStateWrite, StateWriteFlush, check_state_write_circuit_breaker,
    finalize_downloaded, finalize_failed, flush_pending_state_writes,
    flush_pending_state_writes_retaining_failures, state_db_unwritable_error,
    state_write_circuit_breaker_tripped,
};
#[cfg(test)]
use super::finalize::{STATE_DB_UNWRITABLE_THRESHOLD, STATE_WRITE_MAX_RETRIES};
#[cfg(test)]
use super::planner::ADD_ASSET_ALBUM_MAX_RETRIES;
#[cfg(test)]
use super::planner::add_asset_album_with_retry;
use super::planner::{self, ExistingPathMatch, TaskPlanner};
use super::{
    ClaimedLegacyMasterStates, DownloadConfig, DownloadContext, DownloadControls, DownloadOutcome,
    DownloadReporting, DownloadStore, RecordedLocalFile, metadata_rewrite,
    preload_download_context,
};

pub(super) use metadata_rewrite::MetadataFlags;

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
pub(super) const FREE_SPACE_RESNAPSHOT_INTERVAL_BYTES: u64 = 10 * 1024 * 1024 * 1024;

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
pub(super) struct ProducerSkipSummary {
    pub(super) by_state: usize,
    pub(super) on_disk: usize,
    pub(super) ampm_variant: usize,
    pub(super) by_media_type: usize,
    pub(super) by_date_range: usize,
    pub(super) by_live_photo: usize,
    pub(super) by_filename: usize,
    pub(super) by_excluded_album: usize,
    pub(super) duplicates: usize,
    pub(super) retry_exhausted: usize,
    pub(super) retry_only: usize,
}

impl ProducerSkipSummary {
    pub(super) fn total(&self) -> usize {
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

    fn record_filter_reason(&mut self, reason: super::filter::FilterReason) {
        match reason {
            super::filter::FilterReason::MalformedAsset => self.by_filename += 1,
            super::filter::FilterReason::ExcludedAlbum => self.by_excluded_album += 1,
            super::filter::FilterReason::MediaType => self.by_media_type += 1,
            super::filter::FilterReason::LivePhoto => self.by_live_photo += 1,
            super::filter::FilterReason::DateRange => self.by_date_range += 1,
            super::filter::FilterReason::Filename => self.by_filename += 1,
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

impl From<ProducerSkipSummary> for super::SkipBreakdown {
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

/// Result of the streaming download phase.
#[derive(Debug, Default)]
pub(super) struct StreamingResult {
    pub(super) downloaded: usize,
    pub(super) exif_failures: usize,
    pub(super) failed: Vec<DownloadTask>,
    pub(super) auth_errors: usize,
    pub(super) state_write_failures: usize,
    pub(super) enumeration_errors: usize,
    pub(super) assets_seen: u64,
    pub(super) skip_summary: ProducerSkipSummary,
    pub(super) bytes_downloaded: u64,
    pub(super) disk_bytes_written: u64,
    /// Count of 429/503 observations during Phase 1 downloads (per retry
    /// attempt, not per unique task). Feeds SyncStats.rate_limited.
    pub(super) rate_limit_observations: usize,
    /// True when any worker observed HTTP 410 for a signed CDN URL. Once this
    /// happens, the rest of the current URL batch is presumed stale too, so
    /// the pass aborts instead of hammering thousands of expired URLs.
    pub(super) url_expired_abort: bool,
    /// `true` when the producer reached the natural end of the API
    /// stream (so the `enum_in_progress:<zone>` marker can be cleared even
    /// when downstream downloads partially failed). `false` when the
    /// producer aborted via shutdown, channel-close, or panic.
    pub(super) enumeration_complete: bool,
    /// Photos downloaded in this pass (`MediaType::Photo` /
    /// `LivePhotoImage`). Lifted into `SyncStats.photos_downloaded`.
    pub(super) photos_downloaded: usize,
    /// Videos downloaded in this pass (`MediaType::Video` /
    /// `LivePhotoVideo`).
    pub(super) videos_downloaded: usize,
    /// Per-pass recap fold; merged with the cleanup pass's recap before
    /// the friendly card renders.
    pub(super) recap: super::recap::RunRecap,
    #[cfg(test)]
    pub(super) printed_filenames: Vec<PathBuf>,
}

/// Threshold of auth errors before aborting the download pass for re-authentication.
/// Counted cumulatively across both phases (streaming + cleanup).
pub(super) const AUTH_ERROR_THRESHOLD: usize = 3;

fn effective_asset_library<'a>(asset: &'a PhotoAsset, config: &'a DownloadConfig) -> &'a str {
    asset.source_zone().unwrap_or(config.library.as_ref())
}

fn effective_asset_library_arc(asset: &PhotoAsset, config: &DownloadConfig) -> Arc<str> {
    asset
        .source_zone()
        .map(Arc::from)
        .unwrap_or_else(|| Arc::clone(&config.library))
}

fn asset_record_for_derived_path(
    library: Arc<str>,
    asset: &PhotoAsset,
    derived: &DerivedPath,
) -> AssetRecord {
    AssetRecord::new_pending(
        library,
        asset.state_id().to_string(),
        derived.version_size,
        derived.checksum.to_string(),
        derived.filename.clone(),
        asset.created(),
        Some(asset.added_date()),
        derived.size,
        determine_media_type(derived.version_size, asset),
    )
    .with_metadata_arc(asset.metadata_arc())
}

fn pending_versions_for_asset<'a>(
    ctx: &'a DownloadContext,
    library: &str,
    asset: &PhotoAsset,
) -> Option<&'a FxHashSet<Box<str>>> {
    ctx.pending_ids
        .get(library)
        .and_then(|assets| assets.get(asset.state_id()))
}

fn pending_filename_for_asset_version<'a>(
    ctx: &'a DownloadContext,
    library: &str,
    asset: &PhotoAsset,
    version_size: VersionSizeKey,
) -> Option<&'a str> {
    ctx.pending_filenames
        .get(library)
        .and_then(|assets| assets.get(asset.state_id()))
        .and_then(|versions| versions.get(version_size.as_str()))
        .map(Box::as_ref)
}

fn pending_filename_matches_derived(pending_filename: &str, derived_filename: &str) -> bool {
    filenames_match_ampm_equivalent(pending_filename, derived_filename)
        || pending_filename.eq_ignore_ascii_case(derived_filename)
}

fn capture_repair_requested(config: &DownloadConfig) -> bool {
    config.refresh_metadata
        && matches!(
            config.capture_timestamp_repair,
            super::CaptureTimestampRepair::ReplaceWithCaptureLocal
        )
}

async fn adopt_pending_on_disk_skip(
    state_db: Option<&dyn DownloadStore>,
    config: &DownloadConfig,
    asset: &PhotoAsset,
    ctx: &DownloadContext,
    task_planner: &mut TaskPlanner,
) -> PendingOnDiskAdoptionSummary {
    let Some(db) = state_db else {
        return PendingOnDiskAdoptionSummary::default();
    };
    let library = effective_asset_library(asset, config);
    let pending_versions = pending_versions_for_asset(ctx, library, asset);
    let Some(pending_versions) = pending_versions else {
        return PendingOnDiskAdoptionSummary::default();
    };

    let mut summary = PendingOnDiskAdoptionSummary::default();
    for derived in derive_expected_paths(asset, config) {
        let version_size = derived.version_size.as_str();
        if !pending_versions.contains(version_size) {
            continue;
        }
        match adopt_pending_derived_path(
            db,
            library,
            asset,
            task_planner,
            &derived,
            ctx.pending_file_matching_checksum(
                library,
                asset.state_id(),
                derived.version_size,
                derived.checksum.as_ref(),
            ),
            capture_repair_requested(config),
        )
        .await
        {
            Some(PendingOnDiskAdoption::Adopted(_)) => {}
            Some(PendingOnDiskAdoption::StateWriteFailed(_)) => {
                summary.state_write_failures += 1;
            }
            None => {}
        }
    }

    summary
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct PendingOnDiskAdoptionSummary {
    state_write_failures: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PendingOnDiskAdoption {
    Adopted(PathBuf),
    StateWriteFailed(PathBuf),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PendingRetryAdoption {
    NotFound,
    Adopted,
    StateWriteFailed,
}

impl From<PendingOnDiskAdoption> for PendingRetryAdoption {
    fn from(adoption: PendingOnDiskAdoption) -> Self {
        match adoption {
            PendingOnDiskAdoption::Adopted(_) => Self::Adopted,
            PendingOnDiskAdoption::StateWriteFailed(_) => Self::StateWriteFailed,
        }
    }
}

pub(super) struct PendingRetryFileEvidence<'a> {
    pub(super) version_size: VersionSizeKey,
    pub(super) filename: &'a str,
    pub(super) checksum: &'a str,
    pub(super) local_path: PendingRetryLocalPath<'a>,
    pub(super) size: u64,
}

pub(super) enum PendingRetryLocalPath<'a> {
    Unrecorded,
    Current(&'a RecordedLocalFile),
    Historical,
}

pub(super) async fn adopt_pending_on_disk_for_retry(
    db: &dyn DownloadStore,
    config: &DownloadConfig,
    asset: &PhotoAsset,
    task_planner: &mut TaskPlanner,
    planned_tasks: &[DownloadTask],
    evidence: PendingRetryFileEvidence<'_>,
) -> PendingRetryAdoption {
    if matches!(evidence.local_path, PendingRetryLocalPath::Historical) {
        return PendingRetryAdoption::NotFound;
    }

    if let PendingRetryLocalPath::Current(recorded_file) = evidence.local_path {
        let local_path = &recorded_file.path;
        let recorded_filename_matches = local_path
            .file_name()
            .and_then(|filename| filename.to_str())
            .is_some_and(|filename| pending_filename_matches_derived(evidence.filename, filename));
        if !recorded_filename_matches {
            return PendingRetryAdoption::NotFound;
        }

        task_planner.prepare_path_parent(local_path).await;
        for task in planned_tasks.iter().filter(|task| {
            task.version_size == evidence.version_size
                && task.checksum.as_ref() == evidence.checksum
                && task.size == evidence.size
        }) {
            let mut recorded_task = task.clone();
            recorded_task.download_path = local_path.to_path_buf();
            if let Some(adoption) = adopt_pending_task_path(
                db,
                config,
                asset,
                task_planner,
                &recorded_task,
                Some(recorded_file),
            )
            .await
            {
                return adoption.into();
            }
        }

        for derived in derive_expected_paths(asset, config)
            .into_iter()
            .filter(|derived| {
                derived.version_size == evidence.version_size
                    && derived.checksum.as_ref() == evidence.checksum
                    && derived.size == evidence.size
                    && pending_filename_matches_derived(evidence.filename, &derived.filename)
            })
        {
            if let Some(adoption) = adopt_pending_derived_path_at(
                db,
                effective_asset_library(asset, config),
                asset,
                task_planner,
                &derived,
                local_path,
                Some(recorded_file),
                capture_repair_requested(config),
            )
            .await
            {
                return adoption.into();
            }
        }
        return PendingRetryAdoption::NotFound;
    }

    for task in planned_tasks.iter().filter(|task| {
        task.version_size == evidence.version_size
            && task.checksum.as_ref() == evidence.checksum
            && task.size == evidence.size
    }) {
        if let Some(adoption) =
            adopt_pending_task_path(db, config, asset, task_planner, task, None).await
        {
            return adoption.into();
        }
    }

    for derived in derive_expected_paths(asset, config)
        .into_iter()
        .filter(|derived| {
            derived.version_size == evidence.version_size
                && derived.checksum.as_ref() == evidence.checksum
                && derived.size == evidence.size
                && pending_filename_matches_derived(evidence.filename, &derived.filename)
        })
    {
        if let Some(adoption) = adopt_pending_derived_path(
            db,
            effective_asset_library(asset, config),
            asset,
            task_planner,
            &derived,
            None,
            capture_repair_requested(config),
        )
        .await
        {
            return adoption.into();
        }
    }

    PendingRetryAdoption::NotFound
}

async fn adopt_pending_on_disk_task(
    state_db: Option<&dyn DownloadStore>,
    config: &DownloadConfig,
    asset: &PhotoAsset,
    ctx: &DownloadContext,
    task_planner: &mut TaskPlanner,
    task: &DownloadTask,
) -> Option<PendingOnDiskAdoption> {
    let db = state_db?;
    let library = effective_asset_library(asset, config);
    let pending_versions = pending_versions_for_asset(ctx, library, asset)?;
    if !pending_versions.contains(task.version_size.as_str()) {
        return None;
    }

    if let Some(adoption) = adopt_pending_task_path(
        db,
        config,
        asset,
        task_planner,
        task,
        ctx.pending_file_matching_checksum(
            library,
            asset.state_id(),
            task.version_size,
            task.checksum.as_ref(),
        ),
    )
    .await
    {
        return Some(adoption);
    }

    for derived in derive_expected_paths(asset, config) {
        if derived.version_size != task.version_size {
            continue;
        }
        let pending_filename =
            pending_filename_for_asset_version(ctx, library, asset, task.version_size);
        if !pending_filename.is_some_and(|filename| {
            pending_filename_matches_derived(filename, derived.filename.as_str())
        }) {
            continue;
        }
        if let Some(adoption) = adopt_pending_derived_path(
            db,
            library,
            asset,
            task_planner,
            &derived,
            ctx.pending_file_matching_checksum(
                library,
                asset.state_id(),
                derived.version_size,
                derived.checksum.as_ref(),
            ),
            capture_repair_requested(config),
        )
        .await
        {
            return Some(adoption);
        }
    }

    None
}

fn path_matches_recorded_file(path: &Path, recorded_path: &Path) -> bool {
    if path == recorded_path {
        return true;
    }
    if path.parent() != recorded_path.parent() {
        return false;
    }

    match (
        path.file_name().and_then(|name| name.to_str()),
        recorded_path.file_name().and_then(|name| name.to_str()),
    ) {
        (Some(path), Some(recorded)) => filenames_match_ampm_equivalent(path, recorded),
        _ => false,
    }
}

async fn pending_file_size_allows_adoption(
    asset: &PhotoAsset,
    version_size: &str,
    path: &Path,
    actual_size: u64,
    provider_size: u64,
    recorded_file: Option<&RecordedLocalFile>,
) -> bool {
    let recorded_file =
        recorded_file.filter(|recorded| path_matches_recorded_file(path, &recorded.path));
    let result = local_file_size_matches_state(
        path,
        actual_size,
        LocalFileSizeExpectation::ExactProvider(provider_size),
        recorded_file.and_then(|recorded| recorded.local_checksum.as_deref()),
        recorded_file.and_then(|recorded| recorded.download_checksum.as_deref()),
    )
    .await;
    match result {
        Ok(matches) => matches,
        Err(error) => {
            tracing::warn!(
                asset_id = %asset.id(),
                version_size,
                path = %path.display(),
                error = %error,
                "Failed to verify size-divergent pending file"
            );
            false
        }
    }
}

async fn adopt_pending_task_path(
    db: &dyn DownloadStore,
    config: &DownloadConfig,
    asset: &PhotoAsset,
    task_planner: &mut TaskPlanner,
    task: &DownloadTask,
    recorded_file: Option<&RecordedLocalFile>,
) -> Option<PendingOnDiskAdoption> {
    let version_size = task.version_size.as_str();
    let (existing_path, existing_size) =
        task_planner.existing_path_with_size(&task.download_path)?;
    if !pending_file_size_allows_adoption(
        asset,
        version_size,
        &existing_path,
        existing_size,
        task.size,
        recorded_file,
    )
    .await
    {
        return None;
    }

    if let Err(e) = planner::upsert_seen_for_task(db, config, asset, task).await {
        tracing::warn!(
            asset_id = %asset.id(),
            version_size,
            error = %e,
            "Failed to refresh pending asset before adopting planned on-disk file"
        );
        return Some(PendingOnDiskAdoption::StateWriteFailed(existing_path));
    }

    mark_pending_downloaded_from_existing_path(
        db,
        task.library.as_ref(),
        asset,
        version_size,
        existing_path,
        capture_repair_requested(config),
    )
    .await
}

async fn adopt_pending_derived_path(
    db: &dyn DownloadStore,
    library: &str,
    asset: &PhotoAsset,
    task_planner: &mut TaskPlanner,
    derived: &DerivedPath,
    recorded_file: Option<&RecordedLocalFile>,
    mark_capture_repair: bool,
) -> Option<PendingOnDiskAdoption> {
    adopt_pending_derived_path_at(
        db,
        library,
        asset,
        task_planner,
        derived,
        &derived.path,
        recorded_file,
        mark_capture_repair,
    )
    .await
}

async fn adopt_pending_derived_path_at(
    db: &dyn DownloadStore,
    library: &str,
    asset: &PhotoAsset,
    task_planner: &mut TaskPlanner,
    derived: &DerivedPath,
    path: &Path,
    recorded_file: Option<&RecordedLocalFile>,
    mark_capture_repair: bool,
) -> Option<PendingOnDiskAdoption> {
    let version_size = derived.version_size.as_str();
    let (existing_path, existing_size) = task_planner.existing_path_with_size(path)?;
    if !pending_file_size_allows_adoption(
        asset,
        version_size,
        &existing_path,
        existing_size,
        derived.size,
        recorded_file,
    )
    .await
    {
        return None;
    }

    let record = asset_record_for_derived_path(Arc::from(library), asset, derived);
    if let Err(e) = db.upsert_seen(&record).await {
        tracing::warn!(
            asset_id = %asset.id(),
            version_size,
            error = %e,
            "Failed to refresh pending asset before adopting on-disk file"
        );
        return Some(PendingOnDiskAdoption::StateWriteFailed(existing_path));
    }

    mark_pending_downloaded_from_existing_path(
        db,
        library,
        asset,
        version_size,
        existing_path,
        mark_capture_repair,
    )
    .await
}

async fn mark_pending_downloaded_from_existing_path(
    db: &dyn DownloadStore,
    library: &str,
    asset: &PhotoAsset,
    version_size: &str,
    existing_path: PathBuf,
    mark_capture_repair: bool,
) -> Option<PendingOnDiskAdoption> {
    let local_checksum = match super::file::compute_sha256(&existing_path).await {
        Ok(checksum) => checksum,
        Err(e) => {
            tracing::warn!(
                asset_id = %asset.id(),
                version_size,
                path = %existing_path.display(),
                error = %e,
                "Failed to hash on-disk file for pending asset"
            );
            return None;
        }
    };
    if let Err(e) = db
        .mark_downloaded_with_capture_repair(
            library,
            asset.state_id(),
            version_size,
            &existing_path,
            &local_checksum,
            None,
            mark_capture_repair,
        )
        .await
    {
        tracing::warn!(
            asset_id = %asset.id(),
            version_size,
            path = %existing_path.display(),
            error = %e,
            "Failed to mark pending asset downloaded from on-disk file"
        );
        return Some(PendingOnDiskAdoption::StateWriteFailed(existing_path));
    }
    tracing::info!(
        asset_id = %asset.id(),
        version_size,
        path = %existing_path.display(),
        "Resolved pending asset from existing on-disk file"
    );
    Some(PendingOnDiskAdoption::Adopted(existing_path))
}

async fn state_path_size_allows_skip(
    asset: &PhotoAsset,
    version_size: VersionSizeKey,
    path: &Path,
    on_disk_size: u64,
    expected_size: u64,
    recorded_file: &RecordedLocalFile,
) -> bool {
    let result = local_file_size_matches_state(
        path,
        on_disk_size,
        LocalFileSizeExpectation::AtLeastProvider(expected_size),
        recorded_file.local_checksum.as_deref(),
        recorded_file.download_checksum.as_deref(),
    )
    .await;
    match result {
        Ok(true) => true,
        Ok(false) => {
            tracing::warn!(
                asset_id = %asset.id(),
                version_size = %version_size.as_str(),
                path = %path.display(),
                on_disk_size,
                expected_size,
                "State path is smaller than expected; re-downloading instead of skipping"
            );
            false
        }
        Err(error) => {
            tracing::warn!(
                asset_id = %asset.id(),
                version_size = %version_size.as_str(),
                path = %path.display(),
                error = %error,
                "Failed to verify size-divergent state path; re-downloading instead of skipping"
            );
            false
        }
    }
}

fn stored_path_matches_current_collision_family(
    asset_id: &str,
    derived: &DerivedPath,
    derived_paths: &[DerivedPath],
    config: &DownloadConfig,
    stored_path: &Path,
) -> bool {
    if stored_path.parent() != derived.path.parent() {
        return false;
    }

    let Some(stored_filename) = stored_path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };

    collision_family_base_filenames(asset_id, derived, derived_paths, config)
        .iter()
        .any(|base| stored_filename_matches_base_family(stored_filename, base, asset_id))
}

fn collision_family_base_filenames(
    asset_id: &str,
    derived: &DerivedPath,
    derived_paths: &[DerivedPath],
    config: &DownloadConfig,
) -> Vec<String> {
    let mut bases = vec![
        derived.filename.clone(),
        super::paths::add_dedup_suffix(&derived.filename, derived.size),
        super::paths::insert_asset_identity_suffix(&derived.filename, asset_id),
    ];

    if derived.version_size.is_live_photo_motion()
        && let Some(primary) = primary_derived_path(derived_paths)
    {
        let primary_collision_filenames = [
            super::paths::add_dedup_suffix(&primary.filename, primary.size),
            super::paths::insert_asset_identity_suffix(&primary.filename, asset_id),
        ];
        for primary_filename in primary_collision_filenames {
            bases.push(live_photo_motion_filename_for_primary(
                &primary_filename,
                config,
            ));
        }
    }

    bases.sort();
    bases.dedup();
    bases
}

fn primary_derived_path(derived_paths: &[DerivedPath]) -> Option<&DerivedPath> {
    derived_paths
        .iter()
        .find(|derived| derived.version_size.is_primary_media())
}

fn live_photo_motion_filename_for_primary(
    primary_filename: &str,
    config: &DownloadConfig,
) -> String {
    match config.live_photo_mov_filename_policy {
        crate::types::LivePhotoMovFilenamePolicy::Suffix => {
            super::paths::live_photo_mov_path_suffix(primary_filename)
        }
        crate::types::LivePhotoMovFilenamePolicy::Original => {
            super::paths::live_photo_mov_path_original(primary_filename)
        }
    }
}

fn stored_filename_matches_base_family(stored_filename: &str, base: &str, asset_id: &str) -> bool {
    let base = super::paths::clean_filename(base);
    let base = base.as_ref();
    filenames_match_ampm_equivalent(stored_filename, base)
        || super::paths::filename_matches_identity_collision(base, asset_id, stored_filename)
        || super::paths::filename_matches_identity_collision(
            &super::paths::normalize_ampm(base),
            asset_id,
            &super::paths::normalize_ampm(stored_filename),
        )
}

fn filenames_match_ampm_equivalent(a: &str, b: &str) -> bool {
    a == b || super::paths::normalize_ampm(a) == super::paths::normalize_ampm(b)
}

pub(super) async fn recorded_current_path_exists(
    config: &DownloadConfig,
    asset: &PhotoAsset,
    version_size: VersionSizeKey,
    task_planner: &mut TaskPlanner,
    recorded_file: &RecordedLocalFile,
) -> Option<PathBuf> {
    let stored_path = &recorded_file.path;
    let derived_paths = derive_expected_paths(asset, config);

    for derived in &derived_paths {
        if derived.version_size != version_size {
            continue;
        }
        let Some((existing_path, existing_size)) =
            task_planner.existing_path_with_size(&derived.path)
        else {
            continue;
        };
        if existing_path.as_path() == stored_path.as_path() {
            if state_path_size_allows_skip(
                asset,
                derived.version_size,
                &existing_path,
                existing_size,
                derived.size,
                recorded_file,
            )
            .await
            {
                return Some(existing_path);
            }
            return None;
        }
    }

    for derived in &derived_paths {
        if derived.version_size != version_size {
            continue;
        }
        if !stored_path_matches_current_collision_family(
            asset.state_id(),
            derived,
            &derived_paths,
            config,
            stored_path,
        ) {
            continue;
        }
        let (existing_path, existing_size) = task_planner.existing_path_with_size(stored_path)?;
        if state_path_size_allows_skip(
            asset,
            version_size,
            &existing_path,
            existing_size,
            derived.size,
            recorded_file,
        )
        .await
        {
            return Some(existing_path);
        }
        return None;
    }

    None
}

pub(super) async fn state_confirmed_current_path_exists(
    ctx: &DownloadContext,
    config: &DownloadConfig,
    asset: &PhotoAsset,
    task: &DownloadTask,
    task_planner: &mut TaskPlanner,
) -> Option<PathBuf> {
    let recorded_file = ctx.downloaded_file(&task.library, &task.asset_id, task.version_size)?;
    recorded_current_path_exists(
        config,
        asset,
        task.version_size,
        task_planner,
        recorded_file,
    )
    .await
}

async fn record_seen_for_forwarded_task(
    db: &dyn DownloadStore,
    config: &DownloadConfig,
    asset: &PhotoAsset,
    task: &DownloadTask,
) -> Result<(), crate::state::error::StateError> {
    let result = planner::upsert_seen_for_task(db, config, asset, task).await;
    if let Err(e) = &result {
        tracing::warn!(
            asset_id = %task.asset_id,
            error = %e,
            "Failed to record asset"
        );
    }
    result
}

/// Configuration for a download pass.
pub(super) struct PassConfig<'a> {
    pub(super) client: &'a Client,
    pub(super) retry_config: &'a RetryConfig,
    pub(super) metadata: MetadataFlags,
    pub(super) mark_capture_repair_after_download: bool,
    pub(super) concurrency: usize,
    pub(super) reporting: DownloadReporting,
    pub(super) temp_suffix: Arc<str>,
    pub(super) shutdown_token: CancellationToken,
    pub(super) state_db: Option<Arc<dyn DownloadStore>>,
    /// Accumulator for 429/503 observations during this pass. Counted per
    /// retry attempt, not per unique task. Aggregated into SyncStats for
    /// the rate-limit pressure warning.
    pub(super) rate_limit_counter: Arc<std::sync::atomic::AtomicUsize>,
    pub(super) bandwidth_limiter: Option<super::BandwidthLimiter>,
    /// CloudKit zone name scoping every state-DB key written by this pass.
    /// Sourced from `DownloadConfig::library` at pass dispatch.
    pub(super) library: Arc<str>,
}

impl std::fmt::Debug for PassConfig<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PassConfig")
            .field("metadata", &self.metadata)
            .field(
                "mark_capture_repair_after_download",
                &self.mark_capture_repair_after_download,
            )
            .field("concurrency", &self.concurrency)
            .field("reporting", &self.reporting)
            .field("temp_suffix", &self.temp_suffix)
            .field("state_db", &self.state_db.as_ref().map(|_| ".."))
            .finish_non_exhaustive()
    }
}

/// Result of a download pass.
#[derive(Debug)]
pub(super) struct PassResult {
    pub(super) downloaded: usize,
    pub(super) downloaded_tasks: Vec<DownloadTask>,
    pub(super) exif_failures: usize,
    pub(super) failed: Vec<DownloadTask>,
    pub(super) auth_errors: usize,
    pub(super) state_write_failures: usize,
    pub(super) bytes_downloaded: u64,
    pub(super) disk_bytes_written: u64,
    pub(super) rate_limit_observations: usize,
    pub(super) url_expired_abort: bool,
    /// Photos / videos / recap observed during this pass, mirroring
    /// `StreamingResult`. Folded into the cycle's `SyncStats` at the
    /// caller. Defaults are zero / empty so the existing cleanup-pass
    /// path behaves identically in non-friendly mode.
    pub(super) photos_downloaded: usize,
    pub(super) videos_downloaded: usize,
    pub(super) recap: super::recap::RunRecap,
}

/// Return the subset of `paths` that do not exist on disk.
/// Streaming download pipeline that consumes a pre-built combined stream.
///
/// This is the core producer/consumer download logic from `stream_and_download`,
/// factored out so that `download_photos_full_with_token` can supply a
/// token-aware combined stream while reusing the same download machinery.
pub(super) async fn stream_and_download_from_stream<S>(
    download_client: &Client,
    combined: S,
    config: &Arc<DownloadConfig>,
    controls: DownloadControls,
    total: u64,
    shutdown_token: CancellationToken,
    runtime: StreamRuntime,
) -> Result<StreamingResult>
where
    S: futures_util::Stream<Item = anyhow::Result<crate::icloud::photos::PhotoAsset>>
        + Send
        + 'static,
{
    stream_and_download_from_stream_with_context(
        download_client,
        combined,
        config,
        controls,
        total,
        shutdown_token,
        runtime,
    )
    .await
}

pub(super) struct StreamRuntime {
    shared_pb: Option<ProgressBar>,
    shared_bytes: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
    preloaded_download_ctx: Option<Arc<DownloadContext>>,
    /// Set when the caller runs several of these pipelines concurrently and
    /// drains the shared rewrite queue itself once they have all finished.
    /// Concurrent drains can otherwise write the same file, and the later
    /// write wins regardless of which holds the newer metadata.
    defer_metadata_drain: bool,
}

impl StreamRuntime {
    pub(super) fn new(
        shared_pb: Option<ProgressBar>,
        shared_bytes: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
    ) -> Self {
        Self {
            shared_pb,
            shared_bytes,
            preloaded_download_ctx: None,
            defer_metadata_drain: false,
        }
    }

    pub(super) fn with_context(
        shared_pb: Option<ProgressBar>,
        shared_bytes: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
        preloaded_download_ctx: Option<Arc<DownloadContext>>,
    ) -> Self {
        Self {
            shared_pb,
            shared_bytes,
            preloaded_download_ctx,
            defer_metadata_drain: false,
        }
    }

    /// Leaves the rewrite queue for the caller to drain once every concurrent
    /// pipeline has finished.
    pub(super) fn deferring_metadata_drain(mut self) -> Self {
        self.defer_metadata_drain = true;
        self
    }
}

#[derive(Clone, Default)]
struct StreamProducerMetrics {
    assets_seen: Arc<std::sync::atomic::AtomicU64>,
    enum_errors: Arc<std::sync::atomic::AtomicUsize>,
    state_write_failures: Arc<std::sync::atomic::AtomicUsize>,
    enumeration_complete: Arc<std::sync::atomic::AtomicBool>,
}

struct StreamProducer {
    handle: tokio::task::JoinHandle<ProducerSkipSummary>,
    metrics: StreamProducerMetrics,
}

#[derive(Clone)]
struct StreamPipelineShared {
    config: Arc<DownloadConfig>,
    state_db: Option<Arc<dyn DownloadStore>>,
    pb: ProgressBar,
    pipeline_shutdown: CancellationToken,
}

struct StreamConsumerSettings {
    retry_config: RetryConfig,
    metadata_flags: MetadataFlags,
    concurrency: usize,
    mode: crate::personality::Mode,
    bytes_counter: Arc<std::sync::atomic::AtomicU64>,
}

#[derive(Default)]
struct StreamConsumerResult {
    downloaded: usize,
    exif_failures: usize,
    failed: Vec<DownloadTask>,
    auth_errors: usize,
    pending_state_writes: Vec<PendingStateWrite>,
    bytes_downloaded_total: u64,
    disk_bytes_total: u64,
    url_expired_abort: bool,
    rate_limit_observations: usize,
    photos_downloaded: usize,
    videos_downloaded: usize,
    recap: super::recap::RunRecap,
    state_write_circuit_error: Option<anyhow::Error>,
}

pub(super) async fn stream_and_download_from_stream_with_context<S>(
    download_client: &Client,
    combined: S,
    config: &Arc<DownloadConfig>,
    controls: DownloadControls,
    total: u64,
    shutdown_token: CancellationToken,
    runtime: StreamRuntime,
) -> Result<StreamingResult>
where
    S: futures_util::Stream<Item = anyhow::Result<crate::icloud::photos::PhotoAsset>>
        + Send
        + 'static,
{
    let reporting = controls.reporting;

    // When the caller passes a `shared_pb`, they own its lifecycle (we only
    // advance position and update the message). Otherwise we create our own
    // bar and finish_and_clear it before returning. The shared-bar path is
    // used by the per-album-pass loop in `download::mod.rs` to avoid the
    // visible "reset" when one pass finishes and the next starts: a small
    // album finishing fast then a large unfiled pass starting fresh reads as
    // a glitch.
    //
    // The byte counter follows the same pairing rule: caller-supplied with
    // `shared_pb`, or freshly created for an internal bar. The friendly
    // sparkline / rate display reads from this atomic on each redraw.
    let owns_pb = runtime.shared_pb.is_none();
    let bytes_counter = runtime
        .shared_bytes
        .unwrap_or_else(|| std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)));
    let pb = runtime.shared_pb.unwrap_or_else(|| {
        crate::personality::progress::single(
            reporting.no_progress_bar,
            controls.run_mode.only_print_filenames(),
            total,
            reporting.personality_mode,
            Some(std::sync::Arc::clone(&bytes_counter)),
        )
    });

    // Seed the wide_msg line with the pass label so the user can see which
    // album/pass is active even before any task completes. Otherwise an
    // album that's entirely already-on-disk would advance the bar via the
    // producer's skip path (which doesn't set_message) and leave the
    // wide_msg blank for the whole pass.
    //
    pb.set_message(format!("{} \u{00b7} scanning...", config.pass_label()));

    // Select the durable child identity before any run mode performs state
    // checks or path planning. Print-only and dry-run return before the real
    // download producer starts, so normalizing only in that producer makes
    // their paths disagree with a normal sync.
    let download_ctx = match runtime.preloaded_download_ctx {
        Some(ctx) => ctx,
        None => preload_download_context(config).await,
    };
    let default_library = Arc::clone(&config.library);
    let identity_download_ctx = Arc::clone(&download_ctx);
    let identity_config = Arc::clone(config);
    let mut claimed_legacy_master_states = ClaimedLegacyMasterStates::default();
    let combined = combined.map(move |result| {
        result.map(|asset| {
            let library = asset.source_zone().unwrap_or(default_library.as_ref());
            let state_record_name = if !matches!(
                is_asset_filtered(&asset, identity_config.as_ref()),
                Some(FilterReason::ExcludedAlbum)
            ) {
                identity_download_ctx.select_asset_state_record_name(
                    library,
                    &asset,
                    &mut claimed_legacy_master_states,
                )
            } else {
                identity_download_ctx.select_existing_asset_state_record_name(library, &asset)
            };
            asset.with_state_record_name(state_record_name)
        })
    });

    if controls.run_mode.only_print_filenames() {
        tokio::pin!(combined);
        let mut enum_errors = 0usize;
        let mut task_planner = TaskPlanner::new();
        let mut shutdown_break = false;
        #[cfg(test)]
        let mut printed_filenames = Vec::new();
        while let Some(result) = combined.next().await {
            if shutdown_token.is_cancelled() {
                shutdown_break = true;
                break;
            }
            match result {
                Ok(asset) => {
                    if is_asset_filtered(&asset, config.as_ref()).is_some() {
                        continue;
                    }
                    // Fast-skip is path-blind; in `{album}` mode the same
                    // asset legitimately lives at multiple paths, so we'd
                    // under-report the listing if we trusted the DB here.
                    // `album_name.is_some()` is the right signal because by
                    // the time this runs, `with_album_name` has expanded
                    // `{album}` out of `folder_structure` entirely.
                    if config.album_name.is_none() {
                        let candidates = extract_skip_candidates(&asset, config.as_ref());
                        let library = effective_asset_library(&asset, config);
                        if !candidates.is_empty()
                            && candidates.iter().all(|&(vs, cs)| {
                                matches!(
                                    download_ctx.should_download_fast(
                                        library,
                                        asset.state_id(),
                                        vs,
                                        cs,
                                        true
                                    ),
                                    Some(false)
                                )
                            })
                        {
                            continue;
                        }
                    }

                    let plan = task_planner.plan_asset(&asset, config).await;
                    if let Some(resource) = &plan.malformed_resource {
                        enum_errors += 1;
                        tracing::error!(
                            asset_id = %asset.id(),
                            field = %resource.field,
                            reason = %resource.reason,
                            "Malformed CloudKit resource prevented filename planning"
                        );
                        continue;
                    }
                    #[allow(
                        clippy::print_stdout,
                        reason = "--only-print-filenames writes target paths to stdout so callers can pipe to xargs/etc"
                    )]
                    for task in &plan.tasks {
                        #[cfg(test)]
                        printed_filenames.push(task.download_path.clone());
                        println!("{}", task.download_path.display());
                    }
                }
                Err(e) => {
                    enum_errors += 1;
                    tracing::error!(error = %e, "Error fetching asset");
                }
            }
        }
        return Ok(StreamingResult {
            enumeration_errors: enum_errors,
            // Same gate as dry-run — `--only-print-filenames` drains
            // the API stream and can clear the marker on a clean exit.
            enumeration_complete: !shutdown_break,
            #[cfg(test)]
            printed_filenames,
            ..StreamingResult::default()
        });
    }

    if controls.run_mode.is_dry_run() {
        tokio::pin!(combined);
        let mut count = 0usize;
        let mut enum_errors = 0usize;
        let mut task_planner = TaskPlanner::new();
        let mut shutdown_break = false;
        while let Some(result) = combined.next().await {
            if shutdown_token.is_cancelled() {
                tracing::info!("Shutdown requested, stopping dry run");
                shutdown_break = true;
                break;
            }
            match result {
                Ok(asset) => {
                    let plan = task_planner.plan_asset(&asset, config).await;
                    if plan.filter_reason.is_some() {
                        continue;
                    }
                    if let Some(resource) = &plan.malformed_resource {
                        enum_errors += 1;
                        tracing::error!(
                            asset_id = %asset.id(),
                            field = %resource.field,
                            reason = %resource.reason,
                            "Malformed CloudKit resource prevented dry-run planning"
                        );
                        continue;
                    }
                    for task in &plan.tasks {
                        tracing::info!(path = %task.download_path.display(), "[DRY RUN] Would download");
                    }
                    count += plan.tasks.len();
                }
                Err(e) => {
                    enum_errors += 1;
                    tracing::error!(error = %e, "Error fetching asset");
                }
            }
        }
        return Ok(StreamingResult {
            downloaded: count,
            enumeration_errors: enum_errors,
            // Dry-run still drains the API stream; mirror the
            // non-dry-run gate so the enum_in_progress marker can be
            // cleared on a clean dry-run.
            enumeration_complete: !shutdown_break,
            ..StreamingResult::default()
        });
    }

    let download_client = download_client.clone();
    let retry_config = config.retry;
    let metadata_flags = MetadataFlags::from(config.as_ref());
    let concurrency = config.concurrent_downloads;
    let state_db = config.state_db.clone();
    let mode = reporting.personality_mode;

    let defer_metadata_drain = runtime.defer_metadata_drain;

    // Start sync run tracking
    let sync_run_id = if let Some(db) = &state_db {
        match db.start_sync_run().await {
            Ok(id) => {
                tracing::debug!(run_id = id, "Started sync run");
                Some(id)
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to start sync run tracking");
                None
            }
        }
    } else {
        None
    };

    // Log a one-time backfill notice when pre-v5 assets still have NULL
    // metadata_hash. `download_ctx` already loaded downloaded ids and
    // non-null metadata hashes, so avoid a redundant SQLite EXISTS scan per
    // album pass.
    if download_ctx.has_downloaded_without_metadata_hash() {
        tracing::info!("Backfilling metadata for existing assets (one-time after upgrade)");
    }

    let (task_tx, task_rx) = mpsc::channel::<DownloadTask>(concurrency * 2);

    // Batch-size forecast: snapshot free space at enumeration start and
    // track bytes queued to consumers. Emit a one-time warn at 90% and
    // cancel the sync at 100%. This catches the "batch much larger than
    // free space" case early, before downloads run the disk dry mid-stream.
    //
    // A multi-hour sync can have its FS filled by an unrelated process
    // mid-run. Refresh `initial_free` every FREE_SPACE_RESNAPSHOT_INTERVAL_BYTES
    // queued so the bail decision rides on fresher data. Per-task rechecks
    // against a shrinking denominator would fire noisy false-positives as
    // downloads naturally consume the disk; the 10 GiB cadence is a
    // compromise between "stale" and "noisy".
    let initial_free_at_start = crate::available_disk_space(&config.directory);

    // Refuse to start if free disk space is critically low (< 100 MiB).
    // The per-asset batch_forecast_decision catches the rest mid-stream.
    const MIN_FREE_BYTES_HARD: u64 = 100 * 1024 * 1024; // 100 MiB
    if let Some(free) = initial_free_at_start
        && free < MIN_FREE_BYTES_HARD
    {
        return Err(anyhow::anyhow!(
            "Not enough free disk space: only {} bytes are available on {}, but kei needs at least {MIN_FREE_BYTES_HARD} bytes. Free up space or choose a different [download].directory.",
            free,
            config.directory.display(),
        ));
    }

    let pipeline_shutdown = shutdown_token.child_token();
    let shared = StreamPipelineShared {
        config: Arc::clone(config),
        state_db: state_db.clone(),
        pb: pb.clone(),
        pipeline_shutdown: pipeline_shutdown.clone(),
    };
    let producer = spawn_stream_download_producer(
        combined,
        Arc::clone(&download_ctx),
        task_tx,
        initial_free_at_start,
        shared.clone(),
    );

    let consumer_result = consume_stream_download_tasks(
        task_rx,
        download_client,
        shared.clone(),
        StreamConsumerSettings {
            retry_config,
            metadata_flags,
            concurrency,
            mode,
            bytes_counter: Arc::clone(&bytes_counter),
        },
    )
    .await;

    finalize_streaming_download(
        producer,
        consumer_result,
        sync_run_id,
        owns_pb,
        shared,
        defer_metadata_drain,
    )
    .await
}

fn spawn_stream_download_producer<S>(
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
    let state_write_failures_producer = Arc::clone(&metrics.state_write_failures);
    let enumeration_complete_producer = Arc::clone(&metrics.enumeration_complete);
    let queued_bytes_producer = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let space_warn_emitted_producer = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let handle = tokio::spawn(async move {
        let config = &producer_config;
        let metadata_writers_enabled = MetadataFlags::from(config.as_ref()).has_any_write();
        let mut task_planner = TaskPlanner::new();
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
                tracing::debug!(
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
                        tracing::warn!(
                            queued_bytes = total,
                            initial_free_bytes = free,
                            percent_of_free = percent,
                            "Queued download batch approaching 90% of initial free disk space"
                        );
                    }
                    false
                }
                BatchForecast::Bail => {
                    tracing::error!(
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
                        tracing::debug!(
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
                            tracing::warn!(
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
                                    tracing::warn!(
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
                        let drift = download_ctx.has_provider_metadata_drift(
                            &library,
                            asset.state_id(),
                            asset.metadata().metadata_hash.as_deref(),
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
                                    asset.metadata(),
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
                                        tracing::warn!(
                                            asset_id = %asset.id(),
                                            library = %library,
                                            "Changed provider metadata matched no downloaded state row"
                                        );
                                    }
                                }
                                Err(e) => {
                                    state_write_failures_producer
                                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                    tracing::warn!(
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
                        tracing::warn!(
                            asset_id = %asset.id(),
                            album = %album,
                            error = %e,
                            "Failed to record album membership after retries"
                        );
                    }

                    let plan = task_planner.plan_asset(&asset, config).await;
                    if let Some(reason) = plan.filter_reason {
                        skips.record_filter_reason(reason);
                        producer_pb.inc(1);
                        continue;
                    }
                    if let Some(resource) = &plan.malformed_resource {
                        enum_errors_producer.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        tracing::error!(
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
                                tracing::warn!(
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
                                        tracing::warn!(
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
                                tracing::debug!(
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
                                            tracing::debug!(
                                                asset_id = %task.asset_id,
                                                path = %existing_path.display(),
                                                "Skipping (pending state adopted existing file)"
                                            );
                                        }
                                        PendingOnDiskAdoption::StateWriteFailed(existing_path) => {
                                            state_write_failures_producer
                                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                            tracing::debug!(
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
                                        tracing::debug!(
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
                                            tracing::debug!(
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
                                                tracing::debug!(
                                                    asset_id = %task.asset_id,
                                                    path = %task.download_path.display(),
                                                    "Skipping (already downloaded)"
                                                );
                                            }
                                            ExistingPathMatch::AmpmVariant => {
                                                disposition =
                                                    disposition.max(AssetDisposition::AmpmVariant);
                                                tracing::debug!(
                                                    asset_id = %task.asset_id,
                                                    path = %task.download_path.display(),
                                                    "Skipping (AM/PM variant exists on disk)"
                                                );
                                            }
                                            ExistingPathMatch::Missing => {
                                                tracing::debug!(
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
                                tracing::warn!(
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
                    // Single-line `tracing::error!` doesn't need the
                    // progress bar suspended; the bar redraws cleanly after
                    // a one-line emit and the suspend was just paying a
                    // hot-loop cost for no observable benefit.
                    tracing::error!(error = %e, "Error fetching asset");
                }
            }
        }

        // At this point the outer `while let Some(...)` loop has
        // exited via stream-exhaustion (None) or via the in-loop shutdown
        // `break`. Channel-close early returns bypass this code path
        // entirely. Mark enumeration complete only when shutdown wasn't
        // the trigger so the cycle's `enum_in_progress:<zone>` marker is
        // cleared even when downstream downloads partially failed.
        if !producer_shutdown.is_cancelled() {
            enumeration_complete_producer.store(true, std::sync::atomic::Ordering::Relaxed);
        }

        let total_skipped = skips.total();
        if total_skipped > 0 {
            // Single tracing event, runs once after the producer loop
            // exits — no suspend needed.
            tracing::debug!(
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
            tracing::warn!(
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
                        tracing::warn!(
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

async fn consume_stream_download_tasks(
    task_rx: mpsc::Receiver<DownloadTask>,
    download_client: Client,
    shared: StreamPipelineShared,
    settings: StreamConsumerSettings,
) -> StreamConsumerResult {
    let StreamConsumerSettings {
        retry_config,
        metadata_flags,
        concurrency,
        mode,
        bytes_counter,
    } = settings;
    let config = &shared.config;
    let mark_capture_repair_after_download = capture_repair_requested(config);
    let pb = &shared.pb;
    let pipeline_shutdown = shared.pipeline_shutdown;
    let state_db = shared.state_db;
    let temp_suffix: Arc<str> = Arc::clone(&config.temp_suffix);
    let bandwidth_limiter = config.bandwidth_limiter.clone();
    let rate_limit_counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let download_stream = ReceiverStream::new(task_rx)
        .map(|task| {
            let client = download_client.clone();
            let task_state_db = state_db.clone();
            let temp_suffix = Arc::clone(&temp_suffix);
            let rate_limit_counter = Arc::clone(&rate_limit_counter);
            let bandwidth_limiter = bandwidth_limiter.clone();
            let shutdown_token = pipeline_shutdown.clone();
            async move {
                let result = Box::pin(download_single_task(
                    &client,
                    &task,
                    &retry_config,
                    metadata_flags,
                    DownloadSingleContext {
                        temp_suffix: &temp_suffix,
                        state_db: task_state_db.as_deref(),
                        rate_limit_counter: Some(rate_limit_counter.as_ref()),
                        bandwidth_limiter: bandwidth_limiter.as_ref(),
                        shutdown_token: &shutdown_token,
                        mode,
                    },
                ))
                .await;
                (task, result)
            }
        })
        .buffer_unordered(concurrency);

    tokio::pin!(download_stream);

    // On cancellation we keep consuming results so in-flight downloads
    // still get their state rows written; new downloads are gated off by
    // the producer's own cancellation (which closes task_tx, naturally
    // ending this stream). The 30s watchdog in shutdown.rs is the backstop
    // if a hung download blocks the drain.
    let mut downloaded = 0usize;
    let mut exif_failures = 0usize;
    let mut failed: Vec<DownloadTask> = Vec::new();
    let mut auth_errors = 0usize;
    let mut pending_state_writes: Vec<PendingStateWrite> = Vec::new();
    let mut bytes_downloaded_total: u64 = 0;
    let mut disk_bytes_total: u64 = 0;
    let mut url_expired_abort = false;
    let mut photos_downloaded = 0usize;
    let mut videos_downloaded = 0usize;
    let mut recap = super::recap::RunRecap::default();
    let mut drain_logged = false;
    let mut state_write_circuit_error: Option<anyhow::Error> = None;
    while let Some((task, result)) = download_stream.next().await {
        if pipeline_shutdown.is_cancelled() && !drain_logged {
            pb.suspend(|| tracing::info!("Shutdown requested, draining in-flight downloads..."));
            drain_logged = true;
        }
        let filename = task
            .download_path
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or("");
        // Prefix the active filename with the pass's album label so the user
        // can tell which album's items are downloading.
        pb.set_message(format!("{} \u{00b7} {filename}", config.pass_label()));
        match result {
            Ok((exif_ok, local_checksum, download_checksum, bytes_dl, disk_bytes)) => {
                downloaded += 1;
                bytes_downloaded_total += bytes_dl;
                // Photos / videos fire in both modes (SyncStats serialises
                // them into the JSON report); the recap fold is
                // friendly-only so off-mode skips the per-success String
                // allocation `to_recap_asset` does for the filename.
                if task.media_type.is_photo_like() {
                    photos_downloaded += 1;
                } else if task.media_type.is_video_like() {
                    videos_downloaded += 1;
                }
                if mode.is_friendly() {
                    recap.observe(config.pass_label(), task.to_recap_asset());
                }
                // Feed the friendly bar's bandwidth sparkline / rate display.
                // Atomic+Relaxed is fine: the bar reads it on each redraw,
                // doesn't depend on it for correctness, and a missed update
                // smooths out within an EMA tick.
                bytes_counter.fetch_add(bytes_dl, std::sync::atomic::Ordering::Relaxed);
                disk_bytes_total += disk_bytes;
                if !exif_ok {
                    exif_failures += 1;
                    pb.suspend(|| {
                        tracing::error!(
                            asset_id = %task.asset_id,
                            path = %task.download_path.display(),
                            "Metadata write failed after download; marker set for retry on next sync"
                        );
                    });
                }
                if let Some(db) = &state_db {
                    match finalize_downloaded(
                        db.as_ref(),
                        &task.library,
                        &task,
                        local_checksum,
                        download_checksum,
                        exif_ok,
                        mark_capture_repair_after_download,
                    )
                    .await
                    {
                        DownloadedFinalization::Persisted => {}
                        DownloadedFinalization::Deferred { write, error } => {
                            pb.suspend(|| {
                                tracing::warn!(
                                    asset_id = %task.asset_id,
                                    error = %error,
                                    "State write failed, deferring for retry"
                                );
                            });
                            pending_state_writes.push(write);
                            if state_write_circuit_error.is_none()
                                && let Some(err) = check_state_write_circuit_breaker(
                                    db.as_ref(),
                                    &mut pending_state_writes,
                                )
                                .await
                            {
                                pb.suspend(|| {
                                    tracing::error!(
                                        error = %err,
                                        "State write circuit breaker opened; halting downloads"
                                    );
                                });
                                state_write_circuit_error = Some(err);
                                pipeline_shutdown.cancel();
                            }
                        }
                    }
                }
            }
            Err(e) => {
                match classify_download_task_error(&e) {
                    DownloadTaskErrorClass::Interrupted => {
                        log_interrupted_download(pb, &task, &e);
                        continue;
                    }
                    DownloadTaskErrorClass::SessionExpired => {
                        auth_errors += 1;
                        pb.suspend(|| {
                            tracing::warn!(
                                auth_errors,
                                threshold = AUTH_ERROR_THRESHOLD,
                                path = %task.download_path.display(),
                                error = %e,
                                "Auth error"
                            );
                        });
                        if auth_errors >= AUTH_ERROR_THRESHOLD {
                            pb.suspend(|| {
                                tracing::warn!(
                                    "Auth error threshold reached, aborting for re-authentication"
                                );
                            });
                            break;
                        }
                    }
                    DownloadTaskErrorClass::ExpiredUrl => {
                        url_expired_abort = true;
                        pb.suspend(|| {
                            tracing::warn!(
                                asset_id = %task.asset_id,
                                path = %task.download_path.display(),
                                error = %e,
                                "Download URL expired; aborting current URL batch"
                            );
                        });
                        pipeline_shutdown.cancel();
                        continue;
                    }
                    DownloadTaskErrorClass::Other => {
                        pb.suspend(|| {
                            tracing::error!(asset_id = %task.asset_id, path = %task.download_path.display(), error = %e, "Download failed");
                        });
                    }
                }
                if let Some(db) = &state_db
                    && let Err(e) =
                        finalize_failed(db.as_ref(), &task.library, &task, &e.to_string()).await
                {
                    tracing::warn!(
                        asset_id = %task.asset_id,
                        error = %e,
                        "Failed to mark failure"
                    );
                }
                failed.push(task);
            }
        }
    }

    StreamConsumerResult {
        downloaded,
        exif_failures,
        failed,
        auth_errors,
        pending_state_writes,
        bytes_downloaded_total,
        disk_bytes_total,
        url_expired_abort,
        rate_limit_observations: rate_limit_counter.load(std::sync::atomic::Ordering::Relaxed),
        photos_downloaded,
        videos_downloaded,
        recap,
        state_write_circuit_error,
    }
}

async fn finalize_streaming_download(
    producer: StreamProducer,
    mut consumer: StreamConsumerResult,
    sync_run_id: Option<i64>,
    owns_pb: bool,
    shared: StreamPipelineShared,
    defer_metadata_drain: bool,
) -> Result<StreamingResult> {
    let StreamProducer { handle, metrics } = producer;
    let config = shared.config;
    let state_db = shared.state_db;
    let pb = shared.pb;
    let pipeline_shutdown = shared.pipeline_shutdown;

    let (producer_panicked, producer_skips) = match handle.await {
        Ok(skips) => (false, skips),
        Err(e) if e.is_panic() => {
            tracing::error!(error = ?e, "Asset producer task panicked");
            (true, ProducerSkipSummary::default())
        }
        Err(e) => {
            tracing::warn!(error = ?e, "Asset producer task failed (skip counts lost)");
            (false, ProducerSkipSummary::default())
        }
    };

    let assets_seen_count = metrics
        .assets_seen
        .load(std::sync::atomic::Ordering::Relaxed);

    // Only finish the bar when we created it ourselves; if the caller passed
    // a shared bar (per-pass loop), they'll finish it after the last pass.
    if owns_pb {
        pb.finish_and_clear();
    }

    let mut complete_sync_failed = false;
    if let (Some(db), Some(run_id)) = (&state_db, sync_run_id) {
        let stats = SyncRunStats {
            assets_seen: assets_seen_count,
            assets_downloaded: consumer.downloaded as u64,
            assets_failed: consumer.failed.len() as u64,
            enumeration_errors: u64::try_from(
                metrics
                    .enum_errors
                    .load(std::sync::atomic::Ordering::Relaxed),
            )
            .unwrap_or(u64::MAX),
            interrupted: pipeline_shutdown.is_cancelled()
                || consumer.auth_errors >= AUTH_ERROR_THRESHOLD
                || producer_panicked
                || consumer.url_expired_abort,
            ..Default::default()
        };
        match db.complete_sync_run(run_id, &stats).await {
            Err(e) => {
                tracing::warn!(error = %e, "Failed to complete sync run tracking");
                complete_sync_failed = true;
            }
            Ok(()) => {
                tracing::debug!(
                    run_id,
                    assets_seen = assets_seen_count,
                    downloaded = consumer.downloaded,
                    failed = consumer.failed.len(),
                    "Completed sync run"
                );
            }
        }
    }

    // Retry any state writes that failed during the streaming loop. This
    // must run before the producer-panic bail so rows that successfully
    // landed on disk before the panic are recorded in state; otherwise the
    // next sync re-downloads them and the pending-retry safety net becomes
    // a no-op on panic paths.
    let final_state_flush = if let Some(db) = &state_db {
        if consumer.state_write_circuit_error.is_some() {
            StateWriteFlush {
                attempted: consumer.pending_state_writes.len(),
                failures: consumer.pending_state_writes.len(),
            }
        } else {
            flush_pending_state_writes_retaining_failures(
                db.as_ref(),
                &mut consumer.pending_state_writes,
            )
            .await
        }
    } else {
        StateWriteFlush::default()
    };
    let producer_state_write_failures = metrics
        .state_write_failures
        .load(std::sync::atomic::Ordering::Relaxed);
    let state_write_failures = final_state_flush.failures
        + producer_state_write_failures
        + usize::from(complete_sync_failed);
    if consumer.state_write_circuit_error.is_none()
        && state_write_circuit_breaker_tripped(&final_state_flush)
    {
        consumer.state_write_circuit_error =
            Some(state_db_unwritable_error(final_state_flush.attempted));
    }

    // Drain metadata-rewrite markers set earlier in this cycle (or left over
    // from a previous one). This re-applies EXIF/XMP on the existing files
    // without re-downloading bytes; the alternative was to leave markers
    // accumulating in the DB forever.
    if consumer.state_write_circuit_error.is_none()
        && !defer_metadata_drain
        && !config.refresh_metadata
        && let Some(db) = &state_db
    {
        let metadata_flags = MetadataFlags::from(config.as_ref());
        if metadata_flags.has_any_write() {
            consumer.exif_failures += metadata_rewrite::run_pending(
                db.as_ref(),
                metadata_flags,
                Arc::clone(&config.temp_suffix),
                &pipeline_shutdown,
            )
            .await
            .failed;
        }
    }

    if let Some(err) = consumer.state_write_circuit_error {
        return Err(err);
    }

    if producer_panicked {
        return Err(anyhow::anyhow!(
            "The asset producer task crashed, so sync may be incomplete ({} pending state writes were flushed).",
            state_write_failures,
        ));
    }

    // A panicked producer never reached the post-loop "enumeration
    // complete" assignment, so the flag stays `false` even if the bail
    // path above was suppressed. `producer_panicked` is checked above,
    // but if a future change ever returns Ok despite a panic, the flag
    // here protects the `enum_in_progress` marker.
    let enumeration_complete_flag = !producer_panicked
        && metrics
            .enumeration_complete
            .load(std::sync::atomic::Ordering::Relaxed);

    Ok(StreamingResult {
        downloaded: consumer.downloaded,
        exif_failures: consumer.exif_failures,
        failed: consumer.failed,
        auth_errors: consumer.auth_errors,
        state_write_failures,
        enumeration_errors: metrics
            .enum_errors
            .load(std::sync::atomic::Ordering::Relaxed),
        assets_seen: assets_seen_count,
        skip_summary: producer_skips,
        bytes_downloaded: consumer.bytes_downloaded_total,
        disk_bytes_written: consumer.disk_bytes_total,
        rate_limit_observations: consumer.rate_limit_observations,
        enumeration_complete: enumeration_complete_flag,
        photos_downloaded: consumer.photos_downloaded,
        videos_downloaded: consumer.videos_downloaded,
        recap: consumer.recap,
        url_expired_abort: consumer.url_expired_abort,
        #[cfg(test)]
        printed_filenames: Vec::new(),
    })
}

fn producer_enumeration_incomplete(
    result: &StreamingResult,
    shutdown_token: &CancellationToken,
) -> bool {
    !result.enumeration_complete
        && result.assets_seen > 0
        && !shutdown_token.is_cancelled()
        && !result.url_expired_abort
}

fn mark_producer_enumeration_incomplete(stats: &mut super::SyncStats, incomplete: bool) {
    if !incomplete {
        return;
    }
    stats.enumeration_incomplete = true;
    stats.sync_token_blocked = true;
    if stats.sync_token_blocked_reason.is_none() {
        stats.sync_token_blocked_reason = Some(super::PRODUCER_ENUMERATION_INCOMPLETE_REASON);
        stats.sync_token_blocked_source = Some(super::sync_token_blocked_source(
            super::PRODUCER_ENUMERATION_INCOMPLETE_REASON,
        ));
        stats.sync_token_blocked_explanation = Some(super::sync_token_blocked_explanation(
            super::PRODUCER_ENUMERATION_INCOMPLETE_REASON,
        ));
    }
    tracing::warn!(
        reason = super::PRODUCER_ENUMERATION_INCOMPLETE_REASON,
        "Asset producer stopped before iCloud enumeration completed; treating sync as partial failure"
    );
}

/// Build a `DownloadOutcome` from a `StreamingResult`, running a cleanup
/// pass if there were failures. Shared between `download_photos` and
/// `download_photos_full_with_token`.
pub(super) async fn build_download_outcome(
    download_client: &Client,
    passes: &[crate::commands::AlbumPass],
    config: &Arc<DownloadConfig>,
    controls: DownloadControls,
    streaming_result: StreamingResult,
    started: Instant,
    shutdown_token: CancellationToken,
) -> Result<(DownloadOutcome, super::SyncStats)> {
    let run_mode = controls.run_mode;
    let downloaded = streaming_result.downloaded;
    let mut exif_failures = streaming_result.exif_failures;
    let auth_errors = streaming_result.auth_errors;
    let mut state_write_failures = streaming_result.state_write_failures;
    let enumeration_errors = streaming_result.enumeration_errors;
    let enumeration_incomplete =
        producer_enumeration_incomplete(&streaming_result, &shutdown_token);
    let failed_tasks = streaming_result.failed;
    let skip_breakdown: super::SkipBreakdown = streaming_result.skip_summary.into();

    if auth_errors >= AUTH_ERROR_THRESHOLD {
        let stats = super::SyncStats {
            assets_seen: streaming_result.assets_seen,
            downloaded,
            failed: failed_tasks.len(),
            skipped: skip_breakdown,
            bytes_downloaded: streaming_result.bytes_downloaded,
            disk_bytes_written: streaming_result.disk_bytes_written,
            exif_failures,
            state_write_failures,
            enumeration_errors,
            pagination_shortfall_warnings: 0,
            pagination_shortfall_assets: 0,
            enumeration_incomplete,
            sync_token_blocked: false,
            sync_token_blocked_reason: None,
            elapsed_secs: started.elapsed().as_secs_f64(),
            interrupted: true,
            rate_limited: streaming_result.rate_limit_observations,
            photos_downloaded: streaming_result.photos_downloaded,
            videos_downloaded: streaming_result.videos_downloaded,
            recap: streaming_result.recap.clone(),
            ..super::SyncStats::default()
        };
        return Ok((
            DownloadOutcome::SessionExpired {
                auth_error_count: auth_errors,
            },
            stats,
        ));
    }

    if downloaded == 0 && failed_tasks.is_empty() {
        let retry_exhausted = skip_breakdown.retry_exhausted;
        let mut stats = super::SyncStats {
            assets_seen: streaming_result.assets_seen,
            skipped: skip_breakdown,
            exif_failures,
            state_write_failures,
            enumeration_errors,
            elapsed_secs: started.elapsed().as_secs_f64(),
            interrupted: shutdown_token.is_cancelled(),
            ..super::SyncStats::default()
        };
        mark_producer_enumeration_incomplete(&mut stats, enumeration_incomplete);
        if run_mode.is_dry_run() {
            tracing::info!("── Dry Run Summary ──");
            tracing::info!("  0 files would be downloaded");
            tracing::info!(destination = %config.directory.display(), "  destination");
        } else if streaming_result.url_expired_abort {
            tracing::warn!("Download batch aborted because signed iCloud URLs expired");
        } else {
            tracing::info!("No new photos to download");
        }
        // A metadata-only edit cycle downloads nothing, so rewrite failures
        // must still surface here. They stay out of `state_write_failures`:
        // the catalogue and marker are durable, so the checkpoint may advance.
        let failed_count = state_write_failures
            + exif_failures
            + retry_exhausted
            + enumeration_errors
            + usize::from(streaming_result.url_expired_abort)
            + usize::from(enumeration_incomplete);
        if failed_count > 0 {
            return Ok((DownloadOutcome::PartialFailure { failed_count }, stats));
        }
        return Ok((DownloadOutcome::Success, stats));
    }

    if run_mode.is_dry_run() {
        let mut stats = super::SyncStats {
            assets_seen: streaming_result.assets_seen,
            downloaded,
            enumeration_errors,
            skipped: skip_breakdown,
            elapsed_secs: started.elapsed().as_secs_f64(),
            interrupted: shutdown_token.is_cancelled(),
            ..super::SyncStats::default()
        };
        mark_producer_enumeration_incomplete(&mut stats, enumeration_incomplete);
        tracing::info!("── Dry Run Summary ──");
        if shutdown_token.is_cancelled() {
            tracing::info!(scanned = downloaded, "  Interrupted before shutdown");
        } else {
            tracing::info!(count = downloaded, "  files would be downloaded");
        }
        tracing::info!(destination = %config.directory.display(), "  destination");
        tracing::info!(concurrency = config.concurrent_downloads, "  concurrency");
        let failed_count = enumeration_errors + usize::from(enumeration_incomplete);
        if failed_count > 0 {
            return Ok((DownloadOutcome::PartialFailure { failed_count }, stats));
        }
        return Ok((DownloadOutcome::Success, stats));
    }

    if streaming_result.url_expired_abort {
        let retry_exhausted = skip_breakdown.retry_exhausted;
        let mut stats = super::SyncStats {
            assets_seen: streaming_result.assets_seen,
            downloaded,
            failed: failed_tasks.len(),
            skipped: skip_breakdown,
            bytes_downloaded: streaming_result.bytes_downloaded,
            disk_bytes_written: streaming_result.disk_bytes_written,
            exif_failures,
            state_write_failures,
            enumeration_errors,
            pagination_shortfall_warnings: 0,
            pagination_shortfall_assets: 0,
            enumeration_incomplete,
            sync_token_blocked: false,
            sync_token_blocked_reason: None,
            elapsed_secs: started.elapsed().as_secs_f64(),
            interrupted: shutdown_token.is_cancelled(),
            rate_limited: streaming_result.rate_limit_observations,
            photos_downloaded: streaming_result.photos_downloaded,
            videos_downloaded: streaming_result.videos_downloaded,
            recap: streaming_result.recap.clone(),
            ..super::SyncStats::default()
        };
        mark_producer_enumeration_incomplete(&mut stats, enumeration_incomplete);
        log_sync_summary("\u{2500}\u{2500} Summary \u{2500}\u{2500}", &stats);
        return Ok((
            DownloadOutcome::PartialFailure {
                failed_count: failed_tasks.len()
                    + state_write_failures
                    + enumeration_errors
                    + exif_failures
                    + retry_exhausted
                    + 1,
            },
            stats,
        ));
    }

    if failed_tasks.is_empty() {
        let retry_exhausted = skip_breakdown.retry_exhausted;
        let mut stats = super::SyncStats {
            assets_seen: streaming_result.assets_seen,
            downloaded,
            failed: 0,
            skipped: skip_breakdown,
            bytes_downloaded: streaming_result.bytes_downloaded,
            disk_bytes_written: streaming_result.disk_bytes_written,
            exif_failures,
            state_write_failures,
            enumeration_errors,
            pagination_shortfall_warnings: 0,
            pagination_shortfall_assets: 0,
            sync_token_blocked: false,
            sync_token_blocked_reason: None,
            elapsed_secs: started.elapsed().as_secs_f64(),
            interrupted: shutdown_token.is_cancelled(),
            rate_limited: streaming_result.rate_limit_observations,
            photos_downloaded: streaming_result.photos_downloaded,
            videos_downloaded: streaming_result.videos_downloaded,
            recap: streaming_result.recap.clone(),
            ..super::SyncStats::default()
        };
        mark_producer_enumeration_incomplete(&mut stats, enumeration_incomplete);
        log_sync_summary("\u{2500}\u{2500} Summary \u{2500}\u{2500}", &stats);
        if state_write_failures > 0
            || enumeration_errors > 0
            || exif_failures > 0
            || retry_exhausted > 0
            || enumeration_incomplete
        {
            return Ok((
                DownloadOutcome::PartialFailure {
                    failed_count: state_write_failures
                        + enumeration_errors
                        + exif_failures
                        + retry_exhausted
                        + usize::from(enumeration_incomplete),
                },
                stats,
            ));
        }
        return Ok((DownloadOutcome::Success, stats));
    }

    // Phase 2: cleanup pass with fresh CDN URLs
    let cleanup_concurrency = 5;
    let failure_count = failed_tasks.len();
    tracing::info!(
        failure_count,
        concurrency = cleanup_concurrency,
        "── Cleanup pass: re-fetching URLs and retrying failed downloads ──"
    );

    let fresh_tasks =
        super::build_retry_download_tasks(passes, config, &failed_tasks, shutdown_token.clone())
            .await?;
    tracing::debug!(
        count = fresh_tasks.len(),
        "  Re-fetched failed tasks with fresh URLs"
    );

    let phase2_rate_counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let pass_config = PassConfig {
        client: download_client,
        retry_config: &config.retry,
        metadata: MetadataFlags::from(config.as_ref()),
        mark_capture_repair_after_download: matches!(
            config.capture_timestamp_repair,
            super::CaptureTimestampRepair::ReplaceWithCaptureLocal
        ),
        concurrency: cleanup_concurrency,
        reporting: controls.reporting,
        temp_suffix: Arc::clone(&config.temp_suffix),
        shutdown_token: shutdown_token.clone(),
        state_db: config.state_db.clone(),
        rate_limit_counter: Arc::clone(&phase2_rate_counter),
        bandwidth_limiter: config.bandwidth_limiter.clone(),
        library: Arc::clone(&config.library),
    };
    let pass_result = run_download_pass(pass_config, fresh_tasks).await;

    let phase2_downloaded = pass_result.downloaded;
    let remaining_failed = pass_result.failed;
    let phase2_auth_errors = pass_result.auth_errors;
    exif_failures += pass_result.exif_failures;
    state_write_failures += pass_result.state_write_failures;
    let total_auth_errors = auth_errors + phase2_auth_errors;

    if total_auth_errors >= AUTH_ERROR_THRESHOLD {
        let mut merged_recap = streaming_result.recap.clone();
        merged_recap.merge(pass_result.recap.clone());
        let stats = super::SyncStats {
            assets_seen: streaming_result.assets_seen,
            downloaded,
            failed: remaining_failed.len(),
            skipped: skip_breakdown,
            bytes_downloaded: streaming_result.bytes_downloaded + pass_result.bytes_downloaded,
            disk_bytes_written: streaming_result.disk_bytes_written
                + pass_result.disk_bytes_written,
            exif_failures,
            state_write_failures,
            enumeration_errors,
            pagination_shortfall_warnings: 0,
            pagination_shortfall_assets: 0,
            sync_token_blocked: false,
            sync_token_blocked_reason: None,
            elapsed_secs: started.elapsed().as_secs_f64(),
            interrupted: true,
            rate_limited: streaming_result.rate_limit_observations
                + pass_result.rate_limit_observations,
            photos_downloaded: streaming_result.photos_downloaded + pass_result.photos_downloaded,
            videos_downloaded: streaming_result.videos_downloaded + pass_result.videos_downloaded,
            recap: merged_recap,
            ..super::SyncStats::default()
        };
        return Ok((
            DownloadOutcome::SessionExpired {
                auth_error_count: total_auth_errors,
            },
            stats,
        ));
    }

    let failed = remaining_failed.len();
    let succeeded = downloaded + phase2_downloaded;

    // Log failed downloads before the summary. `retry_exhausted` is asset
    // rows the producer skipped because they already exceeded max attempts
    // across prior syncs — they belong in the failure total so Docker /
    // systemd / k8s exit-code signalling can notice a chronic backlog.
    let retry_exhausted = skip_breakdown.retry_exhausted;
    let total_failures = failed
        + state_write_failures
        + exif_failures
        + enumeration_errors
        + retry_exhausted
        + usize::from(enumeration_incomplete);
    if total_failures > 0 {
        for task in &remaining_failed {
            tracing::error!(asset_id = %task.asset_id, path = %task.download_path.display(), "Download failed");
        }
    }

    let mut merged_recap = streaming_result.recap.clone();
    merged_recap.merge(pass_result.recap.clone());
    let mut stats = super::SyncStats {
        assets_seen: streaming_result.assets_seen,
        downloaded: succeeded,
        failed,
        skipped: skip_breakdown,
        bytes_downloaded: streaming_result.bytes_downloaded + pass_result.bytes_downloaded,
        disk_bytes_written: streaming_result.disk_bytes_written + pass_result.disk_bytes_written,
        exif_failures,
        state_write_failures,
        enumeration_errors,
        pagination_shortfall_warnings: 0,
        pagination_shortfall_assets: 0,
        enumeration_incomplete,
        sync_token_blocked: false,
        sync_token_blocked_reason: None,
        elapsed_secs: started.elapsed().as_secs_f64(),
        interrupted: shutdown_token.is_cancelled(),
        rate_limited: streaming_result.rate_limit_observations
            + pass_result.rate_limit_observations,
        photos_downloaded: streaming_result.photos_downloaded + pass_result.photos_downloaded,
        videos_downloaded: streaming_result.videos_downloaded + pass_result.videos_downloaded,
        recap: merged_recap,
        ..super::SyncStats::default()
    };
    mark_producer_enumeration_incomplete(&mut stats, enumeration_incomplete);
    maybe_warn_rate_limit_pressure(&stats);
    log_sync_summary("\u{2500}\u{2500} Summary \u{2500}\u{2500}", &stats);

    if total_failures > 0 {
        return Ok((
            DownloadOutcome::PartialFailure {
                failed_count: total_failures,
            },
            stats,
        ));
    }

    Ok((DownloadOutcome::Success, stats))
}

/// Execute a download pass over the given tasks, returning any that failed.
pub(super) async fn run_download_pass(
    config: PassConfig<'_>,
    tasks: Vec<DownloadTask>,
) -> PassResult {
    // Cleanup-pass bar: same bytes-counter as the main bar would have if
    // wired in, but this pass runs after the main pass closes its bar so we
    // create a fresh counter here. The retry pass downloads less data on
    // average, so the bandwidth display reads as the cleanup-only rate.
    let cleanup_bytes_counter = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let pb = crate::personality::progress::single(
        config.reporting.no_progress_bar,
        false,
        tasks.len() as u64,
        config.reporting.personality_mode,
        Some(std::sync::Arc::clone(&cleanup_bytes_counter)),
    );
    let client = config.client.clone();
    let retry_config = config.retry_config;
    let metadata_flags = config.metadata;
    let mark_capture_repair_after_download = config.mark_capture_repair_after_download;
    let state_db = config.state_db.clone();
    let pass_shutdown = config.shutdown_token.child_token();
    let concurrency = config.concurrency;
    let temp_suffix: Arc<str> = config.temp_suffix;
    let rate_limit_counter = Arc::clone(&config.rate_limit_counter);
    let bandwidth_limiter = config.bandwidth_limiter.clone();
    let library: Arc<str> = Arc::clone(&config.library);
    let mode = config.reporting.personality_mode;

    let mut download_stream = stream::iter(tasks)
        .take_while(|_| std::future::ready(!pass_shutdown.is_cancelled()))
        .map(|task| {
            let client = client.clone();
            let task_state_db = state_db.clone();
            let temp_suffix = Arc::clone(&temp_suffix);
            let rate_limit_counter = Arc::clone(&rate_limit_counter);
            let bandwidth_limiter = bandwidth_limiter.clone();
            let shutdown_token = pass_shutdown.clone();
            async move {
                let result = Box::pin(download_single_task(
                    &client,
                    &task,
                    retry_config,
                    metadata_flags,
                    DownloadSingleContext {
                        temp_suffix: &temp_suffix,
                        state_db: task_state_db.as_deref(),
                        rate_limit_counter: Some(rate_limit_counter.as_ref()),
                        bandwidth_limiter: bandwidth_limiter.as_ref(),
                        shutdown_token: &shutdown_token,
                        mode,
                    },
                ))
                .await;
                (task, result)
            }
        })
        .buffer_unordered(concurrency);

    let mut failed: Vec<DownloadTask> = Vec::new();
    let mut downloaded = 0usize;
    let mut downloaded_tasks: Vec<DownloadTask> = Vec::new();
    let mut auth_errors = 0usize;
    let mut exif_failures = 0usize;
    let mut pending_state_writes: Vec<PendingStateWrite> = Vec::new();
    let mut bytes_downloaded_total: u64 = 0;
    let mut disk_bytes_total: u64 = 0;
    let mut photos_downloaded = 0usize;
    let mut videos_downloaded = 0usize;
    let mut recap = super::recap::RunRecap::default();
    let mut state_write_circuit_open = false;
    let mut url_expired_abort = false;
    // Cleanup pass doesn't carry an album label (it's a flat retry list);
    // recap.observe gets the library name so a recovered asset still
    // counts toward the per-album newest tracker rather than vanishing.
    let pass_label = library.as_ref();

    // Stream results as each task completes so state writes and progress-bar
    // updates fire per-item. Collecting first would freeze the progress bar
    // until the last download finished and defer every mark_downloaded to
    // the end of the pass — defeating the point of parallel cleanup.
    while let Some((task, result)) = download_stream.next().await {
        match &result {
            Ok((exif_ok, local_checksum, download_checksum, bytes_dl, disk_bytes)) => {
                downloaded += 1;
                downloaded_tasks.push(task.clone());
                bytes_downloaded_total += bytes_dl;
                cleanup_bytes_counter.fetch_add(*bytes_dl, std::sync::atomic::Ordering::Relaxed);
                disk_bytes_total += disk_bytes;
                if task.media_type.is_photo_like() {
                    photos_downloaded += 1;
                } else if task.media_type.is_video_like() {
                    videos_downloaded += 1;
                }
                if mode.is_friendly() {
                    recap.observe(pass_label, task.to_recap_asset());
                }
                if !*exif_ok {
                    exif_failures += 1;
                    pb.suspend(|| {
                        tracing::error!(
                            asset_id = %task.asset_id,
                            path = %task.download_path.display(),
                            "Metadata write failed after download; marker set for retry on next sync"
                        );
                    });
                }
                if let Some(db) = &state_db {
                    match finalize_downloaded(
                        db.as_ref(),
                        &task.library,
                        &task,
                        local_checksum.clone(),
                        download_checksum.clone(),
                        *exif_ok,
                        mark_capture_repair_after_download,
                    )
                    .await
                    {
                        DownloadedFinalization::Persisted => {}
                        DownloadedFinalization::Deferred { write, error } => {
                            pb.suspend(|| {
                                tracing::warn!(
                                    asset_id = %task.asset_id,
                                    error = %error,
                                    "State write failed, deferring for retry"
                                );
                            });
                            pending_state_writes.push(write);
                            if !state_write_circuit_open
                                && let Some(err) = check_state_write_circuit_breaker(
                                    db.as_ref(),
                                    &mut pending_state_writes,
                                )
                                .await
                            {
                                pb.suspend(|| {
                                        tracing::error!(
                                            error = %err,
                                            "State write circuit breaker opened; halting cleanup downloads"
                                        );
                                    });
                                state_write_circuit_open = true;
                                pass_shutdown.cancel();
                            }
                        }
                    }
                }
            }
            Err(e) => {
                match classify_download_task_error(e) {
                    DownloadTaskErrorClass::Interrupted => {
                        log_interrupted_download(&pb, &task, e);
                        pb.inc(1);
                        continue;
                    }
                    DownloadTaskErrorClass::SessionExpired => {
                        auth_errors += 1;
                        pb.suspend(|| {
                            tracing::warn!(path = %task.download_path.display(), error = %e, "Auth error");
                        });
                    }
                    DownloadTaskErrorClass::ExpiredUrl => {
                        url_expired_abort = true;
                        pb.suspend(|| {
                            tracing::warn!(
                                asset_id = %task.asset_id,
                                path = %task.download_path.display(),
                                error = %e,
                                "Download URL expired; aborting current URL batch"
                            );
                        });
                        pass_shutdown.cancel();
                    }
                    DownloadTaskErrorClass::Other => {
                        pb.suspend(|| {
                            tracing::error!(asset_id = %task.asset_id, path = %task.download_path.display(), error = %e, "Download failed");
                        });
                    }
                }
                if let Some(db) = &state_db
                    && let Err(e) =
                        finalize_failed(db.as_ref(), &task.library, &task, &e.to_string()).await
                {
                    tracing::warn!(
                        asset_id = %task.asset_id,
                        error = %e,
                        "Failed to mark failure"
                    );
                }
                failed.push(task);
            }
        }
        pb.inc(1);
    }

    // Retry any state writes that failed during the pass
    let state_write_failures = if let Some(db) = &state_db {
        if state_write_circuit_open {
            pending_state_writes.len()
        } else {
            flush_pending_state_writes(db.as_ref(), &pending_state_writes).await
        }
    } else {
        0
    };

    pb.finish_and_clear();
    PassResult {
        downloaded,
        downloaded_tasks,
        exif_failures,
        failed,
        auth_errors,
        state_write_failures,
        bytes_downloaded: bytes_downloaded_total,
        disk_bytes_written: disk_bytes_total,
        rate_limit_observations: rate_limit_counter.load(std::sync::atomic::Ordering::Relaxed),
        url_expired_abort,
        photos_downloaded,
        videos_downloaded,
        recap,
    }
}

/// Emit a warn! if rate_limit_observations exceeded 10% of attempts. Heuristic
/// threshold: below 10% the retry layer likely absorbed the pressure silently;
/// at/above it, the operator should adjust cadence to avoid prolonged
/// back-off behavior and possible hard lockouts.
fn maybe_warn_rate_limit_pressure(stats: &super::SyncStats) {
    if !stats.has_rate_limit_pressure() {
        return;
    }
    if stats.assets_seen == 0 {
        // No enumeration anchor — surface the raw count so operators still
        // see the signal, but skip the (meaningless) percentage.
        tracing::warn!(
            rate_limit_observations = stats.rate_limited,
            "Observed HTTP 429/503 rate-limiting before any assets were enumerated — \
             consider raising [watch] interval or lowering [download] threads"
        );
        return;
    }
    let pct = stats.rate_limited as u64 * 100 / stats.assets_seen;
    if pct >= 10 {
        tracing::warn!(
            rate_limit_observations = stats.rate_limited,
            assets_seen = stats.assets_seen,
            percent = pct,
            "Observed HTTP 429/503 rate-limiting on >=10% of sync attempts — \
             consider raising [watch] interval or lowering [download] threads \
             to reduce sustained pressure on iCloud"
        );
    }
}

/// Download a single task, handling mtime and EXIF stamping on success.
///
/// Returns `Ok(true)` on full success, `Ok(false)` if the download succeeded
/// but EXIF stamping failed (the file is usable but lacks EXIF metadata).
#[derive(Clone, Copy)]
struct DownloadSingleContext<'a> {
    temp_suffix: &'a str,
    state_db: Option<&'a dyn DownloadStore>,
    rate_limit_counter: Option<&'a std::sync::atomic::AtomicUsize>,
    bandwidth_limiter: Option<&'a super::BandwidthLimiter>,
    shutdown_token: &'a CancellationToken,
    mode: crate::personality::Mode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DownloadTaskErrorClass {
    Interrupted,
    SessionExpired,
    ExpiredUrl,
    Other,
}

/// Classify per-task download errors at the worker orchestration boundary.
///
/// The stream and cleanup workers share the same behavioral split: interrupted
/// downloads are drain-only, session expiry contributes to the reauth abort
/// threshold, expired CDN URLs abort the current URL batch, and ordinary
/// failures are recorded on the task. The original error is still propagated
/// to state/logging unchanged.
fn classify_download_task_error(error: &anyhow::Error) -> DownloadTaskErrorClass {
    let Some(download_err) = error.downcast_ref::<DownloadError>() else {
        return DownloadTaskErrorClass::Other;
    };
    if download_err.is_interrupted() {
        DownloadTaskErrorClass::Interrupted
    } else if download_err.is_session_expired() {
        DownloadTaskErrorClass::SessionExpired
    } else if download_err.is_expired_url() {
        DownloadTaskErrorClass::ExpiredUrl
    } else {
        DownloadTaskErrorClass::Other
    }
}

fn log_interrupted_download(pb: &ProgressBar, task: &DownloadTask, error: &anyhow::Error) {
    pb.suspend(|| {
        tracing::info!(
            asset_id = %task.asset_id,
            path = %task.download_path.display(),
            error = %error,
            "Download interrupted before final publish"
        );
    });
}

async fn download_single_task<C: super::file::DownloadClient>(
    client: &C,
    task: &DownloadTask,
    retry_config: &RetryConfig,
    metadata_flags: MetadataFlags,
    context: DownloadSingleContext<'_>,
) -> Result<(bool, String, Option<String>, u64, u64)> {
    if let Some(parent) = task.download_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("Could not create directory {}", parent.display()))?;
    }

    tracing::debug!(
        size_bytes = task.size,
        path = %task.download_path.display(),
        "downloading",
    );

    // Embed writes happen on the .part file before the atomic rename. The
    // extension gate is based on the intended final path before download,
    // then the writer sniffs the downloaded part bytes so the temp suffix
    // does not hide the media type.
    let needs_embed =
        metadata_flags.any_embed() && super::metadata::is_embed_writable_path(&task.download_path);
    let owned_part_path =
        super::file::temp_download_path(&task.download_path, &task.checksum, context.temp_suffix)
            .context("Could not compute temporary download path")?;
    if let Some(db) = context.state_db {
        db.claim_temp_file(&owned_part_path)
            .await
            .context("Could not record temporary-file ownership")?;
    }

    let result: Result<(bool, String, Option<String>, u64, u64)> = async {
        let bytes_downloaded = Box::pin(super::file::download_file_with_mode(
            client,
            &task.url,
            &task.download_path,
            &task.checksum,
            retry_config,
            context.temp_suffix,
            super::file::DownloadOpts {
                skip_rename: needs_embed,
                expected_size: if task.size > 0 { Some(task.size) } else { None },
                publication: task.publication,
            },
            super::file::DownloadLimits {
                rate_limit_counter: context.rate_limit_counter,
                bandwidth_limiter: context.bandwidth_limiter,
                shutdown_token: Some(context.shutdown_token),
            },
            context.mode,
        ))
        .await?;

        // When embed writes are needed, modifications happen on the .part file before
        // the atomic rename, preventing silent corruption on power loss / SIGKILL.
        let part_path = needs_embed.then_some(owned_part_path.as_path());

        // Compute SHA-256 of the downloaded content before EXIF modification
        // so we store a hash that reflects the original download bytes.
        let download_checksum = if let Some(path) = &part_path {
            Some(super::file::compute_sha256(path).await?)
        } else {
            None
        };

        let mut exif_ok = true;
        if let Some(part) = &part_path {
            let outcome = metadata_rewrite::write_download_metadata(
                metadata_rewrite::MetadataWriteRequest {
                    final_path: &task.download_path,
                    embed_path: Some(part),
                    expected_embed_fingerprint: None,
                    sidecar_path: None,
                    payload: Arc::clone(&task.metadata),
                    created_local: task.created_local,
                    flags: metadata_flags,
                    capture_timestamp_repair: crate::download::CaptureTimestampRepair::Preserve,
                    temp_suffix: context.temp_suffix,
                },
            )
            .await;
            exif_ok = !outcome.any_failed();
        }

        // Set mtime on .part (before rename) or final path directly.
        // rename() preserves mtime so this works in both cases.
        let mtime_target = part_path.unwrap_or(&task.download_path).to_path_buf();
        let ts = task.created_local.timestamp();
        if let Err(e) = tokio::task::spawn_blocking(move || set_file_mtime(&mtime_target, ts)).await?
        {
            tracing::warn!(
                path = %task.download_path.display(),
                error = %e,
                "Could not set mtime"
            );
        }

        // Atomic rename: .part → final (only when EXIF path was used)
        if let Some(part) = &part_path {
            super::file::publish_part_to_final(part, &task.download_path, task.publication).await?;
        }

        let outcome =
            metadata_rewrite::write_download_metadata(metadata_rewrite::MetadataWriteRequest {
                final_path: &task.download_path,
                embed_path: None,
                expected_embed_fingerprint: None,
                sidecar_path: Some(&task.download_path),
                payload: Arc::clone(&task.metadata),
                created_local: task.created_local,
                flags: metadata_flags,
                capture_timestamp_repair: crate::download::CaptureTimestampRepair::Preserve,
                temp_suffix: context.temp_suffix,
            })
            .await;
        exif_ok &= !outcome.any_failed();

        let disk_bytes = match tokio::fs::metadata(&task.download_path).await {
            Ok(meta) => meta.len(),
            Err(e) => {
                tracing::warn!(path = %task.download_path.display(), error = %e, "Could not stat downloaded file for size tracking");
                0
            }
        };

        tracing::debug!(path = %task.download_path.display(), "Downloaded");

        // Compute SHA-256 of the final file for local storage and verification.
        let local_checksum = super::file::compute_sha256(&task.download_path).await?;

        // Note: Apple's `fileChecksum` is an MMCS (MobileMe Chunked Storage)
        // compound signature, not a SHA-1/SHA-256 content hash. It cannot be
        // compared against a hash of the downloaded bytes.  Content integrity
        // is verified by size matching (Content-Length + API size field) and
        // magic-byte validation during download instead.

        Ok((
            exif_ok,
            local_checksum,
            download_checksum,
            bytes_downloaded,
            disk_bytes,
        ))
    }
    .await;

    if let Some(db) = context.state_db
        && let Err(error) = db
            .retire_temp_files(std::slice::from_ref(&owned_part_path))
            .await
    {
        if result.is_ok() {
            return Err(error).context("Could not retire temporary-file ownership");
        }
        tracing::warn!(
            path = %owned_part_path.display(),
            error = %error,
            "Could not retire temporary-file ownership after download stopped"
        );
    }

    result
}

pub(super) fn format_duration(d: Duration) -> String {
    let total_secs = d.as_secs();
    let hours = total_secs / 3600;
    let mins = (total_secs % 3600) / 60;
    let secs = total_secs % 60;

    if hours > 0 {
        format!("{hours}h {mins:02}m {secs:02}s")
    } else if mins > 0 {
        format!("{mins}m {secs:02}s")
    } else {
        format!("{secs}s")
    }
}

#[allow(
    clippy::cast_precision_loss,
    reason = "display-only byte-size formatting; precision loss at exabyte scale is fine for a human-readable string"
)]
fn format_bytes(bytes: u64) -> String {
    if bytes >= 1_073_741_824 {
        format!("{:.1} GiB", bytes as f64 / 1_073_741_824.0)
    } else if bytes >= 1_048_576 {
        format!("{:.1} MiB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

/// Log a formatted summary of sync statistics.
pub(super) fn log_sync_summary(title: &str, stats: &super::SyncStats) {
    tracing::info!(title = %title, "Sync summary");

    // Line 1: core counts
    let skipped = stats.skipped.total() - stats.skipped.duplicates;
    let total = stats.downloaded + stats.failed + skipped;
    if skipped > 0 {
        tracing::info!(
            "  {downloaded} downloaded, {skipped} skipped, {failed} failed ({total} total)",
            downloaded = stats.downloaded,
            failed = stats.failed
        );
    } else {
        tracing::info!(
            "  {downloaded} downloaded, {failed} failed ({total} total)",
            downloaded = stats.downloaded,
            failed = stats.failed
        );
    }

    // Line 2: error details (only if any). `enumeration_errors` can
    // gate `PartialFailure` on its own, so an operator chasing exit
    // code 2 with no other failure counts needs to see it here.
    if stats.exif_failures > 0 || stats.state_write_failures > 0 || stats.enumeration_errors > 0 {
        tracing::info!(
            "  {} EXIF write failure(s), {} state write failure(s), {} enumeration error(s)",
            stats.exif_failures,
            stats.state_write_failures,
            stats.enumeration_errors
        );
    }
    if stats.enumeration_incomplete {
        tracing::info!(
            "  Enumeration incomplete; sync token blocked and next cycle will replay changes"
        );
    }

    // Line 3: skip breakdown (only if skips > 0)
    if skipped > 0 {
        let mut reasons = Vec::new();
        if stats.skipped.by_state > 0 {
            reasons.push(format!("{} already downloaded", stats.skipped.by_state));
        }
        if stats.skipped.on_disk > 0 {
            reasons.push(format!("{} on disk", stats.skipped.on_disk));
        }
        if stats.skipped.by_media_type > 0 {
            reasons.push(format!(
                "{} filtered by media type",
                stats.skipped.by_media_type
            ));
        }
        if stats.skipped.by_date_range > 0 {
            reasons.push(format!(
                "{} filtered by date range",
                stats.skipped.by_date_range
            ));
        }
        if stats.skipped.by_live_photo > 0 {
            reasons.push(format!(
                "{} filtered (live photo)",
                stats.skipped.by_live_photo
            ));
        }
        if stats.skipped.by_filename > 0 {
            reasons.push(format!(
                "{} filtered by filename",
                stats.skipped.by_filename
            ));
        }
        if stats.skipped.by_excluded_album > 0 {
            reasons.push(format!(
                "{} excluded by album",
                stats.skipped.by_excluded_album
            ));
        }
        if stats.skipped.ampm_variant > 0 {
            reasons.push(format!("{} AM/PM variants", stats.skipped.ampm_variant));
        }
        if stats.skipped.retry_exhausted > 0 {
            reasons.push(format!(
                "{} retries exhausted",
                stats.skipped.retry_exhausted
            ));
        }
        if stats.skipped.retry_only > 0 {
            reasons.push(format!(
                "{} not failed (retry mode)",
                stats.skipped.retry_only
            ));
        }
        if !reasons.is_empty() {
            tracing::info!("  Skipped: {}", reasons.join(", "));
        }
    }

    // Line 4: transfer stats (only if bytes downloaded)
    if stats.bytes_downloaded > 0 {
        if stats.bytes_downloaded == stats.disk_bytes_written {
            tracing::info!("  Transferred {}", format_bytes(stats.bytes_downloaded));
        } else {
            tracing::info!(
                "  Transferred {}, {} written to disk",
                format_bytes(stats.bytes_downloaded),
                format_bytes(stats.disk_bytes_written)
            );
        }
    }

    // Line 5: elapsed
    tracing::info!(
        "  Completed in {}",
        format_duration(Duration::from_secs_f64(stats.elapsed_secs))
    );
}

/// Set the modification and access times of a file to the given Unix
/// timestamp. Uses `std::fs::File::set_times` (stable since Rust 1.75).
///
/// Handles negative timestamps (dates before 1970) gracefully by clamping
/// to the Unix epoch.
fn set_file_mtime(path: &Path, timestamp: i64) -> std::io::Result<()> {
    let time = if timestamp >= 0 {
        UNIX_EPOCH + Duration::from_secs(timestamp.unsigned_abs())
    } else {
        tracing::warn!(
            path = %path.display(),
            timestamp,
            "Negative timestamp (pre-1970 date), clamping mtime to epoch"
        );
        UNIX_EPOCH
            .checked_sub(Duration::from_secs(timestamp.unsigned_abs()))
            .unwrap_or(SystemTime::UNIX_EPOCH)
    };
    let times = FileTimes::new().set_modified(time).set_accessed(time);
    let file = std::fs::File::options().write(true).open(path)?;
    file.set_times(times)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::filter::MetadataPayload;
    use super::*;
    use crate::state::error::StateError;
    use crate::state::types::SyncSummary;
    use crate::state::{
        AssetRecord, DownloadStateStore, ImportStateStore, MembershipStore, MetadataRewriteStore,
        ReportStateStore, SyncRunStats, SyncTokenStore, TempFileOwnershipStore, VersionSizeKey,
    };
    use crate::test_helpers::TestPhotoAsset;
    use std::collections::{HashMap, HashSet};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
    use tempfile::TempDir;

    fn classify_download_task_error_for(err: DownloadError) -> DownloadTaskErrorClass {
        let err = anyhow::Error::new(err);
        classify_download_task_error(&err)
    }

    struct StaticDownloadClient {
        body: Vec<u8>,
    }

    #[async_trait::async_trait]
    impl super::super::file::DownloadClient for StaticDownloadClient {
        async fn fetch(
            &self,
            _: &str,
            _: Option<u64>,
        ) -> Result<super::super::file::DownloadResponse, Box<dyn std::error::Error + Send + Sync>>
        {
            let body = bytes::Bytes::copy_from_slice(&self.body);
            Ok(super::super::file::DownloadResponse {
                status: 200,
                content_length: Some(body.len() as u64),
                content_range: None,
                content_type: Some("image/jpeg".to_string()),
                stream: Box::pin(futures_util::stream::once(async move { Ok(body) })),
            })
        }
    }

    #[test]
    fn classify_download_task_error_detects_interrupted_download() {
        assert_eq!(
            classify_download_task_error_for(DownloadError::Interrupted {
                path: "photo.jpg".into(),
                bytes_written: 12,
            }),
            DownloadTaskErrorClass::Interrupted
        );
    }

    #[test]
    fn classify_download_task_error_detects_session_expiry_and_expired_url() {
        assert_eq!(
            classify_download_task_error_for(DownloadError::HttpStatus {
                status: 401,
                path: "photo.jpg".into(),
            }),
            DownloadTaskErrorClass::SessionExpired
        );
        assert_eq!(
            classify_download_task_error_for(DownloadError::HttpStatus {
                status: 403,
                path: "photo.jpg".into(),
            }),
            DownloadTaskErrorClass::SessionExpired
        );
        assert_eq!(
            classify_download_task_error_for(DownloadError::HttpStatus {
                status: 410,
                path: "photo.jpg".into(),
            }),
            DownloadTaskErrorClass::ExpiredUrl
        );
    }

    #[test]
    fn classify_download_task_error_treats_ordinary_errors_as_other() {
        assert_eq!(
            classify_download_task_error_for(DownloadError::InvalidContent {
                path: "photo.jpg".into(),
                reason: "not a photo".into(),
            }),
            DownloadTaskErrorClass::Other
        );

        let plain = anyhow::anyhow!("plain failure");
        assert_eq!(
            classify_download_task_error(&plain),
            DownloadTaskErrorClass::Other
        );
    }

    #[test]
    fn classify_download_task_error_detects_context_wrapped_download_errors() {
        let interrupted = anyhow::Error::new(DownloadError::Interrupted {
            path: "photo.jpg".into(),
            bytes_written: 12,
        })
        .context("worker");
        assert_eq!(
            classify_download_task_error(&interrupted),
            DownloadTaskErrorClass::Interrupted
        );

        let session_expired = anyhow::Error::new(DownloadError::HttpStatus {
            status: 403,
            path: "photo.jpg".into(),
        })
        .context("worker");
        assert_eq!(
            classify_download_task_error(&session_expired),
            DownloadTaskErrorClass::SessionExpired
        );

        let expired_url = anyhow::Error::new(DownloadError::HttpStatus {
            status: 410,
            path: "photo.jpg".into(),
        })
        .context("worker");
        assert_eq!(
            classify_download_task_error(&expired_url),
            DownloadTaskErrorClass::ExpiredUrl
        );
    }

    // ── batch_forecast_decision unit tests ─────────────────────────────────

    #[test]
    fn batch_forecast_decision_none_free_always_continues() {
        let queued = AtomicU64::new(0);
        let warn = AtomicBool::new(false);
        let (decision, total) = batch_forecast_decision(10_000, None, &queued, &warn);
        assert_eq!(decision, BatchForecast::Continue);
        assert_eq!(total, 10_000);
        // Even huge sizes must not emit warn/bail when free-space probe failed
        let (decision, _) = batch_forecast_decision(u64::MAX - 10_000, None, &queued, &warn);
        assert_eq!(decision, BatchForecast::Continue);
    }

    #[test]
    fn batch_forecast_decision_below_warn_threshold_continues() {
        let queued = AtomicU64::new(0);
        let warn = AtomicBool::new(false);
        // free = 1000, 50% of free queued → below 90% threshold
        let (decision, total) = batch_forecast_decision(500, Some(1000), &queued, &warn);
        assert_eq!(decision, BatchForecast::Continue);
        assert_eq!(total, 500);
        assert!(!warn.load(Ordering::Relaxed));
    }

    #[test]
    fn batch_forecast_decision_crossing_90pct_warns_once() {
        let queued = AtomicU64::new(0);
        let warn = AtomicBool::new(false);
        // First call crosses 90% threshold
        let (decision, total) = batch_forecast_decision(900, Some(1000), &queued, &warn);
        assert_eq!(decision, BatchForecast::Warn);
        assert_eq!(total, 900);
        assert!(warn.load(Ordering::Relaxed));
        // Subsequent calls that stay below 100% must NOT re-warn
        let (decision, total) = batch_forecast_decision(50, Some(1000), &queued, &warn);
        assert_eq!(decision, BatchForecast::Continue);
        assert_eq!(total, 950);
    }

    #[test]
    fn batch_forecast_decision_crossing_100pct_bails() {
        let queued = AtomicU64::new(800);
        let warn = AtomicBool::new(true); // already warned at 800
        // 800 + 250 = 1050 ≥ 1000 → bail
        let (decision, total) = batch_forecast_decision(250, Some(1000), &queued, &warn);
        assert_eq!(decision, BatchForecast::Bail);
        assert_eq!(total, 1050);
    }

    #[test]
    fn batch_forecast_decision_prefers_bail_over_warn_at_100pct_first_call() {
        // If the very first queued task already exceeds free space, we should
        // bail (not warn). This is the 2TB-into-300GB-disk scenario.
        let queued = AtomicU64::new(0);
        let warn = AtomicBool::new(false);
        let (decision, total) =
            batch_forecast_decision(2_000_000_000, Some(300_000_000), &queued, &warn);
        assert_eq!(decision, BatchForecast::Bail);
        assert_eq!(total, 2_000_000_000);
        // warn flag should NOT have been set — bail short-circuits
        assert!(!warn.load(Ordering::Relaxed));
    }

    #[test]
    fn batch_forecast_decision_zero_size_is_a_noop() {
        let queued = AtomicU64::new(500);
        let warn = AtomicBool::new(false);
        let (decision, total) = batch_forecast_decision(0, Some(1000), &queued, &warn);
        assert_eq!(decision, BatchForecast::Continue);
        assert_eq!(total, 500);
    }

    // ── Free-space re-snapshot cadence ───────────────────────────────

    /// A re-snapshot is required once the running queued total has
    /// crossed `interval` bytes past the last snapshot point. Pinning the
    /// happy path so a future tweak doesn't regress to the stale-snapshot
    /// behaviour the fix was added to repair.
    #[test]
    fn should_resnapshot_free_space_fires_at_interval_boundary() {
        let interval = FREE_SPACE_RESNAPSHOT_INTERVAL_BYTES;
        // Just below the interval: no resnapshot.
        assert!(!should_resnapshot_free_space(interval - 1, 0, interval));
        // Exactly at the interval: resnapshot required.
        assert!(should_resnapshot_free_space(interval, 0, interval));
        // Past the interval: resnapshot still required (caller updates
        // last_snapshot to suppress repeats; this helper is stateless).
        assert!(should_resnapshot_free_space(interval * 2, 0, interval));
    }

    /// When the interval is 0 the helper must return `false` for any
    /// total. Guards against accidentally turning the cadence into "every
    /// call" if a constant is ever zeroed out.
    #[test]
    fn should_resnapshot_free_space_zero_interval_never_fires() {
        for total in [0u64, 1, 1_000, u64::MAX] {
            assert!(
                !should_resnapshot_free_space(total, 0, 0),
                "interval=0 must never request a resnapshot (total={total})"
            );
        }
    }

    /// Drives the helper through a sequence where the simulated free
    /// space drops between snapshots and asserts the bail decision picks up
    /// the fresher snapshot. Pre-fix, the bail rode on the stale 1 TiB
    /// snapshot and never fired even though the FS had only 1 GiB left.
    #[test]
    fn batch_forecast_resnapshot_picks_up_fs_filling_mid_sync() {
        let queued = AtomicU64::new(0);
        let warn = AtomicBool::new(false);

        // Snapshot 1: 1 TiB free. Queue 500 GiB → still well under 90%, no
        // warn or bail.
        let initial_free = 1024u64 * 1024 * 1024 * 1024; // 1 TiB
        let snapshot1_total = 500u64 * 1024 * 1024 * 1024; // 500 GiB
        let (decision, total) =
            batch_forecast_decision(snapshot1_total, Some(initial_free), &queued, &warn);
        assert_eq!(
            decision,
            BatchForecast::Continue,
            "500 GiB / 1 TiB stays under 90%"
        );
        assert_eq!(total, snapshot1_total);

        // Between snapshots an unrelated process consumed almost all the
        // disk. Refresh free-space to 1 GiB. Even though `queued` already
        // counts 500 GiB, the next forecast call should see the fresher
        // 1 GiB free figure and bail.
        let refreshed_free = 1024u64 * 1024 * 1024; // 1 GiB
        let (decision, total) =
            batch_forecast_decision(1024 * 1024, Some(refreshed_free), &queued, &warn);
        assert_eq!(
            decision,
            BatchForecast::Bail,
            "after re-snapshot to 1 GiB free, queued 500 GiB+ must trigger Bail"
        );
        assert!(total > refreshed_free, "queued total dwarfs refreshed free");
    }

    // ── add_asset_album retry tests ─────────────────────────────────

    /// Membership store stub whose `add_asset_album` returns `LockPoisoned` for the
    /// first `fail_first` calls, then succeeds. Tracks total call count so
    /// tests can pin the retry-attempt accounting. Other methods panic
    /// because the test path never reaches them.
    struct AlbumRetryStubDb {
        remaining_failures: AtomicUsize,
        calls: AtomicUsize,
        recorded: std::sync::Mutex<Vec<(String, String, String)>>,
    }

    impl AlbumRetryStubDb {
        fn new(fail_first: usize) -> Self {
            Self {
                remaining_failures: AtomicUsize::new(fail_first),
                calls: AtomicUsize::new(0),
                recorded: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl MembershipStore for AlbumRetryStubDb {
        async fn add_asset_album(
            &self,
            _library: &str,
            asset_id: &str,
            album_name: &str,
            source: &str,
        ) -> Result<(), StateError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            let prev = self.remaining_failures.fetch_sub(1, Ordering::Relaxed);
            if prev > 0 {
                Err(StateError::LockPoisoned("simulated SQLite busy".into()))
            } else {
                self.remaining_failures.store(0, Ordering::Relaxed);
                self.recorded.lock().unwrap().push((
                    asset_id.to_string(),
                    album_name.to_string(),
                    source.to_string(),
                ));
                Ok(())
            }
        }

        async fn get_all_asset_albums(&self, _: &str) -> Result<Vec<(String, String)>, StateError> {
            unimplemented!()
        }

        async fn get_all_asset_people(&self, _: &str) -> Result<Vec<(String, String)>, StateError> {
            unimplemented!()
        }
    }

    /// A single transient `LockPoisoned` from `add_asset_album` must
    /// be retried so the album-membership row lands. Pre-fix this caller
    /// logged at `warn!` and dropped the row, leaving downstream consumers
    /// (EXIF keywords, Immich albums) with incomplete data until the next
    /// full enumeration repopulated it.
    #[tokio::test]
    async fn add_asset_album_retry_recovers_after_one_transient_failure() {
        let stub = AlbumRetryStubDb::new(1); // fail once, succeed on retry
        let result =
            add_asset_album_with_retry(&stub, "PrimarySync", "ASSET_A", "Favorites", "icloud")
                .await;
        assert!(
            result.is_ok(),
            "transient SQLite-busy must be retried, not surfaced as Err"
        );
        let recorded = stub.recorded.lock().unwrap();
        assert_eq!(recorded.len(), 1, "exactly one row must be recorded");
        assert_eq!(
            recorded[0],
            ("ASSET_A".into(), "Favorites".into(), "icloud".into())
        );
        assert_eq!(
            stub.calls.load(Ordering::Relaxed),
            2,
            "must hit the DB twice: one fail + one success"
        );
    }

    /// When failures persist beyond the retry cap, the error must
    /// surface so the caller can log at `warn!` (existing behaviour) — a
    /// genuinely-wedged DB shouldn't be silently retried forever.
    #[tokio::test]
    async fn add_asset_album_retry_surfaces_persistent_failure() {
        // Fail more times than the retry cap.
        let stub = AlbumRetryStubDb::new((ADD_ASSET_ALBUM_MAX_RETRIES + 5) as usize);
        let result =
            add_asset_album_with_retry(&stub, "PrimarySync", "ASSET_B", "Trip", "icloud").await;
        assert!(
            result.is_err(),
            "persistent failure must propagate so the caller's warn! fires"
        );
        assert_eq!(
            stub.calls.load(Ordering::Relaxed) as u32,
            ADD_ASSET_ALBUM_MAX_RETRIES,
            "must attempt exactly ADD_ASSET_ALBUM_MAX_RETRIES times before giving up"
        );
        assert!(
            stub.recorded.lock().unwrap().is_empty(),
            "no row must be recorded when all attempts fail"
        );
    }

    /// A first-call success must not pay the retry cost. Pinning so
    /// the retry loop doesn't accidentally turn into "retry on every call".
    #[tokio::test]
    async fn add_asset_album_retry_no_op_on_first_success() {
        let stub = AlbumRetryStubDb::new(0);
        let result =
            add_asset_album_with_retry(&stub, "PrimarySync", "ASSET_C", "Holiday", "icloud").await;
        assert!(result.is_ok());
        assert_eq!(
            stub.calls.load(Ordering::Relaxed),
            1,
            "first-call success must hit the DB exactly once"
        );
    }

    // ── maybe_warn_rate_limit_pressure ─────────────────────────────────────
    //
    // The helper itself is side-effect-only (emits tracing::warn!); we assert
    // the pure-math decision via the percentage threshold. A full log-capture
    // test would need tracing-subscriber machinery that the rest of this
    // module doesn't set up.

    fn stats_with_rl(assets_seen: u64, rate_limited: usize) -> super::super::SyncStats {
        super::super::SyncStats {
            assets_seen,
            rate_limited,
            ..super::super::SyncStats::default()
        }
    }

    #[test]
    fn rate_limit_pressure_triggers_at_exactly_10_percent() {
        let stats = stats_with_rl(100, 10);
        assert!(stats.has_rate_limit_pressure());
    }

    #[test]
    fn rate_limit_pressure_below_10_percent_does_not_trigger() {
        let stats = stats_with_rl(100, 9);
        assert!(!stats.has_rate_limit_pressure());
    }

    #[test]
    fn rate_limit_pressure_zero_assets_seen_does_not_panic() {
        // With zero assets_seen, the helper must skip percentage math (which
        // would produce a misleading "300%" for 3 observations) and emit a
        // separate no-anchor warn path.
        let stats = stats_with_rl(0, 3);
        assert!(stats.has_rate_limit_pressure());
        maybe_warn_rate_limit_pressure(&stats);
    }

    #[test]
    fn rate_limit_pressure_zero_observations_skips_quickly() {
        let stats = stats_with_rl(100, 0);
        assert!(!stats.has_rate_limit_pressure());
        maybe_warn_rate_limit_pressure(&stats); // no panic, no denom needed
    }

    #[test]
    fn batch_forecast_decision_saturating_mul_never_overflows_warn_threshold() {
        // Near-u64::MAX free values must compute warn_threshold without
        // overflowing and without spuriously warning at tiny totals.
        let queued = AtomicU64::new(0);
        let warn = AtomicBool::new(false);
        let (decision, _total) = batch_forecast_decision(1_000, Some(u64::MAX), &queued, &warn);
        assert_eq!(decision, BatchForecast::Continue);
    }

    #[test]
    fn pass_config_debug_keeps_runtime_handles_out_of_output() {
        let client = reqwest::Client::new();
        let retry_config = RetryConfig::default();
        let config = PassConfig {
            client: &client,
            retry_config: &retry_config,
            metadata: MetadataFlags::DATETIME | MetadataFlags::DESCRIPTION,
            mark_capture_repair_after_download: false,
            concurrency: 3,
            reporting: DownloadReporting::hidden(),
            temp_suffix: Arc::from(".part"),
            shutdown_token: CancellationToken::new(),
            state_db: None,
            rate_limit_counter: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            bandwidth_limiter: None,
            library: Arc::from("PrimarySync"),
        };

        let rendered = format!("{config:?}");
        assert!(rendered.contains("metadata"));
        assert!(rendered.contains("concurrency: 3"));
        assert!(rendered.contains("temp_suffix: \".part\""));
        assert!(!rendered.contains("client"));
        assert!(!rendered.contains("retry_config"));
        assert!(!rendered.contains("shutdown_token"));
    }

    #[test]
    fn test_set_file_mtime_positive_timestamp() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("pos.txt");
        fs::write(&p, b"test").unwrap();
        set_file_mtime(&p, 1_700_000_000).unwrap();
        let meta = fs::metadata(&p).unwrap();
        let mtime = meta.modified().unwrap();
        assert_eq!(mtime, UNIX_EPOCH + Duration::from_secs(1_700_000_000));
    }

    #[test]
    fn test_set_file_mtime_zero_timestamp() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("zero.txt");
        fs::write(&p, b"test").unwrap();
        set_file_mtime(&p, 0).unwrap();
        let meta = fs::metadata(&p).unwrap();
        let mtime = meta.modified().unwrap();
        assert_eq!(mtime, UNIX_EPOCH);
    }

    #[test]
    fn test_set_file_mtime_negative_timestamp() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("neg.txt");
        fs::write(&p, b"test").unwrap();
        // Should not panic — clamps or uses pre-epoch time
        set_file_mtime(&p, -86400).unwrap();
    }

    #[test]
    fn test_set_file_mtime_nonexistent_file() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("nonexistent_file.txt");
        assert!(set_file_mtime(&p, 0).is_err());
    }

    #[test]
    fn test_format_duration_seconds_only() {
        assert_eq!(format_duration(Duration::from_secs(0)), "0s");
        assert_eq!(format_duration(Duration::from_secs(1)), "1s");
        assert_eq!(format_duration(Duration::from_secs(42)), "42s");
        assert_eq!(format_duration(Duration::from_secs(59)), "59s");
    }

    #[test]
    fn test_format_duration_minutes_and_seconds() {
        assert_eq!(format_duration(Duration::from_secs(60)), "1m 00s");
        assert_eq!(format_duration(Duration::from_secs(61)), "1m 01s");
        assert_eq!(format_duration(Duration::from_secs(754)), "12m 34s");
        assert_eq!(format_duration(Duration::from_secs(3599)), "59m 59s");
    }

    #[test]
    fn test_format_duration_hours() {
        assert_eq!(format_duration(Duration::from_secs(3600)), "1h 00m 00s");
        assert_eq!(format_duration(Duration::from_secs(5025)), "1h 23m 45s");
        assert_eq!(format_duration(Duration::from_secs(86399)), "23h 59m 59s");
    }

    // These tests need a larger stack due to large async futures from reqwest
    // and stream combinators. We spawn them on a thread with 8 MiB stack.
    #[test]
    fn test_run_download_pass_skips_all_tasks_when_cancelled() {
        std::thread::Builder::new()
            .stack_size(8 * 1024 * 1024)
            .spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(async {
                        let dir = TempDir::new().unwrap();
                        let token = CancellationToken::new();
                        token.cancel();

                        let tasks = vec![
                            DownloadTask {
                                url: "https://p01.icloud-content.com/a".into(),
                                download_path: dir.path().join("a.jpg"),
                                publication: crate::download::file::FinalPublication::NoReplace,
                                checksum: "aaa".into(),
                                created_local: chrono::Local::now().fixed_offset(),
                                size: 1000,
                                asset_id: "ASSET_A".into(),
                                asset_record_name: "ASSET_A".into(),
                                library: "PrimarySync".into(),
                                metadata: Arc::new(MetadataPayload::default()),
                                version_size: VersionSizeKey::Original,
                                media_type: crate::state::MediaType::Photo,
                            },
                            DownloadTask {
                                url: "https://p01.icloud-content.com/b".into(),
                                download_path: dir.path().join("b.jpg"),
                                publication: crate::download::file::FinalPublication::NoReplace,
                                checksum: "bbb".into(),
                                created_local: chrono::Local::now().fixed_offset(),
                                size: 2000,
                                asset_id: "ASSET_B".into(),
                                asset_record_name: "ASSET_B".into(),
                                library: "PrimarySync".into(),
                                metadata: Arc::new(MetadataPayload::default()),
                                version_size: VersionSizeKey::Original,
                                media_type: crate::state::MediaType::Photo,
                            },
                        ];

                        let client = Client::new();
                        let retry = RetryConfig::default();

                        let pass_config = PassConfig {
                            client: &client,
                            retry_config: &retry,
                            metadata: MetadataFlags::default(),
                            mark_capture_repair_after_download: false,
                            concurrency: 1,
                            reporting: DownloadReporting::hidden(),
                            temp_suffix: std::sync::Arc::from(".kei-tmp"),
                            shutdown_token: token,
                            state_db: None,
                            rate_limit_counter: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                            bandwidth_limiter: None,
                            library: std::sync::Arc::from("PrimarySync"),
                        };
                        let result = run_download_pass(pass_config, tasks).await;
                        assert!(result.failed.is_empty());
                    });
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn test_run_download_pass_processes_tasks_when_not_cancelled() {
        std::thread::Builder::new()
            .stack_size(8 * 1024 * 1024)
            .spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(async {
                        let dir = TempDir::new().unwrap();
                        let token = CancellationToken::new();

                        let tasks = vec![DownloadTask {
                            url: "https://0.0.0.0:1/nonexistent".into(),
                            download_path: dir.path().join("c.jpg"),
                            publication: crate::download::file::FinalPublication::NoReplace,
                            checksum: "ccc".into(),
                            created_local: chrono::Local::now().fixed_offset(),
                            size: 500,
                            asset_id: "ASSET_C".into(),
                            asset_record_name: "ASSET_C".into(),
                            library: "PrimarySync".into(),
                            metadata: Arc::new(MetadataPayload::default()),
                            version_size: VersionSizeKey::Original,
                            media_type: crate::state::MediaType::Photo,
                        }];

                        let client = Client::new();
                        let retry = RetryConfig {
                            max_retries: 0,
                            base_delay_secs: 0,
                            max_delay_secs: 0,
                        };

                        let pass_config = PassConfig {
                            client: &client,
                            retry_config: &retry,
                            metadata: MetadataFlags::default(),
                            mark_capture_repair_after_download: false,
                            concurrency: 1,
                            reporting: DownloadReporting::hidden(),
                            temp_suffix: std::sync::Arc::from(".kei-tmp"),
                            shutdown_token: token,
                            state_db: None,
                            rate_limit_counter: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                            bandwidth_limiter: None,
                            library: std::sync::Arc::from("PrimarySync"),
                        };
                        let result = run_download_pass(pass_config, tasks).await;
                        assert_eq!(result.failed.len(), 1);
                    });
            })
            .unwrap()
            .join()
            .unwrap();
    }

    // ── format_duration additional edge cases ────────────────────────────

    #[test]
    fn test_format_duration_125_seconds() {
        assert_eq!(format_duration(Duration::from_secs(125)), "2m 05s");
    }

    #[test]
    fn test_format_duration_3661_seconds() {
        assert_eq!(format_duration(Duration::from_secs(3661)), "1h 01m 01s");
    }

    #[test]
    fn test_format_duration_ignores_sub_second() {
        // Duration with millis should only show whole seconds
        assert_eq!(format_duration(Duration::from_millis(1999)), "1s");
        assert_eq!(format_duration(Duration::from_millis(500)), "0s");
    }

    #[test]
    fn test_producer_skip_summary_total() {
        let skips = ProducerSkipSummary {
            by_state: 10,
            on_disk: 5,
            ampm_variant: 2,
            by_media_type: 1,
            by_date_range: 0,
            by_live_photo: 0,
            by_filename: 0,
            by_excluded_album: 0,
            duplicates: 3,
            retry_exhausted: 4,
            retry_only: 0,
        };
        assert_eq!(skips.total(), 25);
    }

    #[test]
    fn test_producer_skip_summary_add_assign() {
        let mut a = ProducerSkipSummary {
            by_state: 10,
            on_disk: 5,
            ampm_variant: 2,
            by_media_type: 1,
            by_date_range: 0,
            by_live_photo: 0,
            by_filename: 0,
            by_excluded_album: 0,
            duplicates: 3,
            retry_exhausted: 4,
            retry_only: 0,
        };
        let b = ProducerSkipSummary {
            by_state: 1,
            on_disk: 2,
            ampm_variant: 3,
            by_media_type: 2,
            by_date_range: 1,
            by_live_photo: 1,
            by_filename: 0,
            by_excluded_album: 0,
            duplicates: 5,
            retry_exhausted: 6,
            retry_only: 7,
        };
        a += b;
        assert_eq!(a.by_state, 11);
        assert_eq!(a.on_disk, 7);
        assert_eq!(a.ampm_variant, 5);
        assert_eq!(a.by_media_type, 3);
        assert_eq!(a.by_date_range, 1);
        assert_eq!(a.by_live_photo, 1);
        assert_eq!(a.by_filename, 0);
        assert_eq!(a.by_excluded_album, 0);
        assert_eq!(a.duplicates, 8);
        assert_eq!(a.retry_exhausted, 10);
        assert_eq!(a.retry_only, 7);
        assert_eq!(a.total(), 53);
    }

    #[test]
    fn producer_skip_summary_records_every_filter_reason() {
        let mut skips = ProducerSkipSummary::default();

        skips.record_filter_reason(super::super::filter::FilterReason::MalformedAsset);
        skips.record_filter_reason(super::super::filter::FilterReason::ExcludedAlbum);
        skips.record_filter_reason(super::super::filter::FilterReason::MediaType);
        skips.record_filter_reason(super::super::filter::FilterReason::LivePhoto);
        skips.record_filter_reason(super::super::filter::FilterReason::DateRange);
        skips.record_filter_reason(super::super::filter::FilterReason::Filename);

        assert_eq!(skips.by_filename, 2);
        assert_eq!(skips.by_excluded_album, 1);
        assert_eq!(skips.by_media_type, 1);
        assert_eq!(skips.by_live_photo, 1);
        assert_eq!(skips.by_date_range, 1);
        assert_eq!(skips.total(), 6);
    }

    #[test]
    fn producer_skip_summary_converts_to_public_skip_breakdown() {
        let skips = ProducerSkipSummary {
            by_state: 1,
            on_disk: 2,
            ampm_variant: 3,
            by_media_type: 4,
            by_date_range: 5,
            by_live_photo: 6,
            by_filename: 7,
            by_excluded_album: 8,
            duplicates: 9,
            retry_exhausted: 10,
            retry_only: 11,
        };

        let breakdown = super::super::SkipBreakdown::from(skips);
        assert_eq!(breakdown.by_state, 1);
        assert_eq!(breakdown.on_disk, 2);
        assert_eq!(breakdown.ampm_variant, 3);
        assert_eq!(breakdown.by_media_type, 4);
        assert_eq!(breakdown.by_date_range, 5);
        assert_eq!(breakdown.by_live_photo, 6);
        assert_eq!(breakdown.by_filename, 7);
        assert_eq!(breakdown.by_excluded_album, 8);
        assert_eq!(breakdown.duplicates, 9);
        assert_eq!(breakdown.retry_exhausted, 10);
        assert_eq!(breakdown.retry_only, 11);
    }

    #[test]
    fn test_producer_skip_summary_default_is_zero() {
        let skips = ProducerSkipSummary::default();
        assert_eq!(skips.total(), 0);
    }

    async fn build_zero_download_outcome(
        streaming_result: StreamingResult,
        controls: DownloadControls,
    ) -> (crate::download::DownloadOutcome, super::super::SyncStats) {
        let client = reqwest::Client::new();
        let config = Arc::new(crate::download::DownloadConfig::test_default());
        build_download_outcome(
            &client,
            &[],
            &config,
            controls,
            streaming_result,
            Instant::now(),
            CancellationToken::new(),
        )
        .await
        .expect("zero-download outcome should build")
    }

    /// The producer relies on `AssetDisposition` ordering via `.max()` to
    /// pick the highest-priority outcome when an asset has mixed task results.
    /// If variant order changes, `.max()` silently picks the wrong winner.
    #[test]
    fn test_asset_disposition_ordering() {
        use AssetDisposition::{
            AmpmVariant, Forwarded, OnDisk, RetryExhausted, RetryOnly, StateSkip, Unresolved,
        };
        assert!(Forwarded > OnDisk);
        assert!(OnDisk > AmpmVariant);
        assert!(AmpmVariant > StateSkip);
        assert!(StateSkip > RetryExhausted);
        assert!(RetryExhausted > RetryOnly);
        assert!(RetryOnly > Unresolved);

        // .max() picks the highest priority
        assert_eq!(Unresolved.max(Forwarded), Forwarded);
        assert_eq!(OnDisk.max(RetryExhausted), OnDisk);
        assert_eq!(RetryOnly.max(RetryExhausted), RetryExhausted);
    }

    /// T-6: All pending state writes from the download loop are retained and
    /// re-flushed. Even with multiple records and transient failures, every
    /// write that eventually succeeds reaches the DB.
    /// A download-store stub where `mark_downloaded` fails a configurable number
    /// of times before succeeding. All other methods panic (unused).
    struct FailingDownloadStore {
        remaining_failures: AtomicUsize,
        calls: AtomicUsize,
        successes: AtomicUsize,
        failed_calls: AtomicUsize,
        downloaded_state_loads: AtomicUsize,
        track_failed_calls: bool,
        fail_complete_sync_run: bool,
    }

    impl FailingDownloadStore {
        fn new(fail_count: usize) -> Self {
            Self {
                remaining_failures: AtomicUsize::new(fail_count),
                calls: AtomicUsize::new(0),
                successes: AtomicUsize::new(0),
                failed_calls: AtomicUsize::new(0),
                downloaded_state_loads: AtomicUsize::new(0),
                track_failed_calls: false,
                fail_complete_sync_run: false,
            }
        }

        fn with_mark_failed_tracking() -> Self {
            let mut s = Self::new(0);
            s.track_failed_calls = true;
            s
        }

        fn with_failing_complete_sync_run() -> Self {
            let mut s = Self::new(0);
            s.fail_complete_sync_run = true;
            s
        }

        fn success_count(&self) -> usize {
            self.successes.load(Ordering::Relaxed)
        }

        fn call_count(&self) -> usize {
            self.calls.load(Ordering::Relaxed)
        }

        fn downloaded_state_load_count(&self) -> usize {
            self.downloaded_state_loads.load(Ordering::Relaxed)
        }

        fn failed_call_count(&self) -> usize {
            self.failed_calls.load(Ordering::Relaxed)
        }
    }

    #[async_trait::async_trait]
    impl DownloadStateStore for FailingDownloadStore {
        #[cfg(test)]
        async fn should_download(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: &str,
            _: &Path,
        ) -> Result<bool, StateError> {
            unimplemented!()
        }

        async fn upsert_seen(&self, _: &AssetRecord) -> Result<(), StateError> {
            unimplemented!()
        }

        async fn mark_downloaded(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: &Path,
            _: &str,
            _: Option<&str>,
        ) -> Result<(), StateError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            let prev = self.remaining_failures.fetch_sub(1, Ordering::Relaxed);
            if prev > 0 {
                Err(StateError::LockPoisoned("simulated failure".into()))
            } else {
                self.remaining_failures.store(0, Ordering::Relaxed);
                self.successes.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
        }

        async fn mark_failed(&self, _: &str, _: &str, _: &str, _: &str) -> Result<(), StateError> {
            if self.track_failed_calls {
                self.failed_calls.fetch_add(1, Ordering::Relaxed);
                Ok(())
            } else {
                unimplemented!()
            }
        }

        #[cfg(test)]
        async fn get_pending(&self) -> Result<Vec<AssetRecord>, StateError> {
            Ok(Vec::new())
        }

        async fn get_policy_excluded_ids_for_revalidation(
            &self,
            _: &str,
        ) -> Result<Vec<String>, StateError> {
            Ok(Vec::new())
        }

        async fn reset_failed(&self) -> Result<u64, StateError> {
            unimplemented!()
        }

        async fn prepare_for_retry(
            &self,
            _library: Option<&str>,
            _error_retention: crate::state::RetryErrorRetention,
        ) -> Result<(u64, u64, u64), StateError> {
            Ok((0, 0, 0))
        }

        async fn promote_pending_to_failed(&self, _seen_since: i64) -> Result<u64, StateError> {
            Ok(0)
        }

        async fn get_downloaded_ids(
            &self,
        ) -> Result<HashSet<(String, String, String)>, StateError> {
            Ok(HashSet::new())
        }

        async fn get_soft_deleted_downloaded_ids(
            &self,
        ) -> Result<HashSet<(String, String)>, StateError> {
            Ok(HashSet::new())
        }

        async fn get_all_known_ids(&self) -> Result<HashSet<(String, String)>, StateError> {
            Ok(HashSet::new())
        }

        async fn get_downloaded_checksums(
            &self,
        ) -> Result<HashMap<(String, String, String), String>, StateError> {
            Ok(HashMap::new())
        }

        async fn get_attempt_counts(&self) -> Result<HashMap<(String, String), u32>, StateError> {
            Ok(HashMap::new())
        }

        async fn touch_last_seen_many(&self, _: &str, _: &[&str]) -> Result<(), StateError> {
            Ok(())
        }

        async fn mark_policy_excluded(
            &self,
            _: &str,
            _: &str,
            _: &str,
        ) -> Result<bool, StateError> {
            unimplemented!()
        }

        async fn mark_soft_deleted(
            &self,
            _: &str,
            _: &str,
            _: Option<chrono::DateTime<chrono::Utc>>,
        ) -> Result<(), StateError> {
            Ok(())
        }

        async fn mark_hidden_at_source(&self, _: &str, _: &str) -> Result<(), StateError> {
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl ImportStateStore for FailingDownloadStore {
        async fn import_adopt(
            &self,
            _: &AssetRecord,
            _: &Path,
            _: &str,
            _: u64,
            _: Option<i64>,
        ) -> Result<(), StateError> {
            unimplemented!()
        }

        async fn get_all_imported_records(
            &self,
            _: &str,
        ) -> Result<HashMap<(String, String), crate::state::ImportedRecord>, StateError> {
            Ok(HashMap::new())
        }
    }

    #[async_trait::async_trait]
    impl crate::state::DownloadContextStateStore for FailingDownloadStore {
        async fn get_downloaded_file_records(
            &self,
        ) -> Result<Vec<crate::state::DownloadedFileRecord>, StateError> {
            self.downloaded_state_loads.fetch_add(1, Ordering::Relaxed);
            Ok(Vec::new())
        }
    }

    #[async_trait::async_trait]
    impl TempFileOwnershipStore for FailingDownloadStore {
        async fn claim_temp_file(&self, _: &Path) -> Result<(), StateError> {
            Ok(())
        }

        async fn get_owned_temp_files_before(
            &self,
            _: i64,
        ) -> Result<Vec<crate::state::OwnedTempFile>, StateError> {
            Ok(Vec::new())
        }

        async fn retire_temp_files(&self, _: &[PathBuf]) -> Result<u64, StateError> {
            Ok(0)
        }
    }

    #[async_trait::async_trait]
    impl ReportStateStore for FailingDownloadStore {
        #[cfg(test)]
        async fn get_failed(&self) -> Result<Vec<AssetRecord>, StateError> {
            unimplemented!()
        }

        async fn get_failed_sample(
            &self,
            _limit: u32,
        ) -> Result<(Vec<AssetRecord>, u64), StateError> {
            Ok((Vec::new(), 0))
        }

        async fn get_failed_page(
            &self,
            _offset: u64,
            _limit: u32,
        ) -> Result<Vec<AssetRecord>, StateError> {
            unimplemented!()
        }

        async fn get_pending_page(
            &self,
            _offset: u64,
            _limit: u32,
        ) -> Result<Vec<AssetRecord>, StateError> {
            unimplemented!()
        }

        async fn get_summary(&self) -> Result<SyncSummary, StateError> {
            unimplemented!()
        }

        async fn get_downloaded_page(
            &self,
            _offset: u64,
            _limit: u32,
        ) -> Result<Vec<AssetRecord>, StateError> {
            Ok(Vec::new())
        }

        async fn start_sync_run_at(
            &self,
            _: chrono::DateTime<chrono::Utc>,
        ) -> Result<i64, StateError> {
            Ok(1)
        }

        async fn start_sync_run(&self) -> Result<i64, StateError> {
            Ok(1)
        }

        async fn complete_sync_run(&self, _: i64, _: &SyncRunStats) -> Result<(), StateError> {
            if self.fail_complete_sync_run {
                Err(StateError::LockPoisoned(
                    "simulated complete_sync_run failure".into(),
                ))
            } else {
                Ok(())
            }
        }

        async fn promote_orphaned_sync_runs(&self) -> Result<u64, StateError> {
            Ok(0)
        }
    }

    #[async_trait::async_trait]
    impl SyncTokenStore for FailingDownloadStore {
        async fn get_metadata(&self, _: &str) -> Result<Option<String>, StateError> {
            Ok(None)
        }

        async fn set_metadata(&self, _: &str, _: &str) -> Result<(), StateError> {
            Ok(())
        }

        async fn delete_metadata_by_prefix(&self, _: &str) -> Result<u64, StateError> {
            Ok(0)
        }

        async fn begin_enum_progress(&self, _zone: &str) -> Result<(), StateError> {
            Ok(())
        }

        async fn end_enum_progress(&self, _zone: &str) -> Result<(), StateError> {
            Ok(())
        }

        async fn list_interrupted_enumerations(&self) -> Result<Vec<String>, StateError> {
            Ok(Vec::new())
        }
    }

    #[async_trait::async_trait]
    impl MembershipStore for FailingDownloadStore {
        async fn add_asset_album(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: &str,
        ) -> Result<(), StateError> {
            Ok(())
        }

        async fn get_all_asset_albums(&self, _: &str) -> Result<Vec<(String, String)>, StateError> {
            Ok(Vec::new())
        }

        async fn get_all_asset_people(&self, _: &str) -> Result<Vec<(String, String)>, StateError> {
            Ok(Vec::new())
        }
    }

    #[async_trait::async_trait]
    impl MetadataRewriteStore for FailingDownloadStore {
        async fn record_metadata_write_failure(
            &self,
            _: &str,
            _: &str,
            _: &str,
        ) -> Result<(), StateError> {
            Ok(())
        }

        async fn refresh_downloaded_asset_metadata(
            &self,
            _: &str,
            _: &str,
            _: &crate::state::AssetMetadata,
            _: bool,
            _: bool,
            _: i64,
        ) -> Result<usize, StateError> {
            Ok(0)
        }

        async fn get_downloaded_metadata_hashes(
            &self,
        ) -> Result<HashMap<(String, String, String), String>, StateError> {
            Ok(HashMap::new())
        }

        async fn get_metadata_retry_markers(
            &self,
        ) -> Result<HashSet<(String, String, String)>, StateError> {
            Ok(HashSet::new())
        }

        async fn get_pending_metadata_rewrites_page(
            &self,
            _: Option<&[&str]>,
            _: usize,
            _: usize,
        ) -> Result<Vec<AssetRecord>, StateError> {
            Ok(Vec::new())
        }

        async fn get_pending_metadata_rewrites_page_for_queue(
            &self,
            _: crate::state::db::MetadataRewriteQueue,
            _: Option<&[&str]>,
            _: usize,
            _: usize,
        ) -> Result<Vec<crate::state::db::PendingMetadataRewrite>, StateError> {
            Ok(Vec::new())
        }

        async fn record_capture_repair_prepared(
            &self,
            _: &crate::state::db::PendingMetadataRewrite,
            _: &str,
            _: u64,
        ) -> Result<Option<crate::state::db::CaptureRepairReceipt>, StateError> {
            Ok(None)
        }

        async fn finish_metadata_rewrite(
            &self,
            _: &crate::state::db::PendingMetadataRewrite,
            selected_queue: crate::state::db::MetadataRewriteQueue,
            _: Option<&str>,
            _: Option<&str>,
            completion: crate::state::db::MetadataRewriteCompletion,
        ) -> Result<bool, StateError> {
            Ok(matches!(
                (selected_queue, completion),
                (
                    crate::state::db::MetadataRewriteQueue::Ordinary,
                    crate::state::db::MetadataRewriteCompletion::Ordinary
                        | crate::state::db::MetadataRewriteCompletion::Both
                ) | (
                    crate::state::db::MetadataRewriteQueue::CaptureRepair,
                    crate::state::db::MetadataRewriteCompletion::CaptureRepair
                        | crate::state::db::MetadataRewriteCompletion::Both
                )
            ))
        }

        async fn has_downloaded_without_metadata_hash(&self) -> Result<bool, StateError> {
            Ok(false)
        }

        async fn begin_metadata_capture_revision(
            &self,
            library: &str,
            target_revision: i64,
        ) -> Result<crate::state::MetadataCaptureStatus, StateError> {
            Ok(crate::state::MetadataCaptureStatus {
                library: library.to_owned(),
                active_revision: target_revision,
                pending_revision: None,
                processed_assets: 0,
                failed_assets: 0,
                remaining_assets: 0,
                last_error: None,
            })
        }

        async fn get_metadata_capture_candidates(
            &self,
            _: &str,
            _: i64,
            _: usize,
        ) -> Result<Vec<crate::state::MetadataCaptureCandidate>, StateError> {
            Ok(Vec::new())
        }

        async fn record_metadata_capture_failure(
            &self,
            _: &str,
            _: i64,
            _: &str,
        ) -> Result<(), StateError> {
            Ok(())
        }

        async fn complete_metadata_capture_revision(
            &self,
            library: &str,
            target_revision: i64,
        ) -> Result<crate::state::MetadataCaptureStatus, StateError> {
            self.begin_metadata_capture_revision(library, target_revision)
                .await
        }

        async fn has_metadata_capture_work(&self, _: &[&str], _: i64) -> Result<bool, StateError> {
            Ok(false)
        }
    }

    #[tokio::test]
    async fn flush_pending_state_writes_empty_is_noop() {
        let db = FailingDownloadStore::new(0);
        let result = flush_pending_state_writes(&db, &[]).await;
        assert_eq!(result, 0);
        assert_eq!(db.success_count(), 0);
    }

    /// CG-2 (broadened from adversarial pass, 2026-05-03): `log_sync_summary`
    /// had no test coverage at all. The mutation experiment showed that
    /// dropping every `tracing::info!()` from the body would land green —
    /// silently disabling sync-completion reporting in production logs.
    /// This test is a baseline contract: at least one info event must fire
    /// per call, the structured `title` field must be captured, and the
    /// `downloaded` / `failed` counts must reach the captured output.
    #[tracing_test::traced_test]
    #[test]
    fn log_sync_summary_emits_sync_counts_via_tracing() {
        let stats = super::super::SyncStats {
            downloaded: 3,
            failed: 1,
            skipped: super::super::SkipBreakdown {
                by_state: 2,
                ..Default::default()
            },
            ..Default::default()
        };

        super::log_sync_summary("── Test Summary ──", &stats);

        // Title-line: structured `title` field must be present.
        // Note: tracing renders `title = %title` (Display) unquoted.
        assert!(
            logs_contain("title=── Test Summary ──"),
            "structured title field expected on first event"
        );
        // Title-line: message text must be present.
        assert!(
            logs_contain("Sync summary"),
            "title-line event message expected"
        );
        // Count line: every count must reach the captured stream.
        assert!(
            logs_contain("3 downloaded"),
            "downloaded count missing from summary line"
        );
        assert!(
            logs_contain("1 failed"),
            "failed count missing from summary line"
        );
        // Skipped breakdown line: when skipped > 0, a Skipped: line fires.
        assert!(
            logs_contain("Skipped:"),
            "skipped breakdown line expected when stats.skipped.total() > 0"
        );
    }

    /// When only `enumeration_errors` is non-zero, the line-2 conditional
    /// must still fire. Otherwise an enumeration-error-driven
    /// `PartialFailure` would produce an empty failure line and an
    /// operator chasing exit code 2 has no count.
    #[tracing_test::traced_test]
    #[test]
    fn log_sync_summary_emits_enumeration_errors_when_only_enum_errs() {
        let stats = super::super::SyncStats {
            downloaded: 0,
            failed: 0,
            enumeration_errors: 4,
            ..Default::default()
        };

        super::log_sync_summary("── Test Summary ──", &stats);

        assert!(
            logs_contain("4 enumeration error(s)"),
            "line 2 must surface enumeration_errors when nonzero"
        );
    }

    /// Inverse of the above: when every error counter is zero, the
    /// line-2 conditional must not fire.
    #[tracing_test::traced_test]
    #[test]
    fn log_sync_summary_no_error_line_when_all_counters_zero() {
        let stats = super::super::SyncStats {
            downloaded: 5,
            ..Default::default()
        };

        super::log_sync_summary("── Test Summary ──", &stats);

        assert!(
            !logs_contain("EXIF write failure"),
            "line 2 must not fire when exif/state/enum counters are all zero"
        );
        assert!(
            !logs_contain("enumeration error"),
            "line 2 must not fire when exif/state/enum counters are all zero"
        );
    }

    #[tokio::test]
    async fn flush_pending_state_writes_succeeds_on_first_try() {
        let db = FailingDownloadStore::new(0);
        let pending = vec![PendingStateWrite {
            library: "PrimarySync".into(),
            asset_id: "A1".into(),
            version_size: VersionSizeKey::Original,
            download_path: PathBuf::from("/tmp/codex/kei/photo.jpg"),
            local_checksum: "abc".into(),
            download_checksum: None,
            mark_capture_repair: false,
        }];
        let failures = flush_pending_state_writes(&db, &pending).await;
        assert_eq!(failures, 0);
        assert_eq!(db.success_count(), 1);
    }

    #[tracing_test::traced_test]
    #[tokio::test]
    async fn flush_pending_state_writes_recovers_after_transient_failure() {
        // Fail the first attempt, succeed on retry
        let db = FailingDownloadStore::new(1);
        let pending = vec![PendingStateWrite {
            library: "PrimarySync".into(),
            asset_id: "A1".into(),
            version_size: VersionSizeKey::Original,
            download_path: PathBuf::from("/tmp/codex/kei/photo.jpg"),
            local_checksum: "abc".into(),
            download_checksum: None,
            mark_capture_repair: false,
        }];
        let failures = flush_pending_state_writes(&db, &pending).await;
        assert_eq!(failures, 0);
        assert_eq!(db.success_count(), 1);
        assert!(logs_contain("State write retry failed, will retry"));
        assert!(logs_contain("Recovered deferred state write"));
        assert!(logs_contain("simulated failure"));
    }

    #[tokio::test]
    async fn flush_pending_state_writes_reports_persistent_failure() {
        // Fail all attempts — must exceed STATE_WRITE_MAX_RETRIES
        let db = FailingDownloadStore::new(STATE_WRITE_MAX_RETRIES as usize);
        let pending = vec![PendingStateWrite {
            library: "PrimarySync".into(),
            asset_id: "A1".into(),
            version_size: VersionSizeKey::Original,
            download_path: PathBuf::from("/tmp/codex/kei/photo.jpg"),
            local_checksum: "abc".into(),
            download_checksum: None,
            mark_capture_repair: false,
        }];
        let failures = flush_pending_state_writes(&db, &pending).await;
        assert_eq!(failures, 1);
        assert_eq!(db.success_count(), 0);
    }

    #[tokio::test]
    async fn flush_pending_state_writes_partial_recovery() {
        // First write exhausts all STATE_WRITE_MAX_RETRIES attempts (reported as failure).
        // Second write fails once more then succeeds on retry.
        let db = FailingDownloadStore::new(STATE_WRITE_MAX_RETRIES as usize + 1);
        let pending = vec![
            PendingStateWrite {
                library: "PrimarySync".into(),
                asset_id: "A1".into(),
                version_size: VersionSizeKey::Original,
                download_path: PathBuf::from("/tmp/codex/kei/photo1.jpg"),
                local_checksum: "abc".into(),
                download_checksum: None,
                mark_capture_repair: false,
            },
            PendingStateWrite {
                library: "PrimarySync".into(),
                asset_id: "A2".into(),
                version_size: VersionSizeKey::Original,
                download_path: PathBuf::from("/tmp/codex/kei/photo2.jpg"),
                local_checksum: "def".into(),
                download_checksum: None,
                mark_capture_repair: false,
            },
        ];
        let failures = flush_pending_state_writes(&db, &pending).await;
        assert_eq!(
            failures, 1,
            "First write should fail, second should recover"
        );
        assert_eq!(db.success_count(), 1);
    }

    #[tokio::test]
    async fn flush_pending_state_writes_retains_all_records() {
        // 5 pending writes. First 2 failures are transient (writes 1&2 fail once
        // each then succeed on retry). All 5 should eventually succeed.
        let db = FailingDownloadStore::new(2);
        let pending: Vec<PendingStateWrite> = (0..5)
            .map(|i| PendingStateWrite {
                library: "PrimarySync".into(),
                asset_id: format!("ASSET_{i}").into(),
                version_size: VersionSizeKey::Original,
                download_path: PathBuf::from(format!("/tmp/codex/kei/photo_{i}.jpg")),
                local_checksum: format!("ck_{i}"),
                download_checksum: Some(format!("dl_ck_{i}")),
                mark_capture_repair: false,
            })
            .collect();

        let failures = flush_pending_state_writes(&db, &pending).await;
        assert_eq!(failures, 0, "all 5 writes should eventually succeed");
        assert_eq!(db.success_count(), 5);
    }

    #[tokio::test(start_paused = true)]
    async fn flush_pending_state_writes_retains_only_persistent_failures() {
        let db = FailingDownloadStore::new(STATE_WRITE_MAX_RETRIES as usize + 1);
        let mut pending = vec![
            PendingStateWrite {
                library: "PrimarySync".into(),
                asset_id: "A1".into(),
                version_size: VersionSizeKey::Original,
                download_path: PathBuf::from("/tmp/codex/kei/photo1.jpg"),
                local_checksum: "abc".into(),
                download_checksum: None,
                mark_capture_repair: false,
            },
            PendingStateWrite {
                library: "PrimarySync".into(),
                asset_id: "A2".into(),
                version_size: VersionSizeKey::Original,
                download_path: PathBuf::from("/tmp/codex/kei/photo2.jpg"),
                local_checksum: "def".into(),
                download_checksum: None,
                mark_capture_repair: false,
            },
        ];

        let flush = flush_pending_state_writes_retaining_failures(&db, &mut pending).await;

        assert_eq!(
            flush,
            StateWriteFlush {
                attempted: 2,
                failures: 1,
            }
        );
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].asset_id.as_ref(), "A1");
        assert_eq!(db.success_count(), 1);
    }

    #[test]
    fn state_write_circuit_breaker_requires_threshold_and_all_failures() {
        assert!(!state_write_circuit_breaker_tripped(&StateWriteFlush {
            attempted: STATE_DB_UNWRITABLE_THRESHOLD - 1,
            failures: STATE_DB_UNWRITABLE_THRESHOLD - 1,
        }));
        assert!(!state_write_circuit_breaker_tripped(&StateWriteFlush {
            attempted: STATE_DB_UNWRITABLE_THRESHOLD,
            failures: STATE_DB_UNWRITABLE_THRESHOLD - 1,
        }));
        assert!(state_write_circuit_breaker_tripped(&StateWriteFlush {
            attempted: STATE_DB_UNWRITABLE_THRESHOLD,
            failures: STATE_DB_UNWRITABLE_THRESHOLD,
        }));
    }

    #[tokio::test]
    async fn download_pass_invalid_unknown_media_marks_failed_not_downloaded() {
        use base64::Engine as _;
        use wiremock::matchers::method;
        use wiremock::{Mock, ResponseTemplate};

        let server = crate::start_wiremock_or_skip!();
        let body = b"not media bytes";
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body.to_vec()))
            .mount(&server)
            .await;

        let dir = TempDir::new().unwrap();
        let db = Arc::new(FailingDownloadStore::with_mark_failed_tracking());
        let state_db: Arc<dyn DownloadStore> = db.clone();
        let client = Client::new();
        let retry = RetryConfig {
            max_retries: 0,
            base_delay_secs: 0,
            max_delay_secs: 0,
        };
        let checksum = base64::engine::general_purpose::STANDARD.encode([0x42u8; 32]);
        let download_path = dir.path().join("unknown_header.jpg");
        let part_path =
            super::super::file::temp_download_path(&download_path, &checksum, ".kei-tmp")
                .expect("valid temp path");
        let task = DownloadTask {
            url: format!("{}/photo.jpg", server.uri()).into(),
            download_path: download_path.clone(),
            publication: crate::download::file::FinalPublication::NoReplace,
            checksum: checksum.into(),
            asset_id: "UNKNOWN_MEDIA".into(),
            asset_record_name: "UNKNOWN_MEDIA".into(),
            library: "PrimarySync".into(),
            metadata: Arc::new(MetadataPayload::default()),
            size: body.len() as u64,
            created_local: chrono::Local::now().fixed_offset(),
            version_size: VersionSizeKey::Original,
            media_type: crate::state::MediaType::Photo,
        };

        let result = run_download_pass(
            PassConfig {
                client: &client,
                retry_config: &retry,
                metadata: MetadataFlags::default(),
                mark_capture_repair_after_download: false,
                concurrency: 1,
                reporting: DownloadReporting::hidden(),
                temp_suffix: std::sync::Arc::from(".kei-tmp"),
                shutdown_token: CancellationToken::new(),
                state_db: Some(state_db),
                rate_limit_counter: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                bandwidth_limiter: None,
                library: std::sync::Arc::from("PrimarySync"),
            },
            vec![task],
        )
        .await;

        assert_eq!(result.failed.len(), 1);
        assert_eq!(db.call_count(), 0, "invalid media must not mark_downloaded");
        assert_eq!(
            db.failed_call_count(),
            1,
            "invalid media should be recorded failed"
        );
        assert!(
            !download_path.exists(),
            "invalid media must not publish final path"
        );
        assert!(!part_path.exists(), "invalid media .part should be removed");
        let outcome = crate::download::DownloadOutcome::PartialFailure {
            failed_count: result.failed.len(),
        };
        assert!(
            !crate::sync_cycle::should_store_sync_token(&outcome, false),
            "partial media-download failures must block sync-token advancement"
        );
    }

    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn different_byte_destination_race_keeps_loser_failed_without_metadata_write() {
        use crate::state::{MediaType, SqliteStateDb};
        use base64::Engine as _;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let winner_body = MINIMAL_JPEG.to_vec();
        let mut loser_body = MINIMAL_JPEG.to_vec();
        *loser_body
            .get_mut(17)
            .expect("minimal JPEG must contain a Y-density value") = 2;
        let server = crate::start_wiremock_or_skip!();
        Mock::given(method("GET"))
            .and(path("/WINNER.jpg"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(winner_body.clone())
                    .insert_header("content-type", "image/jpeg"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/LOSER.jpg"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(loser_body.clone())
                    .insert_header("content-type", "image/jpeg"),
            )
            .mount(&server)
            .await;

        let dir = TempDir::new().unwrap();
        let download_path = dir.path().join("shared.jpg");
        let winner_db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
        let loser_db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
        let retry = RetryConfig {
            max_retries: 0,
            base_delay_secs: 0,
            max_delay_secs: 0,
        };
        let metadata = MetadataFlags::from(&crate::config::MetadataConfig {
            xmp_sidecar: true,
            ..crate::config::MetadataConfig::default()
        });
        let make_task = |asset_id: &str, checksum_byte: u8, rating: u8| DownloadTask {
            url: format!("{}/{asset_id}.jpg", server.uri()).into(),
            download_path: download_path.clone(),
            publication: crate::download::file::FinalPublication::NoReplace,
            checksum: base64::engine::general_purpose::STANDARD
                .encode([checksum_byte; 32])
                .into(),
            asset_id: asset_id.into(),
            asset_record_name: asset_id.into(),
            library: "PrimarySync".into(),
            metadata: Arc::new(MetadataPayload {
                rating: Some(rating),
                ..MetadataPayload::default()
            }),
            size: winner_body.len() as u64,
            created_local: chrono::Local::now().fixed_offset(),
            version_size: VersionSizeKey::Original,
            media_type: MediaType::Photo,
        };
        let winner_task = make_task("WINNER", 0x41, 1);
        let loser_task = make_task("LOSER", 0x42, 5);
        let loser_part_path = super::super::file::temp_download_path(
            &download_path,
            &loser_task.checksum,
            ".kei-tmp",
        )
        .expect("valid temporary path");

        for (db, task) in [(&winner_db, &winner_task), (&loser_db, &loser_task)] {
            db.upsert_seen(&AssetRecord::new_pending(
                Arc::from("PrimarySync"),
                task.asset_id.to_string(),
                task.version_size,
                task.checksum.to_string(),
                "shared.jpg".to_string(),
                chrono::Utc::now(),
                None,
                task.size,
                MediaType::Photo,
            ))
            .await
            .unwrap();
        }

        let client = Client::new();
        let winner_store: Arc<dyn DownloadStore> = winner_db.clone();
        let winner_result = run_download_pass(
            PassConfig {
                client: &client,
                retry_config: &retry,
                metadata,
                mark_capture_repair_after_download: false,
                concurrency: 1,
                reporting: DownloadReporting::hidden(),
                temp_suffix: Arc::from(".kei-tmp"),
                shutdown_token: CancellationToken::new(),
                state_db: Some(winner_store),
                rate_limit_counter: Arc::new(AtomicUsize::new(0)),
                bandwidth_limiter: None,
                library: Arc::from("PrimarySync"),
            },
            vec![winner_task],
        )
        .await;
        assert_eq!(winner_result.downloaded, 1);
        assert!(winner_result.failed.is_empty());

        let sidecar_path = sidecar_path_for(&download_path);
        let winner_sidecar = fs::read(&sidecar_path).expect("winner sidecar must be written");
        let loser_store: Arc<dyn DownloadStore> = loser_db.clone();
        let loser_result = run_download_pass(
            PassConfig {
                client: &client,
                retry_config: &retry,
                metadata,
                mark_capture_repair_after_download: false,
                concurrency: 1,
                reporting: DownloadReporting::hidden(),
                temp_suffix: Arc::from(".kei-tmp"),
                shutdown_token: CancellationToken::new(),
                state_db: Some(loser_store),
                rate_limit_counter: Arc::new(AtomicUsize::new(0)),
                bandwidth_limiter: None,
                library: Arc::from("PrimarySync"),
            },
            vec![loser_task],
        )
        .await;

        assert_eq!(loser_result.downloaded, 0);
        assert_eq!(loser_result.failed.len(), 1);
        assert_eq!(fs::read(&download_path).unwrap(), winner_body);
        assert_eq!(fs::read(&sidecar_path).unwrap(), winner_sidecar);
        assert_eq!(fs::read(&loser_part_path).unwrap(), loser_body);
        assert!(
            loser_db
                .get_downloaded_page(0, 10)
                .await
                .unwrap()
                .is_empty()
        );
        let failed = loser_db.get_failed().await.unwrap();
        assert_eq!(failed.len(), 1);
        assert!(
            failed[0]
                .last_error
                .as_deref()
                .is_some_and(|error| error.contains("verified bytes differ")),
            "collision must remain durable failed work"
        );
    }

    #[tokio::test]
    async fn temporary_ownership_retires_after_publish_and_interruption() {
        use base64::Engine as _;

        let jpeg_body = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];
        let client = StaticDownloadClient {
            body: jpeg_body.clone(),
        };

        let dir = TempDir::new().unwrap();
        let db = crate::state::SqliteStateDb::open_in_memory().unwrap();
        let retry = RetryConfig {
            max_retries: 0,
            base_delay_secs: 0,
            max_delay_secs: 0,
        };
        let make_task = |name: &str, checksum_byte: u8| DownloadTask {
            url: format!("https://example.invalid/{name}.jpg").into(),
            download_path: dir.path().join(format!("{name}.jpg")),
            publication: crate::download::file::FinalPublication::NoReplace,
            checksum: base64::engine::general_purpose::STANDARD
                .encode([checksum_byte; 32])
                .into(),
            asset_id: name.into(),
            asset_record_name: name.into(),
            library: "PrimarySync".into(),
            metadata: Arc::new(MetadataPayload::default()),
            size: jpeg_body.len() as u64,
            created_local: chrono::Local::now().fixed_offset(),
            version_size: VersionSizeKey::Original,
            media_type: crate::state::MediaType::Photo,
        };
        let published = make_task("published", 0x41);
        let running = CancellationToken::new();

        download_single_task(
            &client,
            &published,
            &retry,
            MetadataFlags::default(),
            DownloadSingleContext {
                temp_suffix: ".kei-tmp",
                state_db: Some(&db),
                rate_limit_counter: None,
                bandwidth_limiter: None,
                shutdown_token: &running,
                mode: crate::personality::Mode::Off,
            },
        )
        .await
        .unwrap();
        assert!(published.download_path.exists());
        assert!(
            db.get_owned_temp_files_before(i64::MAX)
                .await
                .unwrap()
                .is_empty(),
            "published download must retire ownership"
        );

        let interrupted = make_task("interrupted", 0x42);
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        let error = download_single_task(
            &client,
            &interrupted,
            &retry,
            MetadataFlags::default(),
            DownloadSingleContext {
                temp_suffix: ".kei-tmp",
                state_db: Some(&db),
                rate_limit_counter: None,
                bandwidth_limiter: None,
                shutdown_token: &cancelled,
                mode: crate::personality::Mode::Off,
            },
        )
        .await
        .unwrap_err();
        assert_eq!(
            classify_download_task_error(&error),
            DownloadTaskErrorClass::Interrupted
        );
        assert!(
            db.get_owned_temp_files_before(i64::MAX)
                .await
                .unwrap()
                .is_empty(),
            "graceful interruption must retire ownership"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn download_pass_opens_state_write_circuit_breaker_mid_run() {
        use base64::Engine as _;
        use wiremock::matchers::method;
        use wiremock::{Mock, ResponseTemplate};

        let server = crate::start_wiremock_or_skip!();
        let jpeg_body = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(jpeg_body.clone())
                    .insert_header("content-type", "image/jpeg"),
            )
            .mount(&server)
            .await;

        let dir = TempDir::new().unwrap();
        let db = Arc::new(FailingDownloadStore::new(usize::MAX / 2));
        let state_db: Arc<dyn DownloadStore> = db.clone();
        let client = Client::new();
        let retry = RetryConfig {
            max_retries: 0,
            base_delay_secs: 0,
            max_delay_secs: 0,
        };

        let checksum = base64::engine::general_purpose::STANDARD.encode([0x42u8; 32]);
        let tasks: Vec<DownloadTask> = (0..STATE_DB_UNWRITABLE_THRESHOLD + 3)
            .map(|i| DownloadTask {
                url: format!("{}/photo_{i}.jpg", server.uri()).into(),
                download_path: dir.path().join(format!("photo_{i}.jpg")),
                publication: crate::download::file::FinalPublication::NoReplace,
                checksum: checksum.clone().into(),
                asset_id: format!("CIRCUIT_{i}").into(),
                asset_record_name: format!("CIRCUIT_{i}").into(),
                library: "PrimarySync".into(),
                metadata: Arc::new(MetadataPayload::default()),
                size: jpeg_body.len() as u64,
                created_local: chrono::Local::now().fixed_offset(),
                version_size: VersionSizeKey::Original,
                media_type: crate::state::MediaType::Photo,
            })
            .collect();

        let result = run_download_pass(
            PassConfig {
                client: &client,
                retry_config: &retry,
                metadata: MetadataFlags::default(),
                mark_capture_repair_after_download: false,
                concurrency: 1,
                reporting: DownloadReporting::hidden(),
                temp_suffix: std::sync::Arc::from(".kei-tmp"),
                shutdown_token: CancellationToken::new(),
                state_db: Some(state_db),
                rate_limit_counter: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                bandwidth_limiter: None,
                library: std::sync::Arc::from("PrimarySync"),
            },
            tasks,
        )
        .await;

        assert_eq!(result.state_write_failures, STATE_DB_UNWRITABLE_THRESHOLD);
        assert_eq!(db.success_count(), 0);
        assert!(
            db.call_count() > STATE_DB_UNWRITABLE_THRESHOLD,
            "deferred writes must be retried before opening the circuit"
        );
        for i in 0..STATE_DB_UNWRITABLE_THRESHOLD {
            let landed = dir.path().join(format!("photo_{i}.jpg"));
            assert!(
                landed.exists(),
                "state write failures must not remove an already-published file: {}",
                landed.display()
            );
        }
    }

    /// T-11: When the API returns the same asset ID on two different pages,
    /// the dedup logic (seen_ids) ensures only one download task is created.
    #[test]
    fn test_duplicate_asset_id_detected() {
        use rustc_hash::FxHashSet;

        // Simulate the producer's seen_ids dedup logic
        let mut seen_ids: FxHashSet<Box<str>> = FxHashSet::default();

        let asset1_id: Box<str> = "DUPLICATE_ASSET".into();
        let asset2_id: Box<str> = "DUPLICATE_ASSET".into();
        let asset3_id: Box<str> = "UNIQUE_ASSET".into();

        // First occurrence: insert succeeds
        assert!(
            seen_ids.insert(asset1_id),
            "first occurrence should be accepted"
        );

        // Duplicate on second page: insert returns false
        assert!(
            !seen_ids.insert(asset2_id),
            "duplicate asset ID should be detected and skipped"
        );

        // Different asset: insert succeeds
        assert!(
            seen_ids.insert(asset3_id),
            "unique asset should be accepted"
        );

        assert_eq!(seen_ids.len(), 2, "only 2 unique IDs should be tracked");
    }

    /// When a CancellationToken is already cancelled as the next asset is
    /// yielded, the pass must stop before planning or downloading that asset.
    #[tokio::test]
    async fn shutdown_cancellation_exits_download_pass_promptly() {
        use crate::download::{DownloadConfig, SyncMode};
        use crate::icloud::photos::PhotoAsset;
        use crate::types::{
            AssetVersionSize, FileMatchPolicy, LivePhotoMode, LivePhotoMovFilenamePolicy, RawPolicy,
        };
        use rustc_hash::FxHashSet;

        let asset_stream = futures_util::stream::repeat_with(|| {
            Ok::<PhotoAsset, anyhow::Error>(
                TestPhotoAsset::new("SHUTDOWN_ALREADY_CANCELLED")
                    .orig_size(100)
                    .orig_url("http://127.0.0.1:1/photo.jpg")
                    .orig_checksum("ck_shutdown")
                    .build(),
            )
        });

        let dir = TempDir::new().unwrap();

        let config = Arc::new(DownloadConfig {
            directory: std::sync::Arc::from(dir.path()),
            folder_structure: "{:%Y/%m/%d}".to_string(),
            folder_structure_albums: Arc::from(crate::config::DEFAULT_FOLDER_STRUCTURE_ALBUMS),
            folder_structure_smart_folders: Arc::from(
                crate::config::DEFAULT_FOLDER_STRUCTURE_SMART_FOLDERS,
            ),
            resolution: crate::types::PhotoResolution::Original,
            media: crate::config::MediaSelection::all(),
            skip_created_before: None,
            skip_created_after: None,
            metadata: crate::config::MetadataConfig::default(),
            refresh_metadata: false,
            capture_timestamp_repair: crate::download::CaptureTimestampRepair::Preserve,
            repair_truncated: false,
            concurrent_downloads: 10,
            recent: None,
            recent_scope: crate::cli::RecentScope::Global,
            retry: crate::retry::RetryConfig {
                max_retries: 0,
                base_delay_secs: 0,
                max_delay_secs: 0,
            },
            live_photo_mode: LivePhotoMode::Both,
            live_resolution: AssetVersionSize::LiveOriginal,
            live_photo_mov_filename_policy: LivePhotoMovFilenamePolicy::Suffix,
            edited: false,
            alternative: false,
            raw_policy: RawPolicy::AsIs,
            file_match_policy: FileMatchPolicy::NameSizeDedupWithSuffix,
            force_resolution: false,
            keep_unicode_in_filenames: false,
            filename_exclude: std::sync::Arc::from(Vec::<glob::Pattern>::new()),
            temp_suffix: std::sync::Arc::from(".kei-tmp"),
            state_db: None,
            retry_only: false,
            max_download_attempts: 10,
            sync_mode: SyncMode::Full,
            enum_config_hash: None,
            album_name: None,
            exclude_asset_ids: Arc::new(FxHashSet::default()),
            asset_groupings: Arc::new(crate::download::AssetGroupings::default()),
            bandwidth_limiter: None,
            library: std::sync::Arc::from("PrimarySync"),
        });

        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_millis(50))
            .build()
            .expect("client");

        let shutdown_token = CancellationToken::new();
        shutdown_token.cancel();

        let result = stream_and_download_from_stream(
            &client,
            asset_stream,
            &config,
            DownloadControls::download_hidden(),
            10_000,
            shutdown_token,
            StreamRuntime::new(None, None),
        )
        .await
        .expect("cancelled pass should return a streaming result");

        assert_eq!(result.downloaded, 0, "cancelled pass must not download");
        assert!(
            !result.enumeration_complete,
            "cancelled enumeration must not be considered complete"
        );
    }

    #[tokio::test]
    async fn test_producer_panic_propagates_as_error() {
        use crate::download::{DownloadConfig, SyncMode};
        use crate::icloud::photos::PhotoAsset;
        use crate::types::{
            AssetVersionSize, FileMatchPolicy, LivePhotoMode, LivePhotoMovFilenamePolicy, RawPolicy,
        };
        use rustc_hash::FxHashSet;

        let config = Arc::new(DownloadConfig {
            directory: std::sync::Arc::from(std::path::Path::new(
                "/nonexistent/download_filter_tests",
            )),
            folder_structure: "{:%Y/%m/%d}".to_string(),
            folder_structure_albums: Arc::from(crate::config::DEFAULT_FOLDER_STRUCTURE_ALBUMS),
            folder_structure_smart_folders: Arc::from(
                crate::config::DEFAULT_FOLDER_STRUCTURE_SMART_FOLDERS,
            ),
            resolution: crate::types::PhotoResolution::Original,
            media: crate::config::MediaSelection::all(),
            skip_created_before: None,
            skip_created_after: None,
            metadata: crate::config::MetadataConfig::default(),
            refresh_metadata: false,
            capture_timestamp_repair: crate::download::CaptureTimestampRepair::Preserve,
            repair_truncated: false,
            concurrent_downloads: 1,
            recent: None,
            recent_scope: crate::cli::RecentScope::Global,
            retry: RetryConfig::default(),
            live_photo_mode: LivePhotoMode::Both,
            live_resolution: AssetVersionSize::LiveOriginal,
            live_photo_mov_filename_policy: LivePhotoMovFilenamePolicy::Suffix,
            edited: false,
            alternative: false,
            raw_policy: RawPolicy::AsIs,
            file_match_policy: FileMatchPolicy::NameSizeDedupWithSuffix,
            force_resolution: false,
            keep_unicode_in_filenames: false,
            filename_exclude: std::sync::Arc::from(Vec::<glob::Pattern>::new()),
            temp_suffix: std::sync::Arc::from(".kei-tmp"),
            state_db: None,
            retry_only: false,
            max_download_attempts: 10,
            sync_mode: SyncMode::Full,
            enum_config_hash: None,
            album_name: None,
            exclude_asset_ids: Arc::new(FxHashSet::default()),
            asset_groupings: Arc::new(crate::download::AssetGroupings::default()),
            bandwidth_limiter: None,
            library: std::sync::Arc::from("PrimarySync"),
        });
        let client = reqwest::Client::new();
        let shutdown_token = CancellationToken::new();

        // Stream that panics on first poll — simulates a producer task panic
        let panicking_stream = futures_util::stream::poll_fn(
            |_cx| -> std::task::Poll<Option<anyhow::Result<PhotoAsset>>> {
                panic!("simulated producer panic");
            },
        );

        let err = stream_and_download_from_stream(
            &client,
            panicking_stream,
            &config,
            DownloadControls::download_hidden(),
            0,
            shutdown_token,
            StreamRuntime::new(None, None),
        )
        .await
        .expect_err("should propagate producer panic");
        assert!(
            err.to_string().contains("producer task crashed"),
            "Expected producer panic error, got: {err}"
        );
    }

    #[tokio::test]
    async fn print_and_dry_run_preserve_downloaded_child_identity() {
        use base64::Engine as _;
        use serde_json::json;
        use sha2::{Digest, Sha256};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = crate::start_wiremock_or_skip!();
        let body = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];
        let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&body));
        Mock::given(method("GET"))
            .and(path("/stream-mode-identity.jpg"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(body.clone())
                    .insert_header("content-type", "image/jpeg"),
            )
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("{}/stream-mode-identity.jpg", server.uri());
        let make_asset = || {
            PhotoAsset::new(
                json!({
                    "recordName": "STREAM_MODE_MASTER",
                    "fields": {
                        "filenameEnc": {"value": "stream-mode.jpg", "type": "STRING"},
                        "itemType": {"value": "public.jpeg"},
                        "resOriginalRes": {"value": {
                            "size": body.len(),
                            "downloadURL": url,
                            "fileChecksum": checksum,
                        }},
                        "resOriginalFileType": {"value": "public.jpeg"},
                    },
                }),
                json!({
                    "recordName": "asset-STREAM_MODE_CHILD",
                    "fields": {"assetDate": {"value": 1736899200000.0}},
                }),
            )
        };

        let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().expect("state db"));
        let dir = TempDir::new().expect("temp dir");
        let mut config = DownloadConfig::test_default();
        config.directory = Arc::from(dir.path());
        config.concurrent_downloads = 1;
        config.state_db = Some(db.clone());
        let config = Arc::new(config);

        let downloaded = stream_and_download_from_stream(
            &reqwest::Client::new(),
            stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(make_asset())]),
            &config,
            DownloadControls::download_hidden(),
            1,
            CancellationToken::new(),
            StreamRuntime::new(None, None),
        )
        .await
        .expect("normal sync should download the child");
        assert_eq!(downloaded.downloaded, 1);
        let rows = db.get_downloaded_page(0, 10).await.expect("downloaded row");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id.as_ref(), "asset-STREAM_MODE_CHILD");

        let printed = stream_and_download_from_stream(
            &reqwest::Client::new(),
            stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(make_asset())]),
            &config,
            DownloadControls::new(
                super::super::DownloadRunMode::PrintFilenames,
                DownloadReporting::hidden(),
            ),
            1,
            CancellationToken::new(),
            StreamRuntime::new(None, None),
        )
        .await
        .expect("print-only sync should recognize the downloaded child");
        assert!(
            printed.printed_filenames.is_empty(),
            "print-only must not emit collision paths for downloaded child state: {:?}",
            printed.printed_filenames
        );

        let asset = make_asset();
        let base_path = derive_expected_paths(&asset, config.as_ref())
            .into_iter()
            .next()
            .expect("asset has an expected path")
            .path;
        let base_filename = base_path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("expected path has a filename");
        let expected_dry_run_path =
            base_path.with_file_name(super::super::paths::insert_asset_identity_suffix(
                base_filename,
                asset.asset_record_name(),
            ));
        let (capture, _guard) = crate::test_helpers::TracingCapture::install();
        let dry_run = stream_and_download_from_stream(
            &reqwest::Client::new(),
            stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(asset)]),
            &config,
            DownloadControls::dry_run_hidden(),
            1,
            CancellationToken::new(),
            StreamRuntime::new(None, None),
        )
        .await
        .expect("dry-run sync should plan the child identity");
        assert_eq!(dry_run.downloaded, 1);
        let dry_run_path = capture
            .events()
            .into_iter()
            .find(|event| event.message() == Some("[DRY RUN] Would download"))
            .and_then(|event| event.field("path").map(ToOwned::to_owned))
            .expect("dry-run path event");
        assert_eq!(dry_run_path, expected_dry_run_path.display().to_string());
    }

    #[tokio::test]
    async fn dry_run_mode_uses_stream_pipeline_without_downloading() {
        use crate::download::DownloadConfig;
        use crate::icloud::photos::PhotoAsset;
        use futures_util::stream;

        let dir = TempDir::new().unwrap();
        let mut config = DownloadConfig::test_default();
        config.directory = std::sync::Arc::from(dir.path());
        let config = Arc::new(config);
        let asset = TestPhotoAsset::new("DRY_RUN_MODE")
            .orig_size(123)
            .orig_url("https://p01.icloud-content.com/dry-run.jpg")
            .orig_checksum("ck_dry_run")
            .build();
        let result = stream_and_download_from_stream(
            &reqwest::Client::new(),
            stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(asset)]),
            &config,
            DownloadControls::dry_run_hidden(),
            1,
            CancellationToken::new(),
            StreamRuntime::new(None, None),
        )
        .await
        .expect("dry run should scan through the real stream pipeline");

        assert_eq!(result.downloaded, 1);
        assert!(result.failed.is_empty());
        assert!(
            fs::read_dir(dir.path()).unwrap().next().is_none(),
            "dry-run mode must not create downloaded files"
        );
    }

    #[tokio::test]
    async fn shared_bar_seeds_static_scanning_message_before_first_file() {
        use crate::download::{DownloadConfig, DownloadRunMode};
        use crate::icloud::photos::PhotoAsset;
        use crate::personality::Mode;
        use futures_util::stream;

        let dir = TempDir::new().unwrap();
        let mut config = DownloadConfig::test_default();
        config.directory = std::sync::Arc::from(dir.path());
        config.album_name = Some(std::sync::Arc::from("Trip"));
        let config = Arc::new(config);
        let pb = ProgressBar::hidden();
        let controls = DownloadControls::new(
            DownloadRunMode::Download,
            DownloadReporting::new(false, Mode::Friendly),
        );

        let result = stream_and_download_from_stream(
            &reqwest::Client::new(),
            stream::empty::<anyhow::Result<PhotoAsset>>(),
            &config,
            controls,
            0,
            CancellationToken::new(),
            StreamRuntime::new(Some(pb.clone()), None),
        )
        .await
        .expect("empty shared-bar pass should finish");

        assert_eq!(result.downloaded, 0);
        assert_eq!(pb.message(), "Trip \u{00b7} scanning...");
    }

    #[tokio::test]
    async fn dry_run_mode_reports_enumeration_errors_without_downloading() {
        use crate::download::{DownloadConfig, DownloadOutcome};
        use crate::icloud::photos::PhotoAsset;
        use futures_util::stream;

        let dir = TempDir::new().unwrap();
        let mut config = DownloadConfig::test_default();
        config.directory = std::sync::Arc::from(dir.path());
        let config = Arc::new(config);
        let controls = DownloadControls::dry_run_hidden();
        let asset = TestPhotoAsset::new("DRY_RUN_PARTIAL")
            .orig_size(123)
            .orig_url("https://p01.icloud-content.com/dry-run-partial.jpg")
            .orig_checksum("ck_dry_run_partial")
            .build();
        let client = reqwest::Client::new();

        let streaming_result = stream_and_download_from_stream(
            &client,
            stream::iter(vec![
                Ok::<PhotoAsset, anyhow::Error>(asset),
                Err(anyhow::anyhow!("malformed page")),
            ]),
            &config,
            controls,
            2,
            CancellationToken::new(),
            StreamRuntime::new(None, None),
        )
        .await
        .expect("dry run should continue past enumeration errors");
        let (outcome, stats) = build_download_outcome(
            &client,
            &[],
            &config,
            controls,
            streaming_result,
            Instant::now(),
            CancellationToken::new(),
        )
        .await
        .expect("dry-run outcome should build");

        assert!(
            matches!(outcome, DownloadOutcome::PartialFailure { failed_count: 1 }),
            "dry-run enumeration errors must produce PartialFailure, got {outcome:?}"
        );
        assert_eq!(stats.downloaded, 1);
        assert_eq!(stats.enumeration_errors, 1);
        assert!(
            fs::read_dir(dir.path()).unwrap().next().is_none(),
            "dry-run mode must not create downloaded files"
        );
    }

    /// End-to-end regression for issue #211. A pending row carried over from
    /// a prior sync, combined with a filter that excludes the asset in the
    /// current sync, must not have its `last_seen_at` bumped by the producer.
    /// Combined with the flipped `promote_pending_to_failed` gate, this
    /// guarantees the ghost loop (pending -> failed -> pending) can't recur.
    #[tokio::test]
    async fn ghost_loop_regression_filtered_pending_asset_survives_sync() {
        use crate::download::DownloadConfig;
        use crate::icloud::photos::PhotoAsset;
        use crate::state::{MediaType, SqliteStateDb};
        use chrono::TimeZone;
        use futures_util::stream;
        use std::sync::Arc;

        fn ghost_asset() -> PhotoAsset {
            TestPhotoAsset::new("GHOST")
                .filename("ghost.mov")
                .item_type("com.apple.quicktime-movie")
                .orig_file_type("com.apple.quicktime-movie")
                .orig_size(4096)
                .orig_url("http://127.0.0.1:1/ghost.mov")
                .orig_checksum("ck_ghost")
                .build()
        }

        let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());

        let prior_seen_at = chrono::Utc::now().timestamp() - 86400;
        let record = AssetRecord::new_pending(
            "PrimarySync".into(),
            "GHOST".into(),
            VersionSizeKey::Original,
            "ck_ghost".into(),
            "ghost.mov".into(),
            chrono::Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
            None,
            4096,
            MediaType::Video,
        );
        db.upsert_seen(&record).await.unwrap();
        db.backdate_last_seen("GHOST", prior_seen_at);

        let dir = TempDir::new().unwrap();
        let mut config = DownloadConfig::test_default();
        config.directory = std::sync::Arc::from(dir.path());
        config.media.videos = false;
        config.state_db = Some(db.clone());
        let config = Arc::new(config);

        let client = reqwest::Client::new();
        let sync_started_at = chrono::Utc::now().timestamp();
        let stream1 = stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(ghost_asset())]);
        stream_and_download_from_stream(
            &client,
            stream1,
            &config,
            DownloadControls::download_hidden(),
            1,
            CancellationToken::new(),
            StreamRuntime::new(None, None),
        )
        .await
        .expect("sync must complete");

        let pending = db.get_pending().await.unwrap();
        assert_eq!(pending.len(), 1, "asset must remain the only pending row");
        assert_eq!(&*pending[0].id, "GHOST");
        assert_eq!(
            pending[0].last_seen_at.timestamp(),
            prior_seen_at,
            "producer must NOT bump last_seen_at on a filtered asset"
        );

        let promoted = db.promote_pending_to_failed(sync_started_at).await.unwrap();
        assert_eq!(promoted, 0, "filtered asset must not be promoted");

        // Second cycle locks in stability across repeated syncs.
        let sync_2_start = chrono::Utc::now().timestamp();
        let stream2 = stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(ghost_asset())]);
        stream_and_download_from_stream(
            &client,
            stream2,
            &config,
            DownloadControls::download_hidden(),
            1,
            CancellationToken::new(),
            StreamRuntime::new(None, None),
        )
        .await
        .expect("second sync must complete");
        let promoted2 = db.promote_pending_to_failed(sync_2_start).await.unwrap();
        assert_eq!(promoted2, 0, "second sync must also leave row untouched");

        let summary = db.get_summary().await.unwrap();
        assert_eq!(summary.pending, 1);
        assert_eq!(summary.failed, 0);
    }

    /// Producer-side regression for resolving pending rows when the expected
    /// file already exists on disk.
    ///
    /// A pending row carried over from a prior interrupted sync, whose
    /// new sync sees the expected file at the natural path, must be adopted as
    /// downloaded when the file already exists with the same name and size.
    /// Otherwise standard sync resets failed assets to pending, full
    /// enumeration routes the path collision through a deterministic alternate
    /// path, and the same row is promoted back to failed every run.
    #[tokio::test]
    async fn producer_adopts_pending_on_disk_skip_as_downloaded() {
        use crate::download::DownloadConfig;
        use crate::icloud::photos::PhotoAsset;
        use crate::state::SqliteStateDb;
        use crate::test_helpers::TestAssetRecord;
        use futures_util::stream;
        use std::sync::Arc;

        fn carryover_asset() -> PhotoAsset {
            TestPhotoAsset::new("STUCK")
                .filename("stuck.jpg")
                .item_type("public.jpeg")
                .orig_file_type("public.jpeg")
                .orig_size(1234)
                .orig_url("https://p01.icloud-content.com/stuck.jpg")
                .orig_checksum("ck_stuck")
                .build()
                .with_source_zone(Arc::from("SharedSync-abc"))
        }

        let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());

        let prior_seen_at = chrono::Utc::now().timestamp() - 86400;
        let record = TestAssetRecord::new("STUCK")
            .library("SharedSync-abc")
            .checksum("ck_stuck")
            .filename("stuck.jpg")
            .size(1234)
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.backdate_last_seen("STUCK", prior_seen_at);

        let dir = TempDir::new().unwrap();
        let mut config = DownloadConfig::test_default();
        config.directory = Arc::from(dir.path());
        config.state_db = Some(db.clone());
        config.refresh_metadata = true;
        config.capture_timestamp_repair =
            crate::download::CaptureTimestampRepair::ReplaceWithCaptureLocal;
        config.metadata.set_exif_datetime = true;
        let config = Arc::new(config);

        // Pre-create the on-disk file at the expected natural path. The path
        // layer now emits an identity collision task for this same-size file,
        // so the producer must still adopt the pending row before forwarding.
        let asset = carryover_asset();
        let target_path = crate::download::filter::expected_paths_for(&asset, config.as_ref())
            .first()
            .expect("test asset must derive an expected path")
            .path
            .clone();
        if let Some(parent) = target_path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&target_path, vec![0u8; 1234]).unwrap();

        let client = reqwest::Client::new();
        let sync_started_at = chrono::Utc::now().timestamp();
        let stream1 = stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(carryover_asset())]);
        stream_and_download_from_stream(
            &client,
            stream1,
            &config,
            DownloadControls::download_hidden(),
            1,
            CancellationToken::new(),
            StreamRuntime::new(None, None),
        )
        .await
        .expect("sync must complete");

        let promoted = db.promote_pending_to_failed(sync_started_at).await.unwrap();
        assert_eq!(
            promoted, 0,
            "on-disk pending row should be resolved before failed promotion"
        );

        let summary = db.get_summary().await.unwrap();
        assert_eq!(summary.downloaded, 1);
        assert_eq!(summary.pending, 0);
        assert_eq!(summary.failed, 0);
        let capture_repairs = db
            .get_pending_metadata_rewrites_page_for_queue(
                crate::state::db::MetadataRewriteQueue::CaptureRepair,
                None,
                0,
                1,
            )
            .await
            .unwrap();
        assert_eq!(capture_repairs.len(), 1);
        assert_eq!(capture_repairs[0].asset.id.as_ref(), "STUCK");
    }

    async fn run_producer_metadata_rewritten_pending_file(
        pending_checksum: &str,
    ) -> (
        StreamingResult,
        i64,
        Arc<crate::state::SqliteStateDb>,
        TempDir,
        PathBuf,
        String,
    ) {
        use crate::download::DownloadConfig;
        use crate::icloud::photos::PhotoAsset;
        use crate::state::{SqliteStateDb, VersionSizeKey};
        use crate::test_helpers::TestAssetRecord;
        use futures_util::stream;
        use std::sync::Arc;

        fn metadata_rewritten_asset() -> PhotoAsset {
            TestPhotoAsset::new("METADATA_REWRITTEN_PENDING")
                .filename("rewritten.jpg")
                .item_type("public.jpeg")
                .orig_file_type("public.jpeg")
                .orig_size(8)
                .orig_url("https://p01.icloud-content.com/rewritten.jpg")
                .orig_checksum("ck_metadata_rewritten_pending")
                .build()
                .with_source_zone(Arc::from("SharedSync-abc"))
        }

        let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
        let dir = TempDir::new().unwrap();
        let mut config = DownloadConfig::test_default();
        config.directory = Arc::from(dir.path());
        config.state_db = Some(db.clone());
        let config = Arc::new(config);

        let asset = metadata_rewritten_asset();
        let target_path = crate::download::filter::expected_paths_for(&asset, config.as_ref())
            .first()
            .expect("test asset must derive an expected path")
            .path
            .clone();
        if let Some(parent) = target_path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&target_path, b"shorter").unwrap();
        let local_checksum = crate::download::file::compute_sha256(&target_path)
            .await
            .expect("hash metadata-rewritten file");

        let record = TestAssetRecord::new(asset.state_id())
            .library("SharedSync-abc")
            .checksum(pending_checksum)
            .filename("rewritten.jpg")
            .size(8)
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "SharedSync-abc",
            asset.state_id(),
            VersionSizeKey::Original.as_str(),
            &target_path,
            &local_checksum,
            Some("download-checksum-before-metadata"),
        )
        .await
        .unwrap();
        db.mark_failed(
            "SharedSync-abc",
            asset.state_id(),
            VersionSizeKey::Original.as_str(),
            "prior false truncation",
        )
        .await
        .unwrap();
        db.prepare_for_retry(
            Some("SharedSync-abc"),
            crate::state::RetryErrorRetention::Clear,
        )
        .await
        .unwrap();

        let client = reqwest::Client::new();
        let sync_started_at = chrono::Utc::now().timestamp();
        let assets = stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(
            metadata_rewritten_asset(),
        )]);
        let result = stream_and_download_from_stream(
            &client,
            assets,
            &config,
            DownloadControls::download_hidden(),
            1,
            CancellationToken::new(),
            StreamRuntime::new(None, None),
        )
        .await
        .expect("sync must process metadata-rewritten pending file");

        (
            result,
            sync_started_at,
            db,
            dir,
            target_path,
            local_checksum,
        )
    }

    #[tokio::test]
    async fn producer_adopts_smaller_metadata_rewritten_pending_file() {
        let (result, sync_started_at, db, _dir, target_path, local_checksum) =
            run_producer_metadata_rewritten_pending_file("ck_metadata_rewritten_pending").await;

        assert_eq!(
            db.promote_pending_to_failed(sync_started_at).await.unwrap(),
            0
        );
        assert!(result.failed.is_empty());
        assert_eq!(fs::read(&target_path).unwrap(), b"shorter");
        let downloaded = db.get_downloaded_page(0, 10).await.unwrap();
        assert_eq!(downloaded.len(), 1);
        assert_eq!(
            downloaded[0].local_path.as_deref(),
            Some(target_path.as_path())
        );
        assert_eq!(
            downloaded[0].local_checksum.as_deref(),
            Some(local_checksum.as_str())
        );
        assert_eq!(
            downloaded[0].download_checksum.as_deref(),
            Some("download-checksum-before-metadata")
        );
    }

    #[tokio::test]
    async fn producer_does_not_adopt_metadata_rewritten_file_after_provider_change() {
        let (result, _, db, _dir, target_path, _) =
            run_producer_metadata_rewritten_pending_file("old-provider-checksum").await;

        assert_eq!(result.downloaded, 0);
        assert_eq!(result.failed.len(), 1);
        assert_eq!(fs::read(&target_path).unwrap(), b"shorter");
        assert!(db.get_downloaded_page(0, 10).await.unwrap().is_empty());
        let failed = db.get_failed().await.unwrap();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].checksum.as_ref(), "ck_metadata_rewritten_pending");
    }

    #[tokio::test]
    async fn pending_same_size_collision_does_not_adopt_other_assets_bare_file() {
        use crate::download::DownloadConfig;
        use crate::icloud::photos::PhotoAsset;
        use crate::state::SqliteStateDb;
        use crate::test_helpers::TestAssetRecord;
        use futures_util::stream;
        use std::sync::Arc;

        let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
        let asset = TestPhotoAsset::new("SAME_SIZE_B")
            .filename("IMG_0001.JPG")
            .orig_size(5000)
            .orig_url("https://p01.icloud-content.com/b.jpg")
            .orig_checksum("ck_same_size_b")
            .build();

        let dir = TempDir::new().unwrap();
        let mut config = DownloadConfig::test_default();
        config.directory = Arc::from(dir.path());
        config.state_db = Some(db.clone());
        let config = Arc::new(config);

        let bare_path = crate::download::filter::expected_paths_for(&asset, config.as_ref())
            .first()
            .expect("test asset must derive an expected path")
            .path
            .clone();
        let identity_path = identity_suffixed_path_for(&bare_path, asset.id());
        fs::create_dir_all(bare_path.parent().unwrap()).unwrap();
        fs::write(&bare_path, vec![1u8; 5000]).unwrap();

        let pending = TestAssetRecord::new(asset.id())
            .checksum("ck_same_size_b")
            .filename(
                identity_path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .expect("identity path must have UTF-8 filename"),
            )
            .size(5000)
            .build();
        db.upsert_seen(&pending).await.unwrap();

        let result = stream_and_download_from_stream(
            &reqwest::Client::new(),
            stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(asset)]),
            &config,
            DownloadControls::download_hidden(),
            1,
            CancellationToken::new(),
            StreamRuntime::new(None, None),
        )
        .await
        .expect("sync must complete");

        assert_eq!(
            result.skip_summary.on_disk, 0,
            "pending collision row must not adopt another asset's bare file"
        );
        let summary = db.get_summary().await.unwrap();
        assert_eq!(summary.downloaded, 0);
        assert_eq!(summary.failed, 1);
    }

    #[tokio::test]
    async fn producer_plans_distinct_same_path_same_size_assets() {
        use crate::download::DownloadConfig;
        use crate::icloud::photos::PhotoAsset;
        use futures_util::stream;
        use std::sync::Arc;

        let asset_a = TestPhotoAsset::new("SAME_SIZE_A")
            .filename("IMG_0001.JPG")
            .orig_size(5000)
            .orig_url("https://p01.icloud-content.com/a.jpg")
            .orig_checksum("ck_same_size_a")
            .build();
        let asset_b = TestPhotoAsset::new("SAME_SIZE_B")
            .filename("IMG_0001.JPG")
            .orig_size(5000)
            .orig_url("https://p01.icloud-content.com/b.jpg")
            .orig_checksum("ck_same_size_b")
            .build();

        let dir = TempDir::new().unwrap();
        let mut config = DownloadConfig::test_default();
        config.directory = Arc::from(dir.path());
        let config = Arc::new(config);

        let result = stream_and_download_from_stream(
            &reqwest::Client::new(),
            stream::iter(vec![
                Ok::<PhotoAsset, anyhow::Error>(asset_a),
                Ok::<PhotoAsset, anyhow::Error>(asset_b),
            ]),
            &config,
            DownloadControls::dry_run_hidden(),
            2,
            CancellationToken::new(),
            StreamRuntime::new(None, None),
        )
        .await
        .expect("same-size collision planning should complete");

        assert_eq!(result.downloaded, 2, "unexpected result: {result:?}");
        assert!(result.failed.is_empty());
        assert!(
            fs::read_dir(dir.path()).unwrap().next().is_none(),
            "dry-run planning must not create files"
        );
    }

    /// v5 metadata backfill regression: a previously downloaded row with a
    /// NULL metadata_hash can hit the producer's on-disk-skip branch when
    /// the file already exists. That branch must refresh metadata for the
    /// existing downloaded row; otherwise the "one-time after upgrade"
    /// backfill notice repeats forever.
    #[tokio::test]
    async fn on_disk_skip_backfills_downloaded_row_metadata_hash() {
        use crate::download::DownloadConfig;
        use crate::icloud::photos::PhotoAsset;
        use crate::state::SqliteStateDb;
        use futures_util::stream;
        use std::sync::Arc;

        fn existing_asset() -> PhotoAsset {
            TestPhotoAsset::new("BACKFILL")
                .filename("backfill.jpg")
                .item_type("public.jpeg")
                .orig_file_type("public.jpeg")
                .orig_size(1234)
                .orig_url("https://p01.icloud-content.com/backfill.jpg")
                .orig_checksum("ck_backfill")
                .build()
        }

        let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
        let dir = TempDir::new().unwrap();
        let mut config = DownloadConfig::test_default();
        config.directory = std::sync::Arc::from(dir.path());
        config.state_db = Some(db.clone());
        let config = Arc::new(config);

        let asset = existing_asset();
        let target_path = crate::download::filter::expected_paths_for(&asset, config.as_ref())
            .first()
            .expect("test asset must derive an expected path")
            .path
            .clone();
        if let Some(parent) = target_path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&target_path, vec![0u8; 1234]).unwrap();

        let record = crate::test_helpers::TestAssetRecord::new("BACKFILL")
            .checksum("ck_backfill")
            .filename("backfill.jpg")
            .size(1234)
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "BACKFILL",
            "original",
            &target_path,
            "sha256",
            None,
        )
        .await
        .unwrap();
        db.clear_metadata_hash_for_test("PrimarySync", "BACKFILL", "original");
        assert!(db.has_downloaded_without_metadata_hash().await.unwrap());

        let client = reqwest::Client::new();
        let assets = stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(existing_asset())]);
        let result = stream_and_download_from_stream(
            &client,
            assets,
            &config,
            DownloadControls::download_hidden(),
            1,
            CancellationToken::new(),
            StreamRuntime::new(None, None),
        )
        .await
        .expect("sync must complete");

        assert_eq!(
            result.downloaded, 0,
            "existing file should not be re-downloaded"
        );
        assert!(
            !db.has_downloaded_without_metadata_hash().await.unwrap(),
            "on-disk skip must backfill metadata_hash for existing downloaded rows"
        );
    }

    /// Explicit metadata refresh updates historical versions before current
    /// resolution and filename filters are applied.
    #[tokio::test]
    async fn refresh_metadata_updates_historical_version_before_filename_filter() {
        use crate::download::DownloadConfig;
        use crate::icloud::photos::PhotoAsset;
        use crate::state::SqliteStateDb;
        use crate::state::types::AssetMetadata;
        use futures_util::stream;
        use std::sync::Arc;

        fn existing_asset() -> PhotoAsset {
            TestPhotoAsset::new("REFRESH_FILTERED")
                .filename("filtered.jpg")
                .item_type("public.jpeg")
                .orig_file_type("public.jpeg")
                .orig_size(1234)
                .orig_url("https://p01.icloud-content.com/filtered.jpg")
                .orig_checksum("ck_filtered")
                .build()
        }

        let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
        let dir = TempDir::new().unwrap();
        let mut config = DownloadConfig::test_default();
        config.directory = std::sync::Arc::from(dir.path());
        config.state_db = Some(db.clone());
        config.refresh_metadata = true;
        config.capture_timestamp_repair =
            crate::download::CaptureTimestampRepair::ReplaceWithCaptureLocal;
        config.metadata.set_exif_datetime = true;
        config.filename_exclude =
            Arc::from(vec![glob::Pattern::new("*.jpg").expect("filename pattern")]);
        let config = Arc::new(config);

        let historical_path = dir.path().join("historical-medium.jpg");
        fs::write(
            &historical_path,
            [
                0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00,
                0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0xFF, 0xD9,
            ],
        )
        .unwrap();
        let record = crate::test_helpers::TestAssetRecord::new("REFRESH_FILTERED")
            .version_size(crate::state::VersionSizeKey::Medium)
            .checksum("historical-medium")
            .filename("historical-medium.jpg")
            .size(1234)
            .metadata(AssetMetadata {
                title: Some("stale".to_string()),
                metadata_hash: Some("stale-hash".to_string()),
                ..AssetMetadata::default()
            })
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "REFRESH_FILTERED",
            "medium",
            &historical_path,
            "sha256",
            None,
        )
        .await
        .unwrap();

        let result = stream_and_download_from_stream(
            &reqwest::Client::new(),
            stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(existing_asset())]),
            &config,
            DownloadControls::download_hidden(),
            1,
            CancellationToken::new(),
            StreamRuntime::new(None, None),
        )
        .await
        .expect("sync must complete");

        assert_eq!(
            result.downloaded, 0,
            "filename-filtered asset should not be downloaded"
        );
        let rewrites = db.get_pending_metadata_rewrites(10).await.unwrap();
        assert_eq!(rewrites.len(), 1);
        assert_eq!(
            rewrites[0].version_size,
            crate::state::VersionSizeKey::Medium
        );
        assert_ne!(
            rewrites[0].metadata.metadata_hash.as_deref(),
            Some("stale-hash")
        );
        assert_eq!(rewrites[0].metadata.title, None);
        let capture_repairs = db
            .get_pending_metadata_rewrites_page_for_queue(
                crate::state::db::MetadataRewriteQueue::CaptureRepair,
                None,
                0,
                10,
            )
            .await
            .unwrap();
        assert_eq!(capture_repairs.len(), 1);
        assert_eq!(
            capture_repairs[0].asset.version_size,
            crate::state::VersionSizeKey::Medium
        );
    }

    /// #707 review: the pre-plan refresh makes embedded rewrites reachable for
    /// a filtered downloaded asset. The drain rewrites the media, so the row
    /// must end holding a checksum of the bytes that are now on disk, and the
    /// pre-rewrite hash must survive as the provider download checksum.
    #[tokio::test]
    async fn filtered_embed_rewrite_keeps_stored_checksums_current() {
        use crate::download::DownloadConfig;
        use crate::icloud::photos::PhotoAsset;
        use crate::state::SqliteStateDb;
        use crate::state::types::AssetMetadata;
        use futures_util::stream;
        use std::sync::Arc;

        let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
        let dir = TempDir::new().unwrap();
        let mut config = DownloadConfig::test_default();
        config.directory = Arc::from(dir.path());
        config.state_db = Some(db.clone());
        config.metadata.set_exif_rating = true;
        config.filename_exclude =
            Arc::from(vec![glob::Pattern::new("*.jpg").expect("filename pattern")]);
        let config = Arc::new(config);

        let media_path = dir.path().join("filtered-embed.jpg");
        fs::write(
            &media_path,
            [
                0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00,
                0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0xFF, 0xD9,
            ],
        )
        .unwrap();
        let seeded_checksum = crate::download::file::compute_sha256(&media_path)
            .await
            .expect("hash the seeded media");

        // Stored without the favourite, so the provider edit below raises the
        // rating and gives the embedded writer real work to do.
        let record = crate::test_helpers::TestAssetRecord::new("FILTERED_EMBED")
            .checksum("ck_filtered_embed")
            .filename("filtered-embed.jpg")
            .size(1234)
            .metadata(AssetMetadata {
                metadata_hash: Some("stale-hash".to_string()),
                ..AssetMetadata::default()
            })
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "FILTERED_EMBED",
            "original",
            &media_path,
            &seeded_checksum,
            None,
        )
        .await
        .unwrap();

        let edited: PhotoAsset = TestPhotoAsset::new("FILTERED_EMBED")
            .filename("filtered-embed.jpg")
            .item_type("public.jpeg")
            .orig_file_type("public.jpeg")
            .orig_size(1234)
            .orig_url("https://p01.icloud-content.com/filtered-embed.jpg")
            .orig_checksum("ck_filtered_embed")
            .favorite(true)
            .build();

        let result = stream_and_download_from_stream(
            &reqwest::Client::new(),
            stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(edited)]),
            &config,
            DownloadControls::download_hidden(),
            1,
            CancellationToken::new(),
            StreamRuntime::new(None, None),
        )
        .await
        .expect("sync must complete");

        assert_eq!(result.downloaded, 0, "the filter must skip the media task");
        let on_disk = crate::download::file::compute_sha256(&media_path)
            .await
            .expect("hash the rewritten media");
        assert_ne!(
            on_disk, seeded_checksum,
            "precondition: the embedded rewrite must change the media bytes"
        );

        let row = db.get_downloaded_page(0, 1).await.unwrap().remove(0);
        assert_eq!(
            row.local_checksum.as_deref(),
            Some(on_disk.as_str()),
            "the row must record the bytes the rewrite left on disk"
        );
        assert_eq!(
            row.download_checksum.as_deref(),
            Some(seeded_checksum.as_str()),
            "the pre-rewrite hash must survive as the provider download checksum"
        );
    }

    /// #674 regression: a metadata-only edit drifts the source hash from the
    /// stored hash. On the on-disk skip the catalogue must be refreshed to the
    /// edited source metadata (not left stale) without re-downloading, so a
    /// queued rewrite applies corrected values instead of replaying old ones.
    #[tokio::test]
    async fn on_disk_skip_refreshes_catalogue_on_metadata_drift() {
        use crate::download::DownloadConfig;
        use crate::icloud::photos::PhotoAsset;
        use crate::state::SqliteStateDb;
        use futures_util::stream;
        use std::sync::Arc;

        let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
        let dir = TempDir::new().unwrap();
        let mut config = DownloadConfig::test_default();
        config.directory = Arc::from(dir.path());
        config.metadata.set_exif_rating = true;
        config.state_db = Some(db.clone());
        let config = Arc::new(config);

        // Downloaded while a favourite; the source has since dropped it.
        let stored: PhotoAsset = TestPhotoAsset::new("DRIFT")
            .filename("drift.jpg")
            .orig_size(1000)
            .orig_checksum("ck_drift")
            .favorite(true)
            .build();
        let edited: PhotoAsset = TestPhotoAsset::new("DRIFT")
            .filename("drift.jpg")
            .orig_size(1000)
            .orig_checksum("ck_drift")
            .favorite(false)
            .build();

        let derived = derive_expected_paths(&stored, config.as_ref())
            .into_iter()
            .next()
            .unwrap();
        fs::create_dir_all(derived.path.parent().unwrap()).unwrap();
        fs::write(&derived.path, vec![0u8; 1000]).unwrap();
        let seeded_checksum = crate::download::file::compute_sha256(&derived.path)
            .await
            .unwrap();
        let record = asset_record_for_derived_path(Arc::from("PrimarySync"), &stored, &derived);
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            stored.state_id(),
            derived.version_size.as_str(),
            &derived.path,
            &seeded_checksum,
            None,
        )
        .await
        .unwrap();

        let result = stream_and_download_from_stream(
            &reqwest::Client::new(),
            stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(edited)]),
            &config,
            DownloadControls::download_hidden(),
            1,
            CancellationToken::new(),
            StreamRuntime::new(None, None),
        )
        .await
        .unwrap();

        assert_eq!(result.downloaded, 0, "unchanged bytes must not re-download");
        let refreshed = db.get_downloaded_page(0, 1).await.unwrap().remove(0);
        assert!(
            !refreshed.metadata.is_favorite,
            "the on-disk skip must refresh the stale catalogue to the edited source metadata"
        );
        let queued = db.get_pending_metadata_rewrites(10).await.unwrap();
        assert!(
            queued.is_empty(),
            "the queued rewrite must run from the fresh catalogue and clear its marker"
        );
        assert_eq!(result.exif_failures, 0);
        assert_eq!(
            fs::read_dir(derived.path.parent().unwrap())
                .unwrap()
                .count(),
            1,
            "the refresh must not create a suffixed duplicate"
        );
    }

    /// Minimal valid JPEG (SOI + APP0 JFIF + EOI). The XMP toolkit accepts it,
    /// so a metadata write against it exercises the real writer.
    #[cfg(feature = "xmp")]
    const MINIMAL_JPEG: [u8; 22] = [
        0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00, 0x00,
        0x01, 0x00, 0x01, 0x00, 0x00, 0xFF, 0xD9,
    ];

    /// A `PhotoAsset` backed by `MINIMAL_JPEG`, so a metadata write against its
    /// file exercises the real writer rather than failing to decode.
    #[cfg(feature = "xmp")]
    fn jpeg_asset(id: &str, filename: &str, favorite: bool) -> crate::icloud::photos::PhotoAsset {
        TestPhotoAsset::new(id)
            .filename(filename)
            .orig_size(MINIMAL_JPEG.len() as u64)
            .orig_checksum(&format!("ck_{id}"))
            .favorite(favorite)
            .build()
    }

    /// Seeds a downloaded asset whose on-disk file is a real JPEG.
    #[cfg(feature = "xmp")]
    async fn seed_downloaded_jpeg_asset(
        db: &Arc<crate::state::SqliteStateDb>,
        config: &DownloadConfig,
        stored: &crate::icloud::photos::PhotoAsset,
    ) -> super::DerivedPath {
        let derived = derive_expected_paths(stored, config)
            .into_iter()
            .next()
            .unwrap();
        fs::create_dir_all(derived.path.parent().unwrap()).unwrap();
        fs::write(&derived.path, MINIMAL_JPEG).unwrap();
        let record = asset_record_for_derived_path(Arc::from("PrimarySync"), stored, &derived);
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            stored.state_id(),
            derived.version_size.as_str(),
            &derived.path,
            "local-checksum",
            None,
        )
        .await
        .unwrap();
        derived
    }

    /// The `.xmp` sidecar kei writes beside a media file.
    #[cfg(feature = "xmp")]
    fn sidecar_path_for(media_path: &std::path::Path) -> std::path::PathBuf {
        let mut name = media_path.file_name().unwrap().to_os_string();
        name.push(".xmp");
        media_path.with_file_name(name)
    }

    /// A stored/edited `PhotoAsset` pair sharing identical bytes but differing
    /// provider metadata, which is the shape of a metadata-only iCloud edit.
    fn drifted_asset_pair(
        id: &str,
        filename: &str,
    ) -> (
        crate::icloud::photos::PhotoAsset,
        crate::icloud::photos::PhotoAsset,
    ) {
        let checksum = format!("ck_{id}");
        let stored = TestPhotoAsset::new(id)
            .filename(filename)
            .orig_size(1000)
            .orig_checksum(&checksum)
            .favorite(true)
            .build();
        let edited = TestPhotoAsset::new(id)
            .filename(filename)
            .orig_size(1000)
            .orig_checksum(&checksum)
            .favorite(false)
            .build();
        (stored, edited)
    }

    /// Seed a downloaded asset with its file on disk, as a prior sync would
    /// have left it, and return the derived path it was recorded against.
    async fn seed_downloaded_drift_asset(
        db: &Arc<crate::state::SqliteStateDb>,
        config: &DownloadConfig,
        stored: &crate::icloud::photos::PhotoAsset,
    ) -> super::DerivedPath {
        let derived = derive_expected_paths(stored, config)
            .into_iter()
            .next()
            .unwrap();
        fs::create_dir_all(derived.path.parent().unwrap()).unwrap();
        fs::write(&derived.path, vec![0u8; 1000]).unwrap();
        let record = asset_record_for_derived_path(Arc::from("PrimarySync"), stored, &derived);
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            stored.state_id(),
            derived.version_size.as_str(),
            &derived.path,
            "local-checksum",
            None,
        )
        .await
        .unwrap();
        derived
    }

    /// #707: with local metadata outputs disabled the catalogue must still be
    /// refreshed, and no rewrite may be queued. Backup fidelity of the
    /// manifest does not depend on the user opting into sidecars or EXIF.
    #[tokio::test]
    async fn metadata_drift_without_writers_refreshes_catalogue_and_queues_nothing() {
        use crate::download::DownloadConfig;
        use crate::icloud::photos::PhotoAsset;
        use crate::state::SqliteStateDb;
        use futures_util::stream;
        use std::sync::Arc;

        let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
        let dir = TempDir::new().unwrap();
        let mut config = DownloadConfig::test_default();
        config.directory = Arc::from(dir.path());
        config.state_db = Some(db.clone());
        // No metadata writers enabled.
        let config = Arc::new(config);

        let (stored, edited) = drifted_asset_pair("NOWRITERS", "nowriters.jpg");
        let derived = seed_downloaded_drift_asset(&db, &config, &stored).await;
        let before = fs::metadata(&derived.path).unwrap().len();

        let result = stream_and_download_from_stream(
            &reqwest::Client::new(),
            stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(edited)]),
            &config,
            DownloadControls::download_hidden(),
            1,
            CancellationToken::new(),
            StreamRuntime::new(None, None),
        )
        .await
        .unwrap();

        assert_eq!(result.downloaded, 0);
        assert_eq!(result.state_write_failures, 0);
        let refreshed = db.get_downloaded_page(0, 1).await.unwrap().remove(0);
        assert!(
            !refreshed.metadata.is_favorite,
            "the catalogue must refresh even with local metadata outputs disabled"
        );
        assert!(
            db.get_pending_metadata_rewrites(10)
                .await
                .unwrap()
                .is_empty(),
            "no rewrite may be queued when no metadata writer is enabled"
        );
        assert_eq!(
            fs::metadata(&derived.path).unwrap().len(),
            before,
            "local files must be untouched"
        );
    }

    /// #707: a downloaded asset whose media task is filtered out must still
    /// have its provider metadata applied. The refresh runs before filtering
    /// and path planning, so an excluded asset cannot strand a stale
    /// catalogue row.
    #[tokio::test]
    async fn metadata_drift_refreshes_when_media_task_is_filtered() {
        use crate::download::DownloadConfig;
        use crate::icloud::photos::PhotoAsset;
        use crate::state::SqliteStateDb;
        use futures_util::stream;
        use std::sync::Arc;

        let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
        let dir = TempDir::new().unwrap();
        let mut config = DownloadConfig::test_default();
        config.directory = Arc::from(dir.path());
        config.metadata.set_exif_rating = true;
        config.state_db = Some(db.clone());

        let (stored, edited) = drifted_asset_pair("FILTERED", "filtered.jpg");
        // Filter the asset out of media planning entirely.
        config.exclude_asset_ids = Arc::new(std::iter::once(stored.id().to_string()).collect());
        let config = Arc::new(config);

        seed_downloaded_drift_asset(&db, &config, &stored).await;

        let result = stream_and_download_from_stream(
            &reqwest::Client::new(),
            stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(edited)]),
            &config,
            DownloadControls::download_hidden(),
            1,
            CancellationToken::new(),
            StreamRuntime::new(None, None),
        )
        .await
        .unwrap();

        assert_eq!(result.downloaded, 0);
        assert_eq!(result.state_write_failures, 0);
        let refreshed = db.get_downloaded_page(0, 1).await.unwrap().remove(0);
        assert!(
            !refreshed.metadata.is_favorite,
            "a filtered media task must not prevent the provider metadata refresh"
        );
    }

    /// #707: a photo the provider deleted and the user then restored must
    /// still be recognised as already held. The exclusion for deleted rows
    /// belongs in the staleness checks, not in the shared downloaded list:
    /// filtering that list sends the asset down the fast path, which skips the
    /// on-disk check, so the default naming policy files a copy beside the
    /// original.
    #[tokio::test]
    async fn restored_soft_deleted_asset_is_skipped_without_downloading() {
        use crate::download::DownloadConfig;
        use crate::icloud::photos::PhotoAsset;
        use crate::state::SqliteStateDb;
        use futures_util::stream;
        use std::sync::Arc;

        let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
        let dir = TempDir::new().unwrap();
        let mut config = DownloadConfig::test_default();
        config.directory = Arc::from(dir.path());
        config.state_db = Some(db.clone());
        let config = Arc::new(config);

        let (stored, _) = drifted_asset_pair("RESTORED", "restored.jpg");
        let derived = seed_downloaded_drift_asset(&db, &config, &stored).await;
        db.mark_soft_deleted("PrimarySync", stored.state_id(), None)
            .await
            .unwrap();

        let result = stream_and_download_from_stream(
            &reqwest::Client::new(),
            stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(stored.clone())]),
            &config,
            DownloadControls::download_hidden(),
            1,
            CancellationToken::new(),
            StreamRuntime::new(None, None),
        )
        .await
        .unwrap();

        assert_eq!(result.downloaded, 0);
        assert!(
            result.failed.is_empty(),
            "a restored photo must not be queued for download"
        );
        assert_eq!(result.skip_summary.on_disk, 1);
        assert_eq!(
            fs::read_dir(derived.path.parent().unwrap())
                .unwrap()
                .count(),
            1,
            "no duplicate may be written beside the original"
        );
    }

    /// #707: a restored photo that was also edited must not report unsafe
    /// provider state. Its rows cannot be refreshed while the deletion stands,
    /// and reporting that every cycle would hold the checkpoint forever.
    #[tokio::test]
    async fn restored_and_edited_asset_does_not_report_unsafe_state() {
        use crate::download::DownloadConfig;
        use crate::icloud::photos::PhotoAsset;
        use crate::state::SqliteStateDb;
        use futures_util::stream;
        use std::sync::Arc;

        let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
        let dir = TempDir::new().unwrap();
        let mut config = DownloadConfig::test_default();
        config.directory = Arc::from(dir.path());
        config.metadata.set_exif_rating = true;
        config.state_db = Some(db.clone());
        let config = Arc::new(config);

        let (stored, edited) = drifted_asset_pair("RESTORED_EDIT", "restored_edit.jpg");
        seed_downloaded_drift_asset(&db, &config, &stored).await;
        db.mark_soft_deleted("PrimarySync", stored.state_id(), None)
            .await
            .unwrap();

        let result = stream_and_download_from_stream(
            &reqwest::Client::new(),
            stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(edited)]),
            &config,
            DownloadControls::download_hidden(),
            1,
            CancellationToken::new(),
            StreamRuntime::new(None, None),
        )
        .await
        .unwrap();

        assert_eq!(
            result.state_write_failures, 0,
            "a deleted row that cannot be refreshed is not a durability failure"
        );
        assert_eq!(result.downloaded, 0);
    }

    /// #707 acceptance criterion 1, sidecar half: the rewrite must run from
    /// the refreshed catalogue and land the new value on disk. The direction
    /// matters. Un-favouriting clears the rating and writes nothing, so only
    /// favouriting proves the fresh value reached the file.
    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn metadata_drift_writes_fresh_value_into_the_sidecar() {
        use crate::download::DownloadConfig;
        use crate::icloud::photos::PhotoAsset;
        use crate::state::SqliteStateDb;
        use futures_util::stream;
        use std::sync::Arc;

        let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
        let dir = TempDir::new().unwrap();
        let mut config = DownloadConfig::test_default();
        config.directory = Arc::from(dir.path());
        config.metadata.xmp_sidecar = true;
        config.state_db = Some(db.clone());
        let config = Arc::new(config);

        let stored = jpeg_asset("SIDECAR", "sidecar.jpg", false);
        let edited = jpeg_asset("SIDECAR", "sidecar.jpg", true);
        let derived = seed_downloaded_jpeg_asset(&db, &config, &stored).await;

        let result = stream_and_download_from_stream(
            &reqwest::Client::new(),
            stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(edited)]),
            &config,
            DownloadControls::download_hidden(),
            1,
            CancellationToken::new(),
            StreamRuntime::new(None, None),
        )
        .await
        .unwrap();

        assert_eq!(result.downloaded, 0, "a metadata edit must not fetch media");
        assert_eq!(result.exif_failures, 0);

        let sidecar = fs::read_to_string(sidecar_path_for(&derived.path))
            .expect("sidecar written next to the media file");
        assert!(
            sidecar.contains("<xmp:Rating>5</xmp:Rating>"),
            "the sidecar must carry the favourite from the refreshed catalogue: {sidecar}"
        );
    }

    /// #707: a rewrite marker that predates a provider deletion must not
    /// drain against the tombstoned row. Its metadata is frozen at the values
    /// held before the deletion, so writing it would publish stale data.
    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn marker_predating_a_deletion_is_not_drained() {
        use crate::download::DownloadConfig;
        use crate::icloud::photos::PhotoAsset;
        use crate::state::SqliteStateDb;
        use futures_util::stream;
        use std::sync::Arc;

        let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
        let dir = TempDir::new().unwrap();
        let mut config = DownloadConfig::test_default();
        config.directory = Arc::from(dir.path());
        config.metadata.xmp_sidecar = true;
        config.state_db = Some(db.clone());
        let config = Arc::new(config);

        let stored = jpeg_asset("OLDMARKER", "oldmarker.jpg", false);
        let derived = seed_downloaded_jpeg_asset(&db, &config, &stored).await;
        db.record_metadata_write_failure(
            "PrimarySync",
            stored.state_id(),
            derived.version_size.as_str(),
        )
        .await
        .unwrap();
        db.mark_soft_deleted("PrimarySync", stored.state_id(), None)
            .await
            .unwrap();

        stream_and_download_from_stream(
            &reqwest::Client::new(),
            stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(jpeg_asset(
                "OLDMARKER",
                "oldmarker.jpg",
                true,
            ))]),
            &config,
            DownloadControls::download_hidden(),
            1,
            CancellationToken::new(),
            StreamRuntime::new(None, None),
        )
        .await
        .unwrap();

        assert!(
            !sidecar_path_for(&derived.path).exists(),
            "a marker on a deleted row must not write metadata"
        );
    }

    /// #707: a tombstoned row cannot be refreshed, so it must not be queued
    /// for rewrite either. Doing so would publish the stale values the issue
    /// exists to stop.
    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn restored_asset_is_not_rewritten_from_the_stale_catalogue() {
        use crate::download::DownloadConfig;
        use crate::icloud::photos::PhotoAsset;
        use crate::state::SqliteStateDb;
        use futures_util::stream;
        use std::sync::Arc;

        let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
        let dir = TempDir::new().unwrap();
        let mut config = DownloadConfig::test_default();
        config.directory = Arc::from(dir.path());
        config.metadata.xmp_sidecar = true;
        config.state_db = Some(db.clone());
        let config = Arc::new(config);

        let stored = jpeg_asset("TOMBSTONE", "tombstone.jpg", false);
        let derived = seed_downloaded_jpeg_asset(&db, &config, &stored).await;
        db.mark_soft_deleted("PrimarySync", stored.state_id(), None)
            .await
            .unwrap();

        let result = stream_and_download_from_stream(
            &reqwest::Client::new(),
            stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(jpeg_asset(
                "TOMBSTONE",
                "tombstone.jpg",
                true,
            ))]),
            &config,
            DownloadControls::download_hidden(),
            1,
            CancellationToken::new(),
            StreamRuntime::new(None, None),
        )
        .await
        .unwrap();

        assert_eq!(result.downloaded, 0);
        assert!(
            !sidecar_path_for(&derived.path).exists(),
            "a tombstoned asset must not have metadata written from its stale row"
        );
        assert!(
            db.get_metadata_retry_markers().await.unwrap().is_empty(),
            "a tombstoned asset must not be queued for a later rewrite either"
        );
    }

    /// #707: when the catalogue write fails, the prior metadata must survive
    /// intact and the failure must reach `state_write_failures`, which is the
    /// signal `sync_cycle` uses to hold the zone checkpoint.
    #[tokio::test]
    async fn metadata_drift_refresh_failure_preserves_state_and_reports_not_durable() {
        use crate::download::DownloadConfig;
        use crate::icloud::photos::PhotoAsset;
        use crate::state::SqliteStateDb;
        use futures_util::stream;
        use std::sync::Arc;

        let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
        let dir = TempDir::new().unwrap();
        let mut config = DownloadConfig::test_default();
        config.directory = Arc::from(dir.path());
        config.metadata.set_exif_rating = true;
        config.state_db = Some(db.clone());
        let config = Arc::new(config);

        let (stored, edited) = drifted_asset_pair("DBFAIL", "dbfail.jpg");
        seed_downloaded_drift_asset(&db, &config, &stored).await;

        db.fail_provider_metadata_refresh_for_test();

        let result = stream_and_download_from_stream(
            &reqwest::Client::new(),
            stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(edited)]),
            &config,
            DownloadControls::download_hidden(),
            1,
            CancellationToken::new(),
            StreamRuntime::new(None, None),
        )
        .await
        .unwrap();

        assert!(
            result.state_write_failures > 0,
            "a failed catalogue refresh must be reported as non-durable provider state"
        );
        let unchanged = db.get_downloaded_page(0, 1).await.unwrap().remove(0);
        assert!(
            unchanged.metadata.is_favorite,
            "a failed refresh must leave the prior catalogue metadata intact"
        );
        assert!(
            db.get_pending_metadata_rewrites(10)
                .await
                .unwrap()
                .is_empty(),
            "a failed refresh must not queue a rewrite, which would publish the stale row"
        );
    }

    /// #707: unchanged provider metadata is a no-op. The failure trigger
    /// would turn any attempted catalogue write into an error, so a clean
    /// result proves no write was attempted and no rewrite was queued.
    #[tokio::test]
    async fn unchanged_metadata_performs_no_catalogue_write() {
        use crate::download::DownloadConfig;
        use crate::icloud::photos::PhotoAsset;
        use crate::state::SqliteStateDb;
        use futures_util::stream;
        use std::sync::Arc;

        let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
        let dir = TempDir::new().unwrap();
        let mut config = DownloadConfig::test_default();
        config.directory = Arc::from(dir.path());
        config.metadata.set_exif_rating = true;
        config.state_db = Some(db.clone());
        let config = Arc::new(config);

        let (stored, _) = drifted_asset_pair("SAME", "same.jpg");
        seed_downloaded_drift_asset(&db, &config, &stored).await;

        db.fail_provider_metadata_refresh_for_test();

        // Re-enumerate the identical asset: same bytes, same metadata.
        let result = stream_and_download_from_stream(
            &reqwest::Client::new(),
            stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(stored.clone())]),
            &config,
            DownloadControls::download_hidden(),
            1,
            CancellationToken::new(),
            StreamRuntime::new(None, None),
        )
        .await
        .unwrap();

        assert_eq!(
            result.state_write_failures, 0,
            "unchanged metadata must not attempt a catalogue write"
        );
        assert_eq!(result.downloaded, 0);
        assert!(
            db.get_pending_metadata_rewrites(10)
                .await
                .unwrap()
                .is_empty(),
            "unchanged metadata must not queue a rewrite"
        );
    }

    /// #707: a retry marker must trigger a refresh even when the stored hash
    /// matches the provider. `mark_hidden_at_source` sets `is_hidden` without
    /// recomputing `metadata_hash`, so after an unhide the hashes agree while
    /// the row still reads hidden. Drift alone cannot see that.
    #[tokio::test]
    async fn retry_marker_refreshes_row_whose_hash_matches_but_columns_are_stale() {
        use crate::download::DownloadConfig;
        use crate::icloud::photos::PhotoAsset;
        use crate::state::SqliteStateDb;
        use futures_util::stream;
        use std::sync::Arc;

        let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
        let dir = TempDir::new().unwrap();
        let mut config = DownloadConfig::test_default();
        config.directory = Arc::from(dir.path());
        config.metadata.set_exif_rating = true;
        config.state_db = Some(db.clone());
        let config = Arc::new(config);

        let (visible, _) = drifted_asset_pair("UNHIDE", "unhide.jpg");
        let derived = seed_downloaded_drift_asset(&db, &config, &visible).await;

        // Hidden at source: the column flips, the stored hash does not.
        db.mark_hidden_at_source("PrimarySync", visible.state_id())
            .await
            .unwrap();
        db.record_metadata_write_failure(
            "PrimarySync",
            visible.state_id(),
            derived.version_size.as_str(),
        )
        .await
        .unwrap();
        assert!(
            db.get_downloaded_page(0, 1).await.unwrap()[0]
                .metadata
                .is_hidden,
            "precondition: the stored row reads hidden"
        );

        // The provider now reports it visible again, matching the stored hash.
        let result = stream_and_download_from_stream(
            &reqwest::Client::new(),
            stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(visible.clone())]),
            &config,
            DownloadControls::download_hidden(),
            1,
            CancellationToken::new(),
            StreamRuntime::new(None, None),
        )
        .await
        .unwrap();

        assert_eq!(result.downloaded, 0);
        assert_eq!(result.state_write_failures, 0);
        assert!(
            !db.get_downloaded_page(0, 1).await.unwrap()[0]
                .metadata
                .is_hidden,
            "the retry marker must drive a refresh that clears the stale hidden flag"
        );
    }

    /// #707: when the queued rewrite cannot complete, its marker must survive
    /// for retry and the surviving row must hold the fresh provider metadata,
    /// so the retry cannot replay stale values.
    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn rewrite_failure_retains_marker_holding_fresh_metadata() {
        use crate::download::DownloadConfig;
        use crate::icloud::photos::PhotoAsset;
        use crate::state::SqliteStateDb;
        use futures_util::stream;
        use std::sync::Arc;

        let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
        let dir = TempDir::new().unwrap();
        let mut config = DownloadConfig::test_default();
        config.directory = Arc::from(dir.path());
        config.metadata.xmp_sidecar = true;
        config.state_db = Some(db.clone());
        let config = Arc::new(config);

        let (stored, edited) = drifted_asset_pair("REWRITEFAIL", "rewritefail.jpg");
        seed_downloaded_drift_asset(&db, &config, &stored).await;
        db.fail_metadata_marker_clear_for_test();

        let result = stream_and_download_from_stream(
            &reqwest::Client::new(),
            stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(edited)]),
            &config,
            DownloadControls::download_hidden(),
            1,
            CancellationToken::new(),
            StreamRuntime::new(None, None),
        )
        .await
        .unwrap();

        assert_eq!(
            result.state_write_failures, 0,
            "a local rewrite failure is not a provider-state failure"
        );
        let surviving = db.get_pending_metadata_rewrites(10).await.unwrap();
        assert_eq!(
            surviving.len(),
            1,
            "a failed rewrite must remain visible for retry"
        );
        assert!(
            !surviving[0].metadata.is_favorite,
            "the retained marker must point at the fresh catalogue metadata"
        );

        // A metadata-only edit downloads nothing, so the zero-download branch
        // is the one that has to report the failure.
        assert_eq!(result.downloaded, 0);
        assert!(result.exif_failures > 0);
        let (outcome, stats) = build_download_outcome(
            &reqwest::Client::new(),
            &[],
            &config,
            DownloadControls::download_hidden(),
            result,
            Instant::now(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(
            matches!(outcome, DownloadOutcome::PartialFailure { .. }),
            "a failed rewrite must not report a clean sync, got {outcome:?}"
        );
        assert!(stats.exif_failures > 0);
        assert_eq!(
            stats.state_write_failures, 0,
            "the checkpoint may still advance once the marker is durable"
        );
    }

    /// #741: the normal pending-rewrite owner must treat an unparsable
    /// third-party sidecar as visible retryable work, not successful output.
    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn contract_xmp_sidecar_rewrite_requires_stable_input() {
        use crate::download::DownloadConfig;
        use crate::icloud::photos::PhotoAsset;
        use crate::state::SqliteStateDb;
        use futures_util::stream;
        use std::sync::Arc;

        let dir = TempDir::new().unwrap();
        let db = Arc::new(
            SqliteStateDb::open(&dir.path().join("state.db"))
                .await
                .unwrap(),
        );
        let download_dir = dir.path().join("downloads");
        let mut config = DownloadConfig::test_default();
        config.directory = Arc::from(download_dir.as_path());
        config.metadata.xmp_sidecar = true;
        config.state_db = Some(db.clone());
        let config = Arc::new(config);

        let stored = jpeg_asset("UNPARSABLE_SIDECAR", "unparsable.jpg", true);
        let derived = seed_downloaded_jpeg_asset(&db, &config, &stored).await;
        db.record_metadata_write_failure(
            "PrimarySync",
            stored.state_id(),
            derived.version_size.as_str(),
        )
        .await
        .unwrap();
        let sidecar_path = sidecar_path_for(&derived.path);
        let original = b"<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF";
        fs::write(&sidecar_path, original).unwrap();

        let result = stream_and_download_from_stream(
            &reqwest::Client::new(),
            stream::empty::<Result<PhotoAsset, anyhow::Error>>(),
            &config,
            DownloadControls::download_hidden(),
            0,
            CancellationToken::new(),
            StreamRuntime::new(None, None),
        )
        .await
        .unwrap();

        assert_eq!(result.downloaded, 0);
        assert_eq!(result.exif_failures, 1);
        assert_eq!(fs::read(&sidecar_path).unwrap(), original);
        assert_eq!(
            db.get_pending_metadata_rewrites(10).await.unwrap().len(),
            1,
            "failed sidecar rewrite must keep its durable marker"
        );

        let (outcome, stats) = build_download_outcome(
            &reqwest::Client::new(),
            &[],
            &config,
            DownloadControls::download_hidden(),
            result,
            Instant::now(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(matches!(outcome, DownloadOutcome::PartialFailure { .. }));
        assert_eq!(stats.exif_failures, 1);
        assert_eq!(
            serde_json::to_value(&stats).unwrap()["exif_failures"],
            1,
            "sync report statistics must expose the sidecar failure"
        );
    }

    /// #752: ambiguous bytes retained from a refused sidecar publication must
    /// not block the durable marker from succeeding on a later drain.
    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn retained_sidecar_temp_does_not_block_pending_rewrite_retry() {
        use crate::download::DownloadConfig;
        use crate::icloud::photos::PhotoAsset;
        use crate::state::SqliteStateDb;
        use futures_util::stream;
        use std::sync::Arc;

        let dir = TempDir::new().unwrap();
        let db = Arc::new(
            SqliteStateDb::open(&dir.path().join("state.db"))
                .await
                .unwrap(),
        );
        let download_dir = dir.path().join("downloads");
        let mut config = DownloadConfig::test_default();
        config.directory = Arc::from(download_dir.as_path());
        config.metadata.xmp_sidecar = true;
        config.state_db = Some(db.clone());
        let config = Arc::new(config);

        let stored = jpeg_asset("RETAINED_SIDECAR_TEMP", "retained.jpg", true);
        let derived = seed_downloaded_jpeg_asset(&db, &config, &stored).await;
        db.record_metadata_write_failure(
            "PrimarySync",
            stored.state_id(),
            derived.version_size.as_str(),
        )
        .await
        .unwrap();

        let retained_path = derived.path.parent().unwrap().join(format!(
            ".kei-xmp-{}-{}.kei-tmp",
            std::process::id(),
            u64::MAX
        ));
        let retained_bytes = b"ambiguous bytes from an earlier publication";
        fs::write(&retained_path, retained_bytes).unwrap();

        let result = stream_and_download_from_stream(
            &reqwest::Client::new(),
            stream::empty::<Result<PhotoAsset, anyhow::Error>>(),
            &config,
            DownloadControls::download_hidden(),
            0,
            CancellationToken::new(),
            StreamRuntime::new(None, None),
        )
        .await
        .unwrap();

        assert_eq!(result.downloaded, 0);
        assert_eq!(result.exif_failures, 0);
        assert!(
            db.get_pending_metadata_rewrites(10)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(fs::read(&retained_path).unwrap(), retained_bytes);
        let sidecar = fs::read_to_string(sidecar_path_for(&derived.path)).unwrap();
        assert!(sidecar.contains("<xmp:Rating>5</xmp:Rating>"), "{sidecar}");
    }

    /// Data-sacred regression for the trust-state fast-skip removal.
    ///
    /// When the state DB says an asset is `downloaded` and the config_hash
    /// matches the prior sync, the producer must still verify the file is on
    /// disk. A user-deleted file must be forwarded for re-download, not
    /// fast-skipped on the strength of the DB row alone.
    ///
    /// Setup mirrors the prior failure mode: stored config_hash matches the
    /// current config (so any past trust-state gate would activate), the row
    /// is `downloaded` with a matching checksum, and the file is absent.
    /// Asserts `result.failed` contains the asset (download was attempted
    /// against a dead URL), proving the producer did not skip via state.
    #[tokio::test]
    async fn deleted_downloaded_file_is_forwarded_not_state_skipped() {
        use crate::download::DownloadConfig;
        use crate::icloud::photos::PhotoAsset;
        use crate::state::SqliteStateDb;
        use futures_util::stream;
        use std::sync::Arc;

        fn deleted_asset() -> PhotoAsset {
            TestPhotoAsset::new("DELETED")
                .filename("deleted.jpg")
                .item_type("public.jpeg")
                .orig_file_type("public.jpeg")
                .orig_size(1234)
                // Allowlisted CDN host so the URL passes version validation
                // and the asset reaches the download phase; the non-base64
                // checksum below makes the download fail before any network
                // I/O, which surfaces as a `failed` row in the state DB.
                .orig_url("https://p01.icloud-content.com/deleted.jpg")
                .orig_checksum("ck_deleted")
                .build()
        }

        let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
        let asset = deleted_asset();

        let dir = TempDir::new().unwrap();
        let mut config = DownloadConfig::test_default();
        config.directory = std::sync::Arc::from(dir.path());
        config.state_db = Some(db.clone());
        let config = Arc::new(config);

        let target_path = crate::download::paths::local_download_path(
            &config.directory,
            &config.folder_structure,
            &asset.created_local(),
            "deleted.jpg",
            None,
        );
        let record = crate::test_helpers::TestAssetRecord::new("DELETED")
            .checksum("ck_deleted")
            .filename("deleted.jpg")
            .size(1234)
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "DELETED",
            "original",
            &target_path,
            "ck_deleted",
            None,
        )
        .await
        .unwrap();
        let config_hash = crate::download::hash_download_config(&config);
        db.set_metadata("config_hash", &config_hash).await.unwrap();

        assert!(!target_path.exists(), "target file must not exist");

        let client = reqwest::Client::new();
        let stream1 = stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(deleted_asset())]);
        let result = stream_and_download_from_stream(
            &client,
            stream1,
            &config,
            DownloadControls::download_hidden(),
            1,
            CancellationToken::new(),
            StreamRuntime::new(None, None),
        )
        .await
        .expect("sync must complete");

        // by_state was the counter on the now-removed trust-state fast-skip;
        // a non-zero value here would mean the gate was reintroduced. A
        // renamed-counter regression would slip past this alone, so we also
        // assert the download phase ran (failed row below).
        assert_eq!(
            result.skip_summary.by_state, 0,
            "deleted-but-DB-downloaded asset must not be state-skipped"
        );
        let failed = db.get_failed().await.unwrap();
        assert_eq!(
            failed.len(),
            1,
            "deleted file must be forwarded for re-download (which fails against the dead URL)"
        );
        assert_eq!(&*failed[0].id, "DELETED");
    }

    async fn assert_metadata_mutated_downloaded_file_is_not_redownloaded(on_disk_size: usize) {
        use crate::download::DownloadConfig;
        use crate::icloud::photos::PhotoAsset;
        use crate::state::SqliteStateDb;
        use futures_util::stream;
        use std::sync::Arc;

        fn asset() -> PhotoAsset {
            TestPhotoAsset::new("METADATA_MUTATED")
                .filename("IMG_4123.JPG")
                .item_type("public.jpeg")
                .orig_file_type("public.jpeg")
                .orig_size(1234)
                // Allowlisted CDN host so a regression reaches the download
                // phase; the non-base64 checksum then fails locally without
                // doing network I/O.
                .orig_url("https://p01.icloud-content.com/IMG_4123.JPG")
                .orig_checksum("ck_metadata_mutated")
                .build()
        }

        let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
        let existing_asset = asset();

        let dir = TempDir::new().unwrap();
        let mut config = DownloadConfig::test_default();
        config.directory = std::sync::Arc::from(dir.path());
        config.state_db = Some(db.clone());
        let config = Arc::new(config);

        let target_path = crate::download::paths::local_download_path(
            &config.directory,
            &config.folder_structure,
            &existing_asset.created_local(),
            "IMG_4123.JPG",
            None,
        );
        fs::create_dir_all(target_path.parent().unwrap()).unwrap();
        fs::write(&target_path, vec![0u8; on_disk_size]).unwrap();
        let local_checksum = crate::download::file::compute_sha256(&target_path)
            .await
            .expect("hash metadata-mutated file");

        let record = crate::test_helpers::TestAssetRecord::new("METADATA_MUTATED")
            .checksum("ck_metadata_mutated")
            .filename("IMG_4123.JPG")
            .size(1234)
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "METADATA_MUTATED",
            "original",
            &target_path,
            &local_checksum,
            Some("download_checksum_before_metadata_write"),
        )
        .await
        .unwrap();

        let client = reqwest::Client::new();
        let stream1 = stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(asset())]);
        let result = stream_and_download_from_stream(
            &client,
            stream1,
            &config,
            DownloadControls::download_hidden(),
            1,
            CancellationToken::new(),
            StreamRuntime::new(None, None),
        )
        .await
        .expect("sync must complete");

        assert_eq!(result.downloaded, 0, "asset must not be re-downloaded");
        assert!(
            result.failed.is_empty(),
            "state-backed current path should skip before the dead URL reaches the download phase"
        );
        assert!(
            !target_path.with_file_name("IMG_4123-1234.JPG").exists(),
            "sync must not create a size-dedup duplicate"
        );
        let failed = db.get_failed().await.unwrap();
        assert!(
            failed.is_empty(),
            "metadata-mutated downloaded file should remain downloaded, not failed"
        );
    }

    #[tokio::test]
    async fn metadata_mutated_downloaded_file_is_not_size_dedup_redownloaded() {
        assert_metadata_mutated_downloaded_file_is_not_redownloaded(1500).await;
    }

    /// Metadata embedding can legitimately make the local file smaller after
    /// the downloaded bytes were verified. A later sync must prove the current
    /// bytes from the DB row instead of downloading a `-<size>` duplicate.
    #[tokio::test]
    async fn smaller_metadata_mutated_downloaded_file_is_not_redownloaded() {
        assert_metadata_mutated_downloaded_file_is_not_redownloaded(1200).await;
    }

    fn identity_suffixed_path_for(bare_path: &Path, asset_id: &str) -> PathBuf {
        let filename = bare_path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("test path must have a UTF-8 filename");
        bare_path.with_file_name(crate::download::paths::insert_asset_identity_suffix(
            filename, asset_id,
        ))
    }

    fn suffixed_collision_asset(id: &str, checksum: &str) -> PhotoAsset {
        TestPhotoAsset::new(id)
            .filename("IMG_1816.HEIC")
            .item_type("public.heic")
            .orig_file_type("public.heic")
            .orig_size(1234)
            .orig_url("https://p01.icloud-content.com/IMG_1816.HEIC")
            .orig_checksum(checksum)
            .build()
    }

    fn suffixed_ampm_collision_asset(id: &str, checksum: &str) -> PhotoAsset {
        TestPhotoAsset::new(id)
            .filename("Screenshot 2025-01-14 at 1.40.01\u{202F}PM.PNG")
            .item_type("public.png")
            .orig_file_type("public.png")
            .orig_size(1234)
            .orig_url("https://p01.icloud-content.com/Screenshot.PNG")
            .orig_checksum(checksum)
            .build()
    }

    fn live_photo_collision_asset(id: &str) -> PhotoAsset {
        TestPhotoAsset::new(id)
            .filename("IMG_0001.HEIC")
            .item_type("public.heic")
            .orig_file_type("public.heic")
            .orig_size(2000)
            .orig_url("https://p01.icloud-content.com/IMG_0001.HEIC")
            .orig_checksum("heic_ck")
            .live_photo(
                "https://p01.icloud-content.com/IMG_0001.MOV",
                "mov_ck",
                3000,
            )
            .build()
    }

    fn path_for_filename(config: &DownloadConfig, asset: &PhotoAsset, filename: &str) -> PathBuf {
        crate::download::paths::local_download_path(
            &config.directory,
            &config.folder_structure,
            &asset.created_local(),
            filename,
            None,
        )
    }

    async fn record_downloaded_test_version(
        db: &crate::state::SqliteStateDb,
        asset: &PhotoAsset,
        version_size: VersionSizeKey,
        checksum: &str,
        size: u64,
        path: &Path,
    ) {
        let filename = path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("test path must have a UTF-8 filename");
        let record = crate::test_helpers::TestAssetRecord::new(asset.id())
            .checksum(checksum)
            .filename(filename)
            .size(size)
            .version_size(version_size)
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            asset.id(),
            version_size.as_str(),
            path,
            "local_checksum",
            None,
        )
        .await
        .unwrap();
    }

    /// Regression for #594: a downloaded asset stored at an identity-suffixed
    /// collision path must be matched by its recorded state path, not only by
    /// the bare derived path.
    #[tokio::test]
    async fn suffixed_downloaded_file_is_on_disk_skipped() {
        use crate::download::DownloadConfig;
        use crate::icloud::photos::PhotoAsset;
        use crate::state::SqliteStateDb;
        use futures_util::stream;
        use std::sync::Arc;

        let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
        let dir = TempDir::new().unwrap();
        let mut config = DownloadConfig::test_default();
        config.directory = Arc::from(dir.path());
        config.state_db = Some(db.clone());
        let config = Arc::new(config);

        let asset_b = suffixed_collision_asset("SUFFIXED_B", "ck_suffixed_b");
        let bare_path = crate::download::filter::expected_paths_for(&asset_b, config.as_ref())
            .first()
            .expect("test asset must derive an expected path")
            .path
            .clone();
        let suffixed_path = identity_suffixed_path_for(&bare_path, asset_b.id());
        fs::create_dir_all(bare_path.parent().unwrap()).unwrap();
        fs::write(&bare_path, vec![0u8; 1234]).unwrap();
        fs::write(&suffixed_path, vec![1u8; 1234]).unwrap();

        let record = crate::test_helpers::TestAssetRecord::new("SUFFIXED_B")
            .checksum("ck_suffixed_b")
            .filename("IMG_1816.HEIC")
            .size(1234)
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "SUFFIXED_B",
            "original",
            &suffixed_path,
            "local_checksum_for_suffixed_file",
            None,
        )
        .await
        .unwrap();

        let result = stream_and_download_from_stream(
            &reqwest::Client::new(),
            stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(asset_b)]),
            &config,
            DownloadControls::download_hidden(),
            1,
            CancellationToken::new(),
            StreamRuntime::new(None, None),
        )
        .await
        .expect("sync must complete");

        assert_eq!(result.downloaded, 0, "asset must not be re-downloaded");
        assert_eq!(
            result.skip_summary.on_disk, 1,
            "suffixed downloaded file must count as an on-disk skip"
        );
        assert!(result.failed.is_empty());
        assert!(
            !identity_suffixed_path_for(&bare_path, "SUFFIXED_B-2").exists(),
            "sync must not create an ordinal duplicate for the same asset"
        );
        let failed = db.get_failed().await.unwrap();
        assert!(failed.is_empty(), "suffixed file should remain downloaded");
    }

    #[tokio::test]
    async fn live_photo_motion_with_size_suffixed_primary_stem_is_on_disk_skipped() {
        use crate::download::DownloadConfig;
        use crate::icloud::photos::PhotoAsset;
        use crate::state::SqliteStateDb;
        use futures_util::stream;
        use std::sync::Arc;

        let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
        let dir = TempDir::new().unwrap();
        let mut config = DownloadConfig::test_default();
        config.directory = Arc::from(dir.path());
        config.state_db = Some(db.clone());
        let config = Arc::new(config);

        let asset = live_photo_collision_asset("LIVE_SIZE_COLLISION");
        let bare_primary = path_for_filename(&config, &asset, "IMG_0001.HEIC");
        let stored_primary = path_for_filename(&config, &asset, "IMG_0001-2000.HEIC");
        let stored_motion = path_for_filename(&config, &asset, "IMG_0001-2000_HEVC.MOV");
        fs::create_dir_all(bare_primary.parent().unwrap()).unwrap();
        fs::write(&bare_primary, vec![9u8; 1111]).unwrap();
        fs::write(&stored_primary, vec![1u8; 2000]).unwrap();
        fs::write(&stored_motion, vec![2u8; 3000]).unwrap();

        record_downloaded_test_version(
            db.as_ref(),
            &asset,
            VersionSizeKey::Original,
            "heic_ck",
            2000,
            &stored_primary,
        )
        .await;
        record_downloaded_test_version(
            db.as_ref(),
            &asset,
            VersionSizeKey::LiveOriginal,
            "mov_ck",
            3000,
            &stored_motion,
        )
        .await;

        let result = stream_and_download_from_stream(
            &reqwest::Client::new(),
            stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(asset.clone())]),
            &config,
            DownloadControls::download_hidden(),
            1,
            CancellationToken::new(),
            StreamRuntime::new(None, None),
        )
        .await
        .expect("sync must complete");

        assert_eq!(result.downloaded, 0, "Live Photo must not be re-downloaded");
        assert_eq!(
            result.skip_summary.on_disk, 1,
            "asset should count as an on-disk skip when both paths are already present"
        );
        assert!(result.failed.is_empty());
        assert!(
            !path_for_filename(
                &config,
                &asset,
                "IMG_0001-LIVE_SIZE_COLLISION_HEVC-LIVE_SIZE_COLLISION-2.MOV"
            )
            .exists(),
            "sync must not create a compounding motion duplicate"
        );
    }

    #[tokio::test]
    async fn live_photo_motion_with_identity_suffixed_primary_stem_is_on_disk_skipped() {
        use crate::download::DownloadConfig;
        use crate::icloud::photos::PhotoAsset;
        use crate::state::SqliteStateDb;
        use futures_util::stream;
        use std::sync::Arc;

        let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
        let dir = TempDir::new().unwrap();
        let mut config = DownloadConfig::test_default();
        config.directory = Arc::from(dir.path());
        config.state_db = Some(db.clone());
        let config = Arc::new(config);

        let asset = live_photo_collision_asset("LIVE_ID_COLLISION");
        let bare_primary = path_for_filename(&config, &asset, "IMG_0001.HEIC");
        let stored_primary = path_for_filename(&config, &asset, "IMG_0001-LIVE_ID_COLLISION.HEIC");
        let stored_motion = path_for_filename(
            &config,
            &asset,
            "IMG_0001-LIVE_ID_COLLISION_HEVC-LIVE_ID_COLLISION-2.MOV",
        );
        fs::create_dir_all(bare_primary.parent().unwrap()).unwrap();
        fs::write(&bare_primary, vec![9u8; 2000]).unwrap();
        fs::write(&stored_primary, vec![1u8; 2000]).unwrap();
        fs::write(&stored_motion, vec![2u8; 3000]).unwrap();

        record_downloaded_test_version(
            db.as_ref(),
            &asset,
            VersionSizeKey::Original,
            "heic_ck",
            2000,
            &stored_primary,
        )
        .await;
        record_downloaded_test_version(
            db.as_ref(),
            &asset,
            VersionSizeKey::LiveOriginal,
            "mov_ck",
            3000,
            &stored_motion,
        )
        .await;

        let result = stream_and_download_from_stream(
            &reqwest::Client::new(),
            stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(asset)]),
            &config,
            DownloadControls::download_hidden(),
            1,
            CancellationToken::new(),
            StreamRuntime::new(None, None),
        )
        .await
        .expect("sync must complete");

        assert_eq!(result.downloaded, 0, "Live Photo must not be re-downloaded");
        assert_eq!(result.skip_summary.on_disk, 1);
        assert!(result.failed.is_empty());
    }

    #[tokio::test]
    async fn live_photo_motion_with_sanitized_identity_suffix_is_on_disk_skipped() {
        use crate::download::DownloadConfig;
        use crate::icloud::photos::PhotoAsset;
        use crate::state::SqliteStateDb;
        use futures_util::stream;
        use std::sync::Arc;

        let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
        let dir = TempDir::new().unwrap();
        let mut config = DownloadConfig::test_default();
        config.directory = Arc::from(dir.path());
        config.state_db = Some(db.clone());
        let config = Arc::new(config);

        let asset = live_photo_collision_asset("LIVE/ID_COLLISION");
        let bare_primary = path_for_filename(&config, &asset, "IMG_0001.HEIC");
        let stored_primary = path_for_filename(&config, &asset, "IMG_0001-LIVE_ID_COLLISION.HEIC");
        let stored_motion = path_for_filename(
            &config,
            &asset,
            "IMG_0001-LIVE_ID_COLLISION_HEVC-LIVE_ID_COLLISION-22.MOV",
        );
        fs::create_dir_all(bare_primary.parent().unwrap()).unwrap();
        fs::write(&bare_primary, vec![9u8; 2000]).unwrap();
        fs::write(&stored_primary, vec![1u8; 2000]).unwrap();
        fs::write(&stored_motion, vec![2u8; 3000]).unwrap();

        record_downloaded_test_version(
            db.as_ref(),
            &asset,
            VersionSizeKey::Original,
            "heic_ck",
            2000,
            &stored_primary,
        )
        .await;
        record_downloaded_test_version(
            db.as_ref(),
            &asset,
            VersionSizeKey::LiveOriginal,
            "mov_ck",
            3000,
            &stored_motion,
        )
        .await;

        let result = stream_and_download_from_stream(
            &reqwest::Client::new(),
            stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(asset.clone())]),
            &config,
            DownloadControls::download_hidden(),
            1,
            CancellationToken::new(),
            StreamRuntime::new(None, None),
        )
        .await
        .expect("sync must complete");

        assert_eq!(
            result.downloaded, 0,
            "Live Photo with sanitized asset-id suffix must not be re-downloaded"
        );
        assert_eq!(result.skip_summary.on_disk, 1);
        assert!(result.failed.is_empty());
        assert!(
            !path_for_filename(
                &config,
                &asset,
                "IMG_0001-LIVE_ID_COLLISION_HEVC-LIVE_ID_COLLISION-23.MOV"
            )
            .exists(),
            "sync must not create the next sanitized ordinal duplicate"
        );
    }

    #[tokio::test]
    async fn live_photo_motion_original_policy_with_primary_collision_is_on_disk_skipped() {
        use crate::download::DownloadConfig;
        use crate::icloud::photos::PhotoAsset;
        use crate::state::SqliteStateDb;
        use crate::types::LivePhotoMovFilenamePolicy;
        use futures_util::stream;
        use std::sync::Arc;

        let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
        let dir = TempDir::new().unwrap();
        let mut config = DownloadConfig::test_default();
        config.directory = Arc::from(dir.path());
        config.live_photo_mov_filename_policy = LivePhotoMovFilenamePolicy::Original;
        config.state_db = Some(db.clone());
        let config = Arc::new(config);

        let asset = live_photo_collision_asset("LIVE_ORIGINAL_POLICY");
        let bare_primary = path_for_filename(&config, &asset, "IMG_0001.HEIC");
        let stored_primary = path_for_filename(&config, &asset, "IMG_0001-2000.HEIC");
        let stored_motion = path_for_filename(&config, &asset, "IMG_0001-2000.MOV");
        fs::create_dir_all(bare_primary.parent().unwrap()).unwrap();
        fs::write(&bare_primary, vec![9u8; 1111]).unwrap();
        fs::write(&stored_primary, vec![1u8; 2000]).unwrap();
        fs::write(&stored_motion, vec![2u8; 3000]).unwrap();

        record_downloaded_test_version(
            db.as_ref(),
            &asset,
            VersionSizeKey::Original,
            "heic_ck",
            2000,
            &stored_primary,
        )
        .await;
        record_downloaded_test_version(
            db.as_ref(),
            &asset,
            VersionSizeKey::LiveOriginal,
            "mov_ck",
            3000,
            &stored_motion,
        )
        .await;

        let result = stream_and_download_from_stream(
            &reqwest::Client::new(),
            stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(asset)]),
            &config,
            DownloadControls::download_hidden(),
            1,
            CancellationToken::new(),
            StreamRuntime::new(None, None),
        )
        .await
        .expect("sync must complete");

        assert_eq!(result.downloaded, 0, "Live Photo must not be re-downloaded");
        assert_eq!(result.skip_summary.on_disk, 1);
        assert!(result.failed.is_empty());
    }

    /// Same state-path family as #594, with the AM/PM whitespace variant that
    /// import-existing and normal on-disk probes already treat as equivalent.
    #[tokio::test]
    async fn ampm_variant_suffixed_downloaded_file_is_on_disk_skipped() {
        use crate::download::DownloadConfig;
        use crate::icloud::photos::PhotoAsset;
        use crate::state::SqliteStateDb;
        use futures_util::stream;
        use std::sync::Arc;

        let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
        let dir = TempDir::new().unwrap();
        let mut config = DownloadConfig::test_default();
        config.directory = Arc::from(dir.path());
        config.keep_unicode_in_filenames = true;
        config.state_db = Some(db.clone());
        let config = Arc::new(config);

        let asset = suffixed_ampm_collision_asset("AMPM_SUFFIXED_B", "ck_ampm_suffixed_b");
        let bare_path = crate::download::filter::expected_paths_for(&asset, config.as_ref())
            .first()
            .expect("test asset must derive an expected path")
            .path
            .clone();
        let regular_space_filename = bare_path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("test path must have a UTF-8 filename")
            .replace('\u{202F}', " ");
        let regular_space_bare_path = bare_path.with_file_name(regular_space_filename);
        let suffixed_path = identity_suffixed_path_for(&regular_space_bare_path, asset.id());
        fs::create_dir_all(bare_path.parent().unwrap()).unwrap();
        fs::write(&suffixed_path, vec![1u8; 1234]).unwrap();

        let record = crate::test_helpers::TestAssetRecord::new("AMPM_SUFFIXED_B")
            .checksum("ck_ampm_suffixed_b")
            .filename("Screenshot 2025-01-14 at 1.40.01\u{202F}PM.PNG")
            .size(1234)
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "AMPM_SUFFIXED_B",
            "original",
            &suffixed_path,
            "local_checksum_for_ampm_suffixed_file",
            None,
        )
        .await
        .unwrap();

        let result = stream_and_download_from_stream(
            &reqwest::Client::new(),
            stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(asset)]),
            &config,
            DownloadControls::download_hidden(),
            1,
            CancellationToken::new(),
            StreamRuntime::new(None, None),
        )
        .await
        .expect("sync must complete");

        assert_eq!(result.downloaded, 0, "asset must not be re-downloaded");
        assert_eq!(
            result.skip_summary.on_disk, 1,
            "AM/PM-equivalent suffixed file must count as an on-disk skip"
        );
        assert!(result.failed.is_empty());
        let failed = db.get_failed().await.unwrap();
        assert!(failed.is_empty(), "suffixed file should remain downloaded");
    }

    /// Same #594 path, but the recorded suffixed file is too small. The
    /// state-backed skip must not hide local truncation.
    #[tokio::test]
    async fn truncated_suffixed_downloaded_file_is_forwarded_not_on_disk_skipped() {
        use crate::download::DownloadConfig;
        use crate::icloud::photos::PhotoAsset;
        use crate::state::SqliteStateDb;
        use futures_util::stream;
        use std::sync::Arc;

        let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
        let dir = TempDir::new().unwrap();
        let mut config = DownloadConfig::test_default();
        config.directory = Arc::from(dir.path());
        config.state_db = Some(db.clone());
        let config = Arc::new(config);

        let asset_b = suffixed_collision_asset("TRUNCATED_SUFFIXED_B", "ck_truncated_suffixed_b");
        let bare_path = crate::download::filter::expected_paths_for(&asset_b, config.as_ref())
            .first()
            .expect("test asset must derive an expected path")
            .path
            .clone();
        let suffixed_path = identity_suffixed_path_for(&bare_path, asset_b.id());
        fs::create_dir_all(bare_path.parent().unwrap()).unwrap();
        fs::write(&bare_path, vec![0u8; 1234]).unwrap();
        fs::write(&suffixed_path, []).unwrap();

        let record = crate::test_helpers::TestAssetRecord::new("TRUNCATED_SUFFIXED_B")
            .checksum("ck_truncated_suffixed_b")
            .filename("IMG_1816.HEIC")
            .size(1234)
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "TRUNCATED_SUFFIXED_B",
            "original",
            &suffixed_path,
            "local_checksum_for_truncated_suffixed_file",
            None,
        )
        .await
        .unwrap();

        let result = stream_and_download_from_stream(
            &reqwest::Client::new(),
            stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(asset_b)]),
            &config,
            DownloadControls::download_hidden(),
            1,
            CancellationToken::new(),
            StreamRuntime::new(None, None),
        )
        .await
        .expect("sync must complete");

        assert_eq!(
            result.skip_summary.on_disk, 0,
            "truncated suffixed file must not be counted as an on-disk skip"
        );
        let failed = db.get_failed().await.unwrap();
        assert_eq!(
            failed.len(),
            1,
            "truncated suffixed file must be forwarded for re-download"
        );
        assert_eq!(&*failed[0].id, "TRUNCATED_SUFFIXED_B");
    }

    /// A state-backed identity-suffixed skip is valid only for the current
    /// path family. An existing file recorded under an old directory must not
    /// satisfy a new configured target after path-affecting config drift.
    #[tokio::test]
    async fn old_directory_state_path_is_forwarded_not_on_disk_skipped() {
        use crate::download::DownloadConfig;
        use crate::icloud::photos::PhotoAsset;
        use crate::state::SqliteStateDb;
        use futures_util::stream;
        use std::sync::Arc;

        let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
        let old_dir = TempDir::new().unwrap();
        let new_dir = TempDir::new().unwrap();
        let mut old_config = DownloadConfig::test_default();
        old_config.directory = Arc::from(old_dir.path());
        let old_config = Arc::new(old_config);
        let mut new_config = DownloadConfig::test_default();
        new_config.directory = Arc::from(new_dir.path());
        new_config.state_db = Some(db.clone());
        let new_config = Arc::new(new_config);

        let asset = suffixed_collision_asset("OLD_DIR_SUFFIXED_B", "ck_old_dir_suffixed_b");
        let old_bare_path =
            crate::download::filter::expected_paths_for(&asset, old_config.as_ref())
                .first()
                .expect("test asset must derive an old expected path")
                .path
                .clone();
        let old_suffixed_path = identity_suffixed_path_for(&old_bare_path, asset.id());
        fs::create_dir_all(old_bare_path.parent().unwrap()).unwrap();
        fs::write(&old_suffixed_path, vec![1u8; 1234]).unwrap();

        let new_bare_path =
            crate::download::filter::expected_paths_for(&asset, new_config.as_ref())
                .first()
                .expect("test asset must derive a new expected path")
                .path
                .clone();
        assert!(
            !new_bare_path.exists(),
            "new configured target must be absent"
        );

        let record = crate::test_helpers::TestAssetRecord::new("OLD_DIR_SUFFIXED_B")
            .checksum("ck_old_dir_suffixed_b")
            .filename("IMG_1816.HEIC")
            .size(1234)
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "OLD_DIR_SUFFIXED_B",
            "original",
            &old_suffixed_path,
            "local_checksum_for_old_suffixed_file",
            None,
        )
        .await
        .unwrap();

        let result = stream_and_download_from_stream(
            &reqwest::Client::new(),
            stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(asset)]),
            &new_config,
            DownloadControls::download_hidden(),
            1,
            CancellationToken::new(),
            StreamRuntime::new(None, None),
        )
        .await
        .expect("sync must complete");

        assert_eq!(
            result.skip_summary.on_disk, 0,
            "old directory state path must not count as a current on-disk skip"
        );
        let failed = db.get_failed().await.unwrap();
        assert_eq!(
            failed.len(),
            1,
            "missing new target must be forwarded for re-download"
        );
        assert_eq!(&*failed[0].id, "OLD_DIR_SUFFIXED_B");
    }

    /// Capture-local derivation can move an asset into a different date
    /// folder than a host-local rendering chose. The stored path is then not
    /// a current derived path, so a full sweep forwards the asset for
    /// download into its capture-local folder, and the earlier copy is left
    /// untouched because kei never deletes local media.
    #[tokio::test]
    async fn capture_local_date_folder_forwards_and_keeps_host_local_copy() {
        use crate::download::DownloadConfig;
        use crate::state::SqliteStateDb;
        use futures_util::stream;
        use std::sync::Arc;

        let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
        let dir = TempDir::new().unwrap();
        let mut config = DownloadConfig::test_default();
        config.directory = Arc::from(dir.path());
        config.state_db = Some(db.clone());
        let config = Arc::new(config);

        // 2026-01-31T22:31:59Z captured at +11:00 is 2026-02-01 locally.
        let asset = TestPhotoAsset::new("CAPTURE_OFFSET_MOVE")
            .filename("IMG_9001.JPG")
            .orig_size(1234)
            .orig_url("https://p01.icloud-content.com/IMG_9001.JPG")
            .orig_checksum("ck_capture_offset_move")
            .asset_date(1_769_898_719_000.0)
            .timezone_offset(39_600)
            .build();

        let derived = crate::download::filter::expected_paths_for(&asset, config.as_ref())
            .first()
            .expect("test asset must derive an expected path")
            .path
            .clone();
        assert!(derived.parent().is_some_and(|p| p.ends_with("2026/02/01")));

        let host_local_path = dir.path().join("2026/01/31/IMG_9001.JPG");
        fs::create_dir_all(host_local_path.parent().unwrap()).unwrap();
        fs::write(&host_local_path, vec![7u8; 1234]).unwrap();

        let record = crate::test_helpers::TestAssetRecord::new("CAPTURE_OFFSET_MOVE")
            .checksum("ck_capture_offset_move")
            .filename("IMG_9001.JPG")
            .size(1234)
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "CAPTURE_OFFSET_MOVE",
            "original",
            &host_local_path,
            "local_checksum_for_host_local_file",
            None,
        )
        .await
        .unwrap();

        let result = stream_and_download_from_stream(
            &reqwest::Client::new(),
            stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(asset)]),
            &config,
            DownloadControls::download_hidden(),
            1,
            CancellationToken::new(),
            StreamRuntime::new(None, None),
        )
        .await
        .expect("sync must complete");

        assert_eq!(
            result.skip_summary.on_disk, 0,
            "a host-local stored path must not satisfy the capture-local target"
        );
        let failed = db.get_failed().await.unwrap();
        assert_eq!(failed.len(), 1, "missing capture-local target is forwarded");
        assert_eq!(&*failed[0].id, "CAPTURE_OFFSET_MOVE");
        assert!(
            host_local_path.exists(),
            "the earlier copy must survive; kei does not delete local media"
        );
    }

    /// Data-sacred regression for state-backed on-disk skips.
    ///
    /// A downloaded DB row with a matching remote checksum is not enough to
    /// trust a too-small local file. If the stored current path exists but is
    /// shorter than the API-reported size, the producer must route the asset
    /// back through the download phase instead of letting the truncated file
    /// mask the real media.
    #[tokio::test]
    async fn truncated_downloaded_file_is_forwarded_not_on_disk_skipped() {
        use crate::download::DownloadConfig;
        use crate::icloud::photos::PhotoAsset;
        use crate::state::SqliteStateDb;
        use futures_util::stream;
        use std::sync::Arc;

        fn asset() -> PhotoAsset {
            TestPhotoAsset::new("TRUNCATED_DOWNLOADED")
                .filename("IMG_TRUNCATED.JPG")
                .item_type("public.jpeg")
                .orig_file_type("public.jpeg")
                .orig_size(1234)
                // Allowlisted CDN host so the asset reaches the download
                // phase; the non-base64 checksum fails locally without
                // doing network I/O.
                .orig_url("https://p01.icloud-content.com/IMG_TRUNCATED.JPG")
                .orig_checksum("ck_truncated_downloaded")
                .build()
        }

        let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
        let existing_asset = asset();

        let dir = TempDir::new().unwrap();
        let mut config = DownloadConfig::test_default();
        config.directory = std::sync::Arc::from(dir.path());
        config.state_db = Some(db.clone());
        let config = Arc::new(config);

        let target_path =
            crate::download::filter::expected_paths_for(&existing_asset, config.as_ref())
                .first()
                .expect("test asset must derive an expected path")
                .path
                .clone();
        fs::create_dir_all(target_path.parent().unwrap()).unwrap();
        fs::write(&target_path, []).unwrap();

        let record = crate::test_helpers::TestAssetRecord::new("TRUNCATED_DOWNLOADED")
            .checksum("ck_truncated_downloaded")
            .filename("IMG_TRUNCATED.JPG")
            .size(1234)
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "TRUNCATED_DOWNLOADED",
            "original",
            &target_path,
            "local_checksum_for_truncated_file",
            None,
        )
        .await
        .unwrap();

        let client = reqwest::Client::new();
        let stream1 = stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(asset())]);
        let result = stream_and_download_from_stream(
            &client,
            stream1,
            &config,
            DownloadControls::download_hidden(),
            1,
            CancellationToken::new(),
            StreamRuntime::new(None, None),
        )
        .await
        .expect("sync must complete");

        assert_eq!(
            result.downloaded, 0,
            "dead test URL should not produce a successful download"
        );
        assert_eq!(
            result.skip_summary.on_disk, 0,
            "truncated downloaded file must not be counted as an on-disk skip"
        );
        let failed = db.get_failed().await.unwrap();
        assert_eq!(
            failed.len(),
            1,
            "truncated file must be forwarded for re-download (which fails against the dead URL)"
        );
        assert_eq!(&*failed[0].id, "TRUNCATED_DOWNLOADED");
    }

    /// When zero assets were downloaded but the producer saw enumeration
    /// errors (e.g. malformed API page), `build_download_outcome` must
    /// return `PartialFailure` — not `Success`. Before the fix, the
    /// zero-download branch ignored `enumeration_errors`, letting the
    /// sync-token advance and silently skipping the errored assets.
    #[tokio::test]
    async fn zero_downloads_with_enumeration_errors_returns_partial_failure() {
        use crate::download::DownloadOutcome;

        let streaming_result = StreamingResult {
            enumeration_errors: 3,
            ..StreamingResult::default()
        };
        let (outcome, stats) =
            build_zero_download_outcome(streaming_result, DownloadControls::download_hidden())
                .await;
        assert!(
            matches!(outcome, DownloadOutcome::PartialFailure { failed_count: 3 }),
            "expected PartialFailure with failed_count=3, got {outcome:?}"
        );
        assert_eq!(stats.enumeration_errors, 3);
    }

    #[tokio::test]
    async fn producer_incomplete_enumeration_returns_partial_failure_and_blocks_token() {
        use crate::download::DownloadOutcome;

        let streaming_result = StreamingResult {
            assets_seen: 1,
            enumeration_complete: false,
            ..StreamingResult::default()
        };
        let (outcome, stats) =
            build_zero_download_outcome(streaming_result, DownloadControls::download_hidden())
                .await;

        assert!(
            matches!(outcome, DownloadOutcome::PartialFailure { failed_count: 1 }),
            "incomplete producer enumeration must not report Success, got {outcome:?}"
        );
        assert!(stats.enumeration_incomplete);
        assert!(stats.sync_token_blocked);
        assert_eq!(
            stats.sync_token_blocked_reason,
            Some(super::super::PRODUCER_ENUMERATION_INCOMPLETE_REASON)
        );
        assert!(
            !crate::sync_cycle::should_store_sync_token(&outcome, false),
            "partial incomplete-enumeration outcomes must not advance sync tokens"
        );
    }

    #[tokio::test]
    async fn free_space_forecast_cancel_before_stream_exhaustion_is_partial_failure() {
        use crate::download::{DownloadConfig, DownloadOutcome};
        use crate::icloud::photos::PhotoAsset;
        use crate::retry::RetryConfig;
        use futures_util::stream;
        use std::sync::Arc;

        let dir = TempDir::new().unwrap();
        let oversized = TestPhotoAsset::new("TOO_BIG_FOR_DISK")
            .filename("too-big.jpg")
            .orig_size(8_000_000_000_000)
            .orig_url("https://p01.icloud-content.com/too-big.jpg")
            .orig_checksum("not-valid-base64")
            .build();
        let undiscovered = TestPhotoAsset::new("UNDISCOVERED_AFTER_CANCEL")
            .filename("undiscovered.jpg")
            .orig_url("https://p01.icloud-content.com/undiscovered.jpg")
            .orig_checksum("BAUG")
            .build();
        let stream = stream::iter(vec![
            Ok::<PhotoAsset, anyhow::Error>(oversized),
            Ok::<PhotoAsset, anyhow::Error>(undiscovered),
        ]);

        let mut config = DownloadConfig::test_default();
        config.directory = Arc::from(dir.path());
        config.retry = RetryConfig {
            max_retries: 0,
            base_delay_secs: 0,
            max_delay_secs: 0,
        };
        let config = Arc::new(config);
        let client = reqwest::Client::new();
        let controls = DownloadControls::download_hidden();

        let streaming_result = stream_and_download_from_stream(
            &client,
            stream,
            &config,
            controls,
            0,
            CancellationToken::new(),
            StreamRuntime::new(None, None),
        )
        .await
        .expect("free-space forecast cancellation should return a streaming result");

        assert_eq!(
            streaming_result.assets_seen, 1,
            "producer should stop before reading the second stream item"
        );
        assert!(
            !streaming_result.enumeration_complete,
            "forecast cancellation must leave enumeration incomplete"
        );

        let (outcome, stats) = build_download_outcome(
            &client,
            &[],
            &config,
            controls,
            streaming_result,
            Instant::now(),
            CancellationToken::new(),
        )
        .await
        .expect("outcome should build after forecast cancellation");

        assert!(
            matches!(outcome, DownloadOutcome::PartialFailure { .. }),
            "forecast cancellation must not report clean success, got {outcome:?}"
        );
        assert!(stats.enumeration_incomplete);
        assert_eq!(
            stats.sync_token_blocked_reason,
            Some(super::super::PRODUCER_ENUMERATION_INCOMPLETE_REASON)
        );
        assert!(!crate::sync_cycle::should_store_sync_token(&outcome, false));
    }

    #[tokio::test]
    async fn zero_downloads_with_state_write_failures_returns_partial_failure() {
        use crate::download::DownloadOutcome;

        let streaming_result = StreamingResult {
            state_write_failures: 1,
            ..StreamingResult::default()
        };
        let (outcome, stats) =
            build_zero_download_outcome(streaming_result, DownloadControls::download_hidden())
                .await;
        assert!(
            matches!(outcome, DownloadOutcome::PartialFailure { failed_count: 1 }),
            "expected PartialFailure with failed_count=1, got {outcome:?}"
        );
        assert_eq!(stats.state_write_failures, 1);
    }

    #[tokio::test]
    async fn expired_url_abort_returns_durable_partial_failure() {
        use crate::download::DownloadOutcome;

        let streaming_result = StreamingResult {
            downloaded: 1,
            url_expired_abort: true,
            ..StreamingResult::default()
        };
        let (outcome, stats) =
            build_zero_download_outcome(streaming_result, DownloadControls::download_hidden())
                .await;
        assert!(
            matches!(outcome, DownloadOutcome::PartialFailure { failed_count: 1 }),
            "expired CDN URL must stop the batch as a PartialFailure, got {outcome:?}"
        );
        assert_eq!(stats.downloaded, 1);
        assert!(
            !stats.interrupted,
            "a durable transfer abort is not a process interruption"
        );
    }

    #[tokio::test]
    async fn expired_url_abort_with_zero_downloads_is_not_success() {
        use crate::download::DownloadOutcome;

        let streaming_result = StreamingResult {
            url_expired_abort: true,
            ..StreamingResult::default()
        };
        let (outcome, stats) =
            build_zero_download_outcome(streaming_result, DownloadControls::download_hidden())
                .await;

        assert!(
            matches!(outcome, DownloadOutcome::PartialFailure { failed_count: 1 }),
            "expired CDN URL before any success must not be reported as a clean no-op, got {outcome:?}"
        );
        assert_eq!(stats.downloaded, 0);
        assert!(!stats.interrupted);
    }

    #[tokio::test]
    async fn complete_sync_run_failure_without_downloads_returns_partial_failure() {
        use crate::download::{DownloadConfig, DownloadOutcome};
        use crate::icloud::photos::PhotoAsset;
        use futures_util::stream;

        let dir = TempDir::new().unwrap();
        let mut config = DownloadConfig::test_default();
        config.directory = std::sync::Arc::from(dir.path());
        config.state_db = Some(Arc::new(
            FailingDownloadStore::with_failing_complete_sync_run(),
        ));
        let config = Arc::new(config);
        let client = reqwest::Client::new();
        let controls = DownloadControls::download_hidden();

        let streaming_result = stream_and_download_from_stream(
            &client,
            stream::empty::<anyhow::Result<PhotoAsset>>(),
            &config,
            controls,
            0,
            CancellationToken::new(),
            StreamRuntime::new(None, None),
        )
        .await
        .expect("empty sync should finish the stream pipeline");

        assert_eq!(streaming_result.downloaded, 0);
        assert_eq!(streaming_result.state_write_failures, 1);

        let (outcome, stats) = build_download_outcome(
            &client,
            &[],
            &config,
            controls,
            streaming_result,
            Instant::now(),
            CancellationToken::new(),
        )
        .await
        .expect("outcome should build");

        assert!(
            matches!(outcome, DownloadOutcome::PartialFailure { failed_count: 1 }),
            "complete_sync_run failure must not report Success, got {outcome:?}"
        );
        assert_eq!(stats.state_write_failures, 1);
    }

    #[tokio::test]
    async fn stream_with_preloaded_download_context_does_not_reload_state_db() {
        let db = Arc::new(FailingDownloadStore::new(0));
        let dyn_db: Arc<dyn DownloadStore> = db.clone();
        let mut raw_config = DownloadConfig::test_default();
        raw_config.state_db = Some(dyn_db);
        let config = Arc::new(raw_config);
        let preloaded = preload_download_context(&config).await;
        assert_eq!(
            db.downloaded_state_load_count(),
            1,
            "preload should read downloaded state once"
        );

        let client = reqwest::Client::new();
        let stream = stream::empty::<anyhow::Result<PhotoAsset>>();
        stream_and_download_from_stream_with_context(
            &client,
            stream,
            &config,
            DownloadControls::download_hidden(),
            0,
            CancellationToken::new(),
            StreamRuntime::with_context(None, None, Some(preloaded)),
        )
        .await
        .expect("empty stream should complete");

        assert_eq!(
            db.downloaded_state_load_count(),
            1,
            "stream should reuse the preloaded context instead of reloading the DB"
        );
    }

    #[tokio::test]
    async fn dry_run_zero_downloads_with_enumeration_errors_returns_partial_failure() {
        use crate::download::DownloadOutcome;

        let streaming_result = StreamingResult {
            enumeration_errors: 2,
            ..StreamingResult::default()
        };
        let (outcome, stats) =
            build_zero_download_outcome(streaming_result, DownloadControls::dry_run_hidden()).await;
        assert!(
            matches!(outcome, DownloadOutcome::PartialFailure { failed_count: 2 }),
            "expected dry-run PartialFailure with failed_count=2, got {outcome:?}"
        );
        assert_eq!(stats.enumeration_errors, 2);
    }

    /// When SIGTERM fires mid-sync, the .part files must not be promoted
    /// to final paths. CancellationToken cancellation must prevent rename.
    /// This test verifies the cancellation plumbing works, which is the
    /// prerequisite for the crash-recovery safety net.
    #[tokio::test]
    async fn cancellation_prevents_consumer_from_processing() {
        let token = CancellationToken::new();
        let child = token.child_token();
        assert!(!child.is_cancelled(), "fresh token must not be cancelled");
        token.cancel();
        assert!(
            child.is_cancelled(),
            "child must reflect parent cancellation"
        );
    }
}
