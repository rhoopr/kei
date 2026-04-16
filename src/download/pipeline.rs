//! Streaming download pipeline — producer/consumer architecture that starts
//! downloading as soon as the first API page returns. Includes the Phase 2
//! cleanup pass and all single-task download logic.

use std::fs::FileTimes;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use futures_util::stream::{self, StreamExt};
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::Client;
use rustc_hash::{FxHashMap, FxHashSet};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;

use crate::icloud::photos::PhotoAlbum;
use crate::retry::RetryConfig;
use crate::state::{AssetRecord, StateDb, SyncRunStats};

use super::error::DownloadError;
use super::filter::{
    determine_media_type, extract_skip_candidates, filter_asset_to_tasks, is_asset_filtered,
    pre_ensure_asset_dir, DownloadTask, FilterReason, NormalizedPath,
};
use super::{paths, DownloadConfig, DownloadContext, DownloadOutcome};

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
}

/// Threshold of auth errors before aborting the download pass for re-authentication.
/// Counted cumulatively across both phases (streaming + cleanup).
pub(super) const AUTH_ERROR_THRESHOLD: usize = 3;

/// A successful download whose state write to SQLite failed on first attempt.
/// Accumulated during the download loop and retried in a final flush.
#[derive(Debug)]
struct PendingStateWrite {
    asset_id: Box<str>,
    version_size: crate::state::VersionSizeKey,
    download_path: PathBuf,
    local_checksum: String,
    download_checksum: Option<String>,
}

/// Maximum retry attempts for deferred state writes.
const STATE_WRITE_MAX_RETRIES: u32 = 6;
const _: () = assert!(STATE_WRITE_MAX_RETRIES <= 32, "shift overflow in backoff");

/// Retry all pending state writes that failed during the download loop.
///
/// Each write is attempted up to [`STATE_WRITE_MAX_RETRIES`] times with
/// exponential backoff (200ms, 400ms, 800ms, 1.6s, 3.2s between attempts
/// 1–5; attempt 6 fails immediately). SQLite lock contention is transient,
/// so generous retries prevent files from ending up on disk but untracked
/// in the state DB.
/// Returns the number of writes that still failed after all retries.
async fn flush_pending_state_writes(db: &dyn StateDb, pending: &[PendingStateWrite]) -> usize {
    if pending.is_empty() {
        return 0;
    }
    tracing::debug!(count = pending.len(), "Retrying deferred state writes");
    let mut failures = 0;
    for write in pending {
        let mut succeeded = false;
        for attempt in 1..=STATE_WRITE_MAX_RETRIES {
            match db
                .mark_downloaded(
                    &write.asset_id,
                    write.version_size.as_str(),
                    &write.download_path,
                    &write.local_checksum,
                    write.download_checksum.as_deref(),
                )
                .await
            {
                Ok(()) => {
                    succeeded = true;
                    break;
                }
                Err(e) => {
                    if attempt < STATE_WRITE_MAX_RETRIES {
                        tracing::debug!(
                            asset_id = %write.asset_id,
                            attempt,
                            error = %e,
                            "State write retry failed, will retry"
                        );
                        tokio::time::sleep(Duration::from_millis(
                            200 * u64::from(1u32 << (attempt - 1)),
                        ))
                        .await;
                    } else {
                        tracing::error!(
                            asset_id = %write.asset_id,
                            path = %write.download_path.display(),
                            error = %e,
                            "State write failed after {STATE_WRITE_MAX_RETRIES} attempts — \
                             file on disk but untracked; next sync will detect it via \
                             filesystem check and skip re-download"
                        );
                    }
                }
            }
        }
        if !succeeded {
            failures += 1;
        }
    }
    if failures > 0 {
        tracing::warn!(
            failures,
            total = pending.len(),
            "Some state writes could not be saved"
        );
    } else {
        tracing::debug!(count = pending.len(), "All deferred state writes recovered");
    }
    failures
}

/// Create a progress bar with a consistent template.
///
/// Returns `ProgressBar::hidden()` when the user passed `--no-progress-bar`,
/// `--only-print-filenames`, or stdout is not a TTY (e.g. piped output, cron
/// jobs) — this prevents output corruption and honours the user's preference.
fn create_progress_bar(
    no_progress_bar: bool,
    only_print_filenames: bool,
    total: u64,
) -> ProgressBar {
    if no_progress_bar || only_print_filenames || !std::io::stdout().is_terminal() {
        return ProgressBar::hidden();
    }
    let pb = ProgressBar::new(total);
    // Template is a compile-time constant; unwrap_or_else handles the impossible case
    if let Ok(style) = ProgressStyle::with_template(
        "[{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({eta}) {msg}",
    ) {
        pb.set_style(style.progress_chars("=> "));
    }
    pb
}

/// Configuration for a download pass.
pub(super) struct PassConfig<'a> {
    pub(super) client: &'a Client,
    pub(super) retry_config: &'a RetryConfig,
    pub(super) set_exif: bool,
    pub(super) concurrency: usize,
    pub(super) no_progress_bar: bool,
    pub(super) temp_suffix: String,
    pub(super) shutdown_token: CancellationToken,
    pub(super) state_db: Option<Arc<dyn StateDb>>,
}

impl std::fmt::Debug for PassConfig<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PassConfig")
            .field("set_exif", &self.set_exif)
            .field("concurrency", &self.concurrency)
            .field("no_progress_bar", &self.no_progress_bar)
            .field("temp_suffix", &self.temp_suffix)
            .field("state_db", &self.state_db.as_ref().map(|_| ".."))
            .finish_non_exhaustive()
    }
}

/// Result of a download pass.
#[derive(Debug)]
pub(super) struct PassResult {
    pub(super) exif_failures: usize,
    pub(super) failed: Vec<DownloadTask>,
    pub(super) auth_errors: usize,
    pub(super) state_write_failures: usize,
    pub(super) bytes_downloaded: u64,
    pub(super) disk_bytes_written: u64,
}

/// Streaming download pipeline that consumes a pre-built combined stream.
///
/// This is the core producer/consumer download logic from `stream_and_download`,
/// factored out so that `download_photos_full_with_token` can supply a
/// token-aware combined stream while reusing the same download machinery.
pub(super) async fn stream_and_download_from_stream<S>(
    download_client: &Client,
    combined: S,
    config: &Arc<DownloadConfig>,
    total: u64,
    shutdown_token: CancellationToken,
) -> Result<StreamingResult>
where
    S: futures_util::Stream<Item = anyhow::Result<crate::icloud::photos::PhotoAsset>>
        + Send
        + 'static,
{
    let pb = create_progress_bar(config.no_progress_bar, config.only_print_filenames, total);

    if config.only_print_filenames {
        // Load state DB context so we skip already-downloaded assets,
        // matching the incremental path's behavior.
        let download_ctx = if let Some(db) = &config.state_db {
            DownloadContext::load(db.as_ref(), false).await
        } else {
            DownloadContext::default()
        };

        tokio::pin!(combined);
        let mut enum_errors = 0usize;
        let mut claimed_paths: FxHashMap<NormalizedPath, u64> = FxHashMap::default();
        let mut dir_cache = paths::DirCache::new();
        while let Some(result) = combined.next().await {
            if shutdown_token.is_cancelled() {
                break;
            }
            match result {
                Ok(asset) => {
                    if is_asset_filtered(&asset, config).is_some() {
                        continue;
                    }
                    let candidates = extract_skip_candidates(&asset, config);
                    if !candidates.is_empty()
                        && candidates.iter().all(|&(vs, cs)| {
                            matches!(
                                download_ctx.should_download_fast(asset.id(), vs, cs, true),
                                Some(false)
                            )
                        })
                    {
                        continue;
                    }

                    pre_ensure_asset_dir(&mut dir_cache, &asset, config).await;
                    let tasks =
                        filter_asset_to_tasks(&asset, config, &mut claimed_paths, &mut dir_cache);
                    for task in &tasks {
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
            ..StreamingResult::default()
        });
    }

    if config.dry_run {
        tokio::pin!(combined);
        let mut count = 0usize;
        let mut enum_errors = 0usize;
        let mut claimed_paths: FxHashMap<NormalizedPath, u64> = FxHashMap::default();
        let mut dir_cache = paths::DirCache::new();
        while let Some(result) = combined.next().await {
            if shutdown_token.is_cancelled() {
                tracing::info!("Shutdown requested, stopping dry run");
                break;
            }
            match result {
                Ok(asset) => {
                    if is_asset_filtered(&asset, config).is_some() {
                        continue;
                    }
                    pre_ensure_asset_dir(&mut dir_cache, &asset, config).await;
                    let tasks =
                        filter_asset_to_tasks(&asset, config, &mut claimed_paths, &mut dir_cache);
                    for task in &tasks {
                        tracing::info!(path = %task.download_path.display(), "[DRY RUN] Would download");
                    }
                    count += tasks.len();
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
            ..StreamingResult::default()
        });
    }

    let download_client = download_client.clone();
    let retry_config = config.retry;
    let set_exif = config.set_exif_datetime;
    let concurrency = config.concurrent_downloads;
    let state_db = config.state_db.clone();

    // Pre-load download context for O(1) skip decisions
    let download_ctx = if let Some(db) = &state_db {
        tracing::debug!("Pre-loading download state from database");
        DownloadContext::load(db.as_ref(), config.retry_only).await
    } else {
        DownloadContext::default()
    };
    tracing::debug!(
        downloaded_ids = download_ctx.downloaded_ids.len(),
        "Download context loaded"
    );

    // Determine if we can trust the state DB for early skips
    let trust_state = if let Some(db) = &state_db {
        let config_hash = super::hash_download_config(config);
        let stored_hash = db.get_metadata("config_hash").await.unwrap_or(None);
        let mut trust = stored_hash.as_deref() == Some(&config_hash);
        if !trust {
            if stored_hash.is_some() {
                tracing::info!("Download config changed since last sync, verifying all files");
                // Clear stored sync tokens so the next cycle/run falls back to
                // full enumeration, picking up assets that the old incremental
                // token would have missed under the new filter settings.
                match db.delete_metadata_by_prefix("sync_token:").await {
                    Ok(n) if n > 0 => {
                        tracing::debug!(cleared = n, "Cleared stale sync tokens");
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "Failed to clear sync tokens");
                    }
                    _ => {}
                }
            }
            if let Err(e) = db.set_metadata("config_hash", &config_hash).await {
                tracing::warn!(error = %e, "Failed to persist config_hash");
            }
        }
        trust = trust && !download_ctx.downloaded_ids.is_empty();

        // Sample-check that "downloaded" files still exist on disk
        if trust {
            let sample_count = download_ctx
                .downloaded_ids
                .len()
                .div_ceil(20) // ~5% sample
                .clamp(5, 500);
            match db.sample_downloaded_paths(sample_count).await {
                Ok(paths) => {
                    let missing: Vec<_> = paths.iter().filter(|p| !p.exists()).collect();
                    if !missing.is_empty() {
                        tracing::warn!(
                            sampled = paths.len(),
                            missing = missing.len(),
                            "Sample check found missing files, disabling trust-state"
                        );
                        for p in &missing {
                            tracing::debug!(path = %p.display(), "Missing downloaded file");
                        }
                        trust = false;
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to sample downloaded paths, disabling trust-state");
                    trust = false;
                }
            }
        }
        trust
    } else {
        false
    };
    if trust_state {
        tracing::debug!(
            "Trust-state mode active: skipping filesystem checks for DB-confirmed assets"
        );
    }

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

    let mut downloaded = 0usize;
    let mut exif_failures = 0usize;
    let mut failed: Vec<DownloadTask> = Vec::new();
    let mut auth_errors = 0usize;
    let mut pending_state_writes: Vec<PendingStateWrite> = Vec::new();
    let mut bytes_downloaded_total: u64 = 0;
    let mut disk_bytes_total: u64 = 0;

    let (task_tx, task_rx) = mpsc::channel::<DownloadTask>(concurrency * 2);

    let assets_seen = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let assets_seen_producer = Arc::clone(&assets_seen);
    let enum_errors = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let enum_errors_producer = Arc::clone(&enum_errors);

    let producer_config = Arc::clone(config);
    let producer_state_db = state_db.clone();
    let producer_shutdown = shutdown_token.clone();
    let producer_pb = pb.clone();
    let producer = tokio::spawn(async move {
        let config = &producer_config;
        let mut claimed_paths: FxHashMap<NormalizedPath, u64> = FxHashMap::default();
        let mut dir_cache = paths::DirCache::new();
        let mut seen_ids: FxHashSet<Box<str>> = FxHashSet::default();
        let mut skips = ProducerSkipSummary::default();
        let mut assets_forwarded = 0u64;
        tokio::pin!(combined);
        while let Some(result) = combined.next().await {
            if producer_shutdown.is_cancelled() {
                break;
            }
            match result {
                Ok(asset) => {
                    if !seen_ids.insert(asset.id().into()) {
                        tracing::warn!(
                            asset_id = %asset.id(),
                            "Duplicate asset ID from API, skipping"
                        );
                        skips.duplicates += 1;
                        continue;
                    }

                    assets_seen_producer.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

                    if let Some(reason) = is_asset_filtered(&asset, config) {
                        match reason {
                            FilterReason::ExcludedAlbum => skips.by_excluded_album += 1,
                            FilterReason::MediaType => skips.by_media_type += 1,
                            FilterReason::LivePhoto => skips.by_live_photo += 1,
                            FilterReason::DateRange => skips.by_date_range += 1,
                            FilterReason::Filename => skips.by_filename += 1,
                        }
                        producer_pb.inc(1);
                        continue;
                    }

                    if trust_state {
                        let candidates = extract_skip_candidates(&asset, config);
                        if !candidates.is_empty()
                            && candidates.iter().all(|&(vs, cs)| {
                                matches!(
                                    download_ctx.should_download_fast(asset.id(), vs, cs, true),
                                    Some(false)
                                )
                            })
                        {
                            if let Some(db) = &producer_state_db {
                                if let Err(e) = db.touch_last_seen(asset.id()).await {
                                    tracing::debug!(error = %e, asset_id = asset.id(), "Failed to update last-seen timestamp");
                                }
                            }
                            skips.by_state += 1;
                            producer_pb.inc(1);
                            continue;
                        }
                    }

                    pre_ensure_asset_dir(&mut dir_cache, &asset, config).await;

                    let tasks =
                        filter_asset_to_tasks(&asset, config, &mut claimed_paths, &mut dir_cache);
                    if tasks.is_empty() {
                        // Asset was enumerated but produced no tasks (files on
                        // disk or dedup'd). Update last_seen_at so
                        // promote_pending_to_failed knows this asset was seen.
                        if let Some(db) = &producer_state_db {
                            if let Err(e) = db.touch_last_seen(asset.id()).await {
                                tracing::debug!(error = %e, asset_id = asset.id(), "Failed to touch last_seen for on-disk asset");
                            }
                        }
                        skips.on_disk += 1;
                        producer_pb.inc(1);
                    } else {
                        let mut disposition = AssetDisposition::Unresolved;

                        for task in tasks {
                            // Mark assets that have exceeded the retry limit as failed.
                            if let Some(&attempts) =
                                download_ctx.attempt_counts.get(task.asset_id.as_ref())
                            {
                                if config.max_download_attempts > 0
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
                                        if let Err(e) = db
                                            .mark_failed(
                                                &task.asset_id,
                                                task.version_size.as_str(),
                                                &error,
                                            )
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
                            }

                            if config.retry_only
                                && !download_ctx.known_ids.contains(task.asset_id.as_ref())
                            {
                                tracing::debug!(
                                    asset_id = %task.asset_id,
                                    "Skipping new asset in retry-only mode"
                                );
                                disposition = disposition.max(AssetDisposition::RetryOnly);
                                continue;
                            }

                            if let Some(db) = &producer_state_db {
                                let media_type = determine_media_type(task.version_size, &asset);
                                let record = AssetRecord::new_pending(
                                    task.asset_id.to_string(),
                                    task.version_size,
                                    task.checksum.to_string(),
                                    task.download_path
                                        .file_name()
                                        .and_then(|f| f.to_str())
                                        .unwrap_or("")
                                        .to_string(),
                                    asset.created(),
                                    Some(asset.added_date()),
                                    task.size,
                                    media_type,
                                );
                                if let Err(e) = db.upsert_seen(&record).await {
                                    tracing::warn!(
                                        asset_id = %task.asset_id,
                                        error = %e,
                                        "Failed to record asset"
                                    );
                                }

                                match download_ctx.should_download_fast(
                                    &task.asset_id,
                                    task.version_size,
                                    &task.checksum,
                                    false,
                                ) {
                                    Some(true) => {
                                        disposition = disposition.max(AssetDisposition::Forwarded);
                                        if task_tx.send(task).await.is_err() {
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
                                        // Directory was pre-populated above, so these
                                        // are cache-hits -- no blocking I/O.
                                        if dir_cache.exists(&task.download_path) {
                                            disposition = disposition.max(AssetDisposition::OnDisk);
                                            tracing::debug!(
                                                asset_id = %task.asset_id,
                                                path = %task.download_path.display(),
                                                "Skipping (already downloaded)"
                                            );
                                        } else if dir_cache
                                            .find_ampm_variant(&task.download_path)
                                            .is_some()
                                        {
                                            disposition =
                                                disposition.max(AssetDisposition::AmpmVariant);
                                            tracing::debug!(
                                                asset_id = %task.asset_id,
                                                path = %task.download_path.display(),
                                                "Skipping (AM/PM variant exists on disk)"
                                            );
                                        } else {
                                            tracing::debug!(
                                                asset_id = %task.asset_id,
                                                path = %task.download_path.display(),
                                                "File missing, will re-download"
                                            );
                                            disposition =
                                                disposition.max(AssetDisposition::Forwarded);
                                            if task_tx.send(task).await.is_err() {
                                                return skips;
                                            }
                                        }
                                    }
                                }
                            } else {
                                disposition = disposition.max(AssetDisposition::Forwarded);
                                if task_tx.send(task).await.is_err() {
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
                    producer_pb.suspend(|| tracing::error!(error = %e, "Error fetching asset"));
                }
            }
        }

        let total_skipped = skips.total();
        if total_skipped > 0 {
            producer_pb.suspend(|| {
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
            });
        }

        // Invariant: every unique asset must be either skipped or forwarded.
        // Duplicates and enum errors are outside the unique-asset count.
        let seen = assets_seen_producer.load(std::sync::atomic::Ordering::Relaxed);
        let skipped_unique = (total_skipped - skips.duplicates) as u64;
        let accounted = skipped_unique + assets_forwarded;
        if accounted != seen {
            producer_pb.suspend(|| {
                tracing::warn!(
                    assets_seen = seen,
                    accounted,
                    forwarded = assets_forwarded,
                    skipped = skipped_unique,
                    duplicates = skips.duplicates,
                    "Asset accounting mismatch -- some assets may be untracked"
                );
            });
        }

        skips
    });

    let temp_suffix: Arc<str> = config.temp_suffix.clone().into();
    let download_stream = ReceiverStream::new(task_rx)
        .map(|task| {
            let client = download_client.clone();
            let temp_suffix = Arc::clone(&temp_suffix);
            async move {
                let result = Box::pin(download_single_task(
                    &client,
                    &task,
                    &retry_config,
                    set_exif,
                    &temp_suffix,
                ))
                .await;
                (task, result)
            }
        })
        .buffer_unordered(concurrency);

    tokio::pin!(download_stream);

    while let Some((task, result)) = download_stream.next().await {
        if shutdown_token.is_cancelled() {
            pb.suspend(|| tracing::info!("Shutdown requested, stopping new downloads"));
            break;
        }
        let filename = task
            .download_path
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or("")
            .to_string();
        pb.set_message(filename);
        match result {
            Ok((exif_ok, local_checksum, download_checksum, bytes_dl, disk_bytes)) => {
                downloaded += 1;
                bytes_downloaded_total += bytes_dl;
                disk_bytes_total += disk_bytes;
                if !exif_ok {
                    exif_failures += 1;
                }
                if let Some(db) = &state_db {
                    if let Err(e) = db
                        .mark_downloaded(
                            &task.asset_id,
                            task.version_size.as_str(),
                            &task.download_path,
                            &local_checksum,
                            download_checksum.as_deref(),
                        )
                        .await
                    {
                        pb.suspend(|| {
                            tracing::warn!(
                                asset_id = %task.asset_id,
                                error = %e,
                                "State write failed, deferring for retry"
                            );
                        });
                        pending_state_writes.push(PendingStateWrite {
                            asset_id: task.asset_id.clone(),
                            version_size: task.version_size,
                            download_path: task.download_path.clone(),
                            local_checksum,
                            download_checksum,
                        });
                    }
                }
            }
            Err(e) => {
                if let Some(download_err) = e.downcast_ref::<DownloadError>() {
                    if download_err.is_session_expired() {
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
                    } else {
                        pb.suspend(|| {
                            tracing::error!(asset_id = %task.asset_id, path = %task.download_path.display(), error = %e, "Download failed");
                        });
                    }
                } else {
                    pb.suspend(|| {
                        tracing::error!(asset_id = %task.asset_id, path = %task.download_path.display(), error = %e, "Download failed");
                    });
                }
                if let Some(db) = &state_db {
                    if let Err(e) = db
                        .mark_failed(&task.asset_id, task.version_size.as_str(), &e.to_string())
                        .await
                    {
                        tracing::warn!(
                            asset_id = %task.asset_id,
                            error = %e,
                            "Failed to mark failure"
                        );
                    }
                }
                failed.push(task);
            }
        }
    }

    let (producer_panicked, producer_skips) = match producer.await {
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

    let assets_seen_count = assets_seen.load(std::sync::atomic::Ordering::Relaxed);

    pb.finish_and_clear();

    if let (Some(db), Some(run_id)) = (&state_db, sync_run_id) {
        let stats = SyncRunStats {
            assets_seen: assets_seen_count,
            assets_downloaded: downloaded as u64,
            assets_failed: failed.len() as u64,
            interrupted: shutdown_token.is_cancelled()
                || auth_errors >= AUTH_ERROR_THRESHOLD
                || producer_panicked,
        };
        if let Err(e) = db.complete_sync_run(run_id, &stats).await {
            tracing::warn!(error = %e, "Failed to complete sync run tracking");
        } else {
            tracing::debug!(
                run_id,
                assets_seen = assets_seen_count,
                downloaded,
                failed = failed.len(),
                "Completed sync run"
            );
        }
    }

    if producer_panicked {
        return Err(anyhow::anyhow!(
            "Asset producer panicked — sync may be incomplete"
        ));
    }

    // Retry any state writes that failed during the streaming loop
    let state_write_failures = if let Some(db) = &state_db {
        flush_pending_state_writes(db.as_ref(), &pending_state_writes).await
    } else {
        0
    };

    Ok(StreamingResult {
        downloaded,
        exif_failures,
        failed,
        auth_errors,
        state_write_failures,
        enumeration_errors: enum_errors.load(std::sync::atomic::Ordering::Relaxed),
        assets_seen: assets_seen_count,
        skip_summary: producer_skips,
        bytes_downloaded: bytes_downloaded_total,
        disk_bytes_written: disk_bytes_total,
    })
}

/// Build a `DownloadOutcome` from a `StreamingResult`, running a cleanup
/// pass if there were failures. Shared between `download_photos` and
/// `download_photos_full_with_token`.
pub(super) async fn build_download_outcome(
    download_client: &Client,
    albums: &[PhotoAlbum],
    config: &Arc<DownloadConfig>,
    streaming_result: StreamingResult,
    started: Instant,
    shutdown_token: CancellationToken,
) -> Result<(DownloadOutcome, super::SyncStats)> {
    let downloaded = streaming_result.downloaded;
    let mut exif_failures = streaming_result.exif_failures;
    let failed_tasks = streaming_result.failed;
    let auth_errors = streaming_result.auth_errors;
    let mut state_write_failures = streaming_result.state_write_failures;
    let enumeration_errors = streaming_result.enumeration_errors;
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
            elapsed_secs: started.elapsed().as_secs_f64(),
            interrupted: true,
        };
        return Ok((
            DownloadOutcome::SessionExpired {
                auth_error_count: auth_errors,
            },
            stats,
        ));
    }

    if downloaded == 0 && failed_tasks.is_empty() {
        let stats = super::SyncStats {
            assets_seen: streaming_result.assets_seen,
            skipped: skip_breakdown,
            elapsed_secs: started.elapsed().as_secs_f64(),
            interrupted: shutdown_token.is_cancelled(),
            ..super::SyncStats::default()
        };
        if config.dry_run {
            tracing::info!("── Dry Run Summary ──");
            tracing::info!("  0 files would be downloaded");
            tracing::info!(destination = %config.directory.display(), "  destination");
        } else {
            tracing::info!("No new photos to download");
        }
        return Ok((DownloadOutcome::Success, stats));
    }

    if config.dry_run {
        let stats = super::SyncStats {
            assets_seen: streaming_result.assets_seen,
            downloaded,
            skipped: skip_breakdown,
            elapsed_secs: started.elapsed().as_secs_f64(),
            interrupted: shutdown_token.is_cancelled(),
            ..super::SyncStats::default()
        };
        tracing::info!("── Dry Run Summary ──");
        if shutdown_token.is_cancelled() {
            tracing::info!(scanned = downloaded, "  Interrupted before shutdown");
        } else {
            tracing::info!(count = downloaded, "  files would be downloaded");
        }
        tracing::info!(destination = %config.directory.display(), "  destination");
        tracing::info!(concurrency = config.concurrent_downloads, "  concurrency");
        return Ok((DownloadOutcome::Success, stats));
    }

    if failed_tasks.is_empty() {
        let stats = super::SyncStats {
            assets_seen: streaming_result.assets_seen,
            downloaded,
            failed: 0,
            skipped: skip_breakdown,
            bytes_downloaded: streaming_result.bytes_downloaded,
            disk_bytes_written: streaming_result.disk_bytes_written,
            exif_failures,
            state_write_failures,
            enumeration_errors,
            elapsed_secs: started.elapsed().as_secs_f64(),
            interrupted: shutdown_token.is_cancelled(),
        };
        log_sync_summary("\u{2500}\u{2500} Summary \u{2500}\u{2500}", &stats);
        if state_write_failures > 0 || enumeration_errors > 0 || exif_failures > 0 {
            return Ok((
                DownloadOutcome::PartialFailure {
                    failed_count: state_write_failures + enumeration_errors + exif_failures,
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

    let fresh_tasks = super::build_download_tasks(albums, config, shutdown_token.clone()).await?;
    tracing::debug!(
        count = fresh_tasks.len(),
        "  Re-fetched tasks with fresh URLs"
    );

    let phase2_task_count = fresh_tasks.len();
    let pass_config = PassConfig {
        client: download_client,
        retry_config: &config.retry,
        set_exif: config.set_exif_datetime,
        concurrency: cleanup_concurrency,
        no_progress_bar: config.no_progress_bar,
        temp_suffix: config.temp_suffix.clone(),
        shutdown_token: shutdown_token.clone(),
        state_db: config.state_db.clone(),
    };
    let pass_result = run_download_pass(pass_config, fresh_tasks).await;

    let remaining_failed = pass_result.failed;
    let phase2_auth_errors = pass_result.auth_errors;
    exif_failures += pass_result.exif_failures;
    state_write_failures += pass_result.state_write_failures;
    let total_auth_errors = auth_errors + phase2_auth_errors;

    if total_auth_errors >= AUTH_ERROR_THRESHOLD {
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
            elapsed_secs: started.elapsed().as_secs_f64(),
            interrupted: true,
        };
        return Ok((
            DownloadOutcome::SessionExpired {
                auth_error_count: total_auth_errors,
            },
            stats,
        ));
    }

    let failed = remaining_failed.len();
    let phase2_succeeded = phase2_task_count - failed;
    let succeeded = downloaded + phase2_succeeded;

    // Log failed downloads before the summary
    let total_failures = failed + state_write_failures + exif_failures;
    if total_failures > 0 {
        for task in &remaining_failed {
            tracing::error!(asset_id = %task.asset_id, path = %task.download_path.display(), "Download failed");
        }
    }

    let stats = super::SyncStats {
        assets_seen: streaming_result.assets_seen,
        downloaded: succeeded,
        failed,
        skipped: skip_breakdown,
        bytes_downloaded: streaming_result.bytes_downloaded + pass_result.bytes_downloaded,
        disk_bytes_written: streaming_result.disk_bytes_written + pass_result.disk_bytes_written,
        exif_failures,
        state_write_failures,
        enumeration_errors,
        elapsed_secs: started.elapsed().as_secs_f64(),
        interrupted: shutdown_token.is_cancelled(),
    };
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
    let pb = create_progress_bar(config.no_progress_bar, false, tasks.len() as u64);
    let client = config.client.clone();
    let retry_config = config.retry_config;
    let set_exif = config.set_exif;
    let state_db = config.state_db.clone();
    let shutdown_token = config.shutdown_token.clone();
    let concurrency = config.concurrency;
    let temp_suffix: Arc<str> = config.temp_suffix.into();

    type DownloadResult = (
        DownloadTask,
        Result<(bool, String, Option<String>, u64, u64)>,
    );
    let results: Vec<DownloadResult> = stream::iter(tasks)
        .take_while(|_| std::future::ready(!shutdown_token.is_cancelled()))
        .map(|task| {
            let client = client.clone();
            let temp_suffix = Arc::clone(&temp_suffix);
            async move {
                let result = Box::pin(download_single_task(
                    &client,
                    &task,
                    retry_config,
                    set_exif,
                    &temp_suffix,
                ))
                .await;
                (task, result)
            }
        })
        .buffer_unordered(concurrency)
        .collect()
        .await;

    let mut failed: Vec<DownloadTask> = Vec::new();
    let mut auth_errors = 0usize;
    let mut exif_failures = 0usize;
    let mut pending_state_writes: Vec<PendingStateWrite> = Vec::new();
    let mut bytes_downloaded_total: u64 = 0;
    let mut disk_bytes_total: u64 = 0;

    for (task, result) in results {
        match &result {
            Ok((exif_ok, local_checksum, download_checksum, bytes_dl, disk_bytes)) => {
                bytes_downloaded_total += bytes_dl;
                disk_bytes_total += disk_bytes;
                if !*exif_ok {
                    exif_failures += 1;
                }
                if let Some(db) = &state_db {
                    if let Err(e) = db
                        .mark_downloaded(
                            &task.asset_id,
                            task.version_size.as_str(),
                            &task.download_path,
                            local_checksum,
                            download_checksum.as_deref(),
                        )
                        .await
                    {
                        pb.suspend(|| {
                            tracing::warn!(
                                asset_id = %task.asset_id,
                                error = %e,
                                "State write failed, deferring for retry"
                            );
                        });
                        pending_state_writes.push(PendingStateWrite {
                            asset_id: task.asset_id.clone(),
                            version_size: task.version_size,
                            download_path: task.download_path.clone(),
                            local_checksum: local_checksum.clone(),
                            download_checksum: download_checksum.clone(),
                        });
                    }
                }
            }
            Err(e) => {
                let is_auth = e
                    .downcast_ref::<DownloadError>()
                    .is_some_and(DownloadError::is_session_expired);
                if is_auth {
                    auth_errors += 1;
                    pb.suspend(|| {
                        tracing::warn!(path = %task.download_path.display(), error = %e, "Auth error");
                    });
                } else {
                    pb.suspend(|| {
                        tracing::error!(asset_id = %task.asset_id, path = %task.download_path.display(), error = %e, "Download failed");
                    });
                }
                if let Some(db) = &state_db {
                    if let Err(e) = db
                        .mark_failed(&task.asset_id, task.version_size.as_str(), &e.to_string())
                        .await
                    {
                        tracing::warn!(
                            asset_id = %task.asset_id,
                            error = %e,
                            "Failed to mark failure"
                        );
                    }
                }
                failed.push(task);
            }
        }
        pb.inc(1);
    }

    // Retry any state writes that failed during the pass
    let state_write_failures = if let Some(db) = &state_db {
        flush_pending_state_writes(db.as_ref(), &pending_state_writes).await
    } else {
        0
    };

    pb.finish_and_clear();
    PassResult {
        exif_failures,
        failed,
        auth_errors,
        state_write_failures,
        bytes_downloaded: bytes_downloaded_total,
        disk_bytes_written: disk_bytes_total,
    }
}

/// Download a single task, handling mtime and EXIF stamping on success.
///
/// Returns `Ok(true)` on full success, `Ok(false)` if the download succeeded
/// but EXIF stamping failed (the file is usable but lacks EXIF metadata).
async fn download_single_task(
    client: &Client,
    task: &DownloadTask,
    retry_config: &RetryConfig,
    set_exif: bool,
    temp_suffix: &str,
) -> Result<(bool, String, Option<String>, u64, u64)> {
    if let Some(parent) = task.download_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("failed to create directory {}", parent.display()))?;
    }

    tracing::debug!(
        size_bytes = task.size,
        path = %task.download_path.display(),
        "downloading",
    );

    // Determine if EXIF modification is needed so we can keep the .part file
    // around for modification before the atomic rename to the final path.
    let needs_exif = set_exif && {
        let ext = task
            .download_path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        matches!(ext.to_ascii_lowercase().as_str(), "jpg" | "jpeg")
    };

    let bytes_downloaded = Box::pin(super::file::download_file(
        client,
        &task.url,
        &task.download_path,
        &task.checksum,
        retry_config,
        temp_suffix,
        super::file::DownloadOpts {
            skip_rename: needs_exif,
            expected_size: if task.size > 0 { Some(task.size) } else { None },
        },
    ))
    .await?;

    // When EXIF is needed, modifications happen on the .part file before
    // the atomic rename, preventing silent corruption on power loss / SIGKILL.
    let part_path = if needs_exif {
        Some(
            super::file::temp_download_path(&task.download_path, &task.checksum, temp_suffix)
                .context("failed to compute part path")?,
        )
    } else {
        None
    };

    // Compute SHA-256 of the downloaded content before EXIF modification
    // so we store a hash that reflects the original download bytes.
    let download_checksum = if let Some(path) = &part_path {
        Some(super::file::compute_sha256(path).await?)
    } else {
        None
    };

    let mut exif_ok = true;
    if let Some(part) = &part_path {
        let exif_path = part.clone();
        let date_str = task.created_local.format("%Y:%m:%d %H:%M:%S").to_string();
        let exif_result =
            tokio::task::spawn_blocking(move || match super::exif::get_photo_exif(&exif_path) {
                Ok(None) => {
                    if let Err(e) = super::exif::set_photo_exif(&exif_path, &date_str) {
                        tracing::warn!(path = %exif_path.display(), error = %e, "Failed to set EXIF");
                        false
                    } else {
                        true
                    }
                }
                Ok(Some(_)) => true,
                Err(e) => {
                    tracing::warn!(path = %exif_path.display(), error = %e, "Failed to read EXIF");
                    false
                }
            })
            .await;
        match exif_result {
            Ok(ok) => exif_ok = ok,
            Err(e) => {
                tracing::warn!(error = %e, "EXIF task panicked");
                exif_ok = false;
            }
        }
    }

    // Set mtime on .part (before rename) or final path directly.
    // rename() preserves mtime so this works in both cases.
    let mtime_target = part_path
        .as_deref()
        .unwrap_or(&task.download_path)
        .to_path_buf();
    let ts = task.created_local.timestamp();
    if let Err(e) = tokio::task::spawn_blocking(move || set_file_mtime(&mtime_target, ts)).await? {
        tracing::warn!(
            path = %task.download_path.display(),
            error = %e,
            "Could not set mtime"
        );
    }

    // Atomic rename: .part → final (only when EXIF path was used)
    if let Some(part) = &part_path {
        super::file::rename_part_to_final(part, &task.download_path).await?;
    }

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
    tracing::info!("{title}");

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

    // Line 2: error details (only if any)
    if stats.exif_failures > 0 || stats.state_write_failures > 0 {
        tracing::info!(
            "  {} EXIF write failure(s), {} state write failure(s)",
            stats.exif_failures,
            stats.state_write_failures
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
            reasons.push(format!(
                "{} live photo variants",
                stats.skipped.ampm_variant
            ));
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
    use super::*;
    use crate::state::error::StateError;
    use crate::state::types::SyncSummary;
    use crate::state::{AssetRecord, SyncRunStats, VersionSizeKey};
    use crate::test_helpers::TestPhotoAsset;
    use std::collections::{HashMap, HashSet};
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;

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

    #[test]
    fn test_create_progress_bar_hidden_when_disabled() {
        let pb = create_progress_bar(true, false, 100);
        assert!(pb.is_hidden());
    }

    #[test]
    fn test_create_progress_bar_hidden_when_only_print_filenames() {
        let pb = create_progress_bar(false, true, 100);
        assert!(pb.is_hidden());
    }

    #[test]
    fn test_create_progress_bar_with_total() {
        // When not disabled, the bar should have the correct length.
        // In CI/test environments stdout may not be a TTY, so the bar
        // may be hidden — we test both branches.
        let pb = create_progress_bar(false, false, 42);
        if std::io::stdout().is_terminal() {
            assert!(!pb.is_hidden());
            assert_eq!(pb.length(), Some(42));
        } else {
            // Non-TTY: bar is hidden regardless of the flag
            assert!(pb.is_hidden());
        }
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
                                url: "https://example.com/a".into(),
                                download_path: dir.path().join("a.jpg"),
                                checksum: "aaa".into(),
                                created_local: chrono::Local::now(),
                                size: 1000,
                                asset_id: "ASSET_A".into(),
                                version_size: VersionSizeKey::Original,
                            },
                            DownloadTask {
                                url: "https://example.com/b".into(),
                                download_path: dir.path().join("b.jpg"),
                                checksum: "bbb".into(),
                                created_local: chrono::Local::now(),
                                size: 2000,
                                asset_id: "ASSET_B".into(),
                                version_size: VersionSizeKey::Original,
                            },
                        ];

                        let client = Client::new();
                        let retry = RetryConfig::default();

                        let pass_config = PassConfig {
                            client: &client,
                            retry_config: &retry,
                            set_exif: false,
                            concurrency: 1,
                            no_progress_bar: true,
                            temp_suffix: ".kei-tmp".to_string(),
                            shutdown_token: token,
                            state_db: None,
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
                            checksum: "ccc".into(),
                            created_local: chrono::Local::now(),
                            size: 500,
                            asset_id: "ASSET_C".into(),
                            version_size: VersionSizeKey::Original,
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
                            set_exif: false,
                            concurrency: 1,
                            no_progress_bar: true,
                            temp_suffix: ".kei-tmp".to_string(),
                            shutdown_token: token,
                            state_db: None,
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
    fn test_producer_skip_summary_default_is_zero() {
        let skips = ProducerSkipSummary::default();
        assert_eq!(skips.total(), 0);
    }

    /// The producer relies on `AssetDisposition` ordering via `.max()` to
    /// pick the highest-priority outcome when an asset has mixed task results.
    /// If variant order changes, `.max()` silently picks the wrong winner.
    #[test]
    fn test_asset_disposition_ordering() {
        use AssetDisposition::*;
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
    /// A StateDb stub where `mark_downloaded` fails a configurable number
    /// of times before succeeding. All other methods panic (unused).
    struct FailingStateDb {
        remaining_failures: AtomicUsize,
        successes: AtomicUsize,
    }

    impl FailingStateDb {
        fn new(fail_count: usize) -> Self {
            Self {
                remaining_failures: AtomicUsize::new(fail_count),
                successes: AtomicUsize::new(0),
            }
        }

        fn success_count(&self) -> usize {
            self.successes.load(Ordering::Relaxed)
        }
    }

    #[async_trait::async_trait]
    impl StateDb for FailingStateDb {
        #[cfg(test)]
        async fn should_download(
            &self,
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
            _: &Path,
            _: &str,
            _: Option<&str>,
        ) -> Result<(), StateError> {
            let prev = self.remaining_failures.fetch_sub(1, Ordering::Relaxed);
            if prev > 0 {
                Err(StateError::LockPoisoned("simulated failure".into()))
            } else {
                self.remaining_failures.store(0, Ordering::Relaxed);
                self.successes.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
        }
        async fn mark_failed(&self, _: &str, _: &str, _: &str) -> Result<(), StateError> {
            unimplemented!()
        }
        async fn get_failed(&self) -> Result<Vec<AssetRecord>, StateError> {
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
            unimplemented!()
        }
        async fn start_sync_run(&self) -> Result<i64, StateError> {
            unimplemented!()
        }
        async fn complete_sync_run(&self, _: i64, _: &SyncRunStats) -> Result<(), StateError> {
            unimplemented!()
        }
        async fn reset_failed(&self) -> Result<u64, StateError> {
            unimplemented!()
        }
        async fn prepare_for_retry(&self) -> Result<(u64, u64, u64), StateError> {
            Ok((0, 0, 0))
        }
        async fn promote_pending_to_failed(&self, _seen_since: i64) -> Result<u64, StateError> {
            Ok(0)
        }
        async fn get_downloaded_ids(&self) -> Result<HashSet<(String, String)>, StateError> {
            unimplemented!()
        }
        async fn get_all_known_ids(&self) -> Result<HashSet<String>, StateError> {
            unimplemented!()
        }
        async fn get_downloaded_checksums(
            &self,
        ) -> Result<HashMap<(String, String), String>, StateError> {
            unimplemented!()
        }
        async fn get_attempt_counts(&self) -> Result<HashMap<String, u32>, StateError> {
            Ok(HashMap::new())
        }
        async fn get_metadata(&self, _: &str) -> Result<Option<String>, StateError> {
            unimplemented!()
        }
        async fn set_metadata(&self, _: &str, _: &str) -> Result<(), StateError> {
            unimplemented!()
        }
        async fn delete_metadata_by_prefix(&self, _: &str) -> Result<u64, StateError> {
            unimplemented!()
        }
        async fn touch_last_seen(&self, _: &str) -> Result<(), StateError> {
            unimplemented!()
        }
        async fn sample_downloaded_paths(
            &self,
            _: usize,
        ) -> Result<Vec<std::path::PathBuf>, StateError> {
            unimplemented!()
        }
    }

    #[tokio::test]
    async fn flush_pending_state_writes_empty_is_noop() {
        let db = FailingStateDb::new(0);
        let result = flush_pending_state_writes(&db, &[]).await;
        assert_eq!(result, 0);
        assert_eq!(db.success_count(), 0);
    }

    #[tokio::test]
    async fn flush_pending_state_writes_succeeds_on_first_try() {
        let db = FailingStateDb::new(0);
        let pending = vec![PendingStateWrite {
            asset_id: "A1".into(),
            version_size: VersionSizeKey::Original,
            download_path: PathBuf::from("/tmp/claude/photo.jpg"),
            local_checksum: "abc".into(),
            download_checksum: None,
        }];
        let failures = flush_pending_state_writes(&db, &pending).await;
        assert_eq!(failures, 0);
        assert_eq!(db.success_count(), 1);
    }

    #[tokio::test]
    async fn flush_pending_state_writes_recovers_after_transient_failure() {
        // Fail the first attempt, succeed on retry
        let db = FailingStateDb::new(1);
        let pending = vec![PendingStateWrite {
            asset_id: "A1".into(),
            version_size: VersionSizeKey::Original,
            download_path: PathBuf::from("/tmp/claude/photo.jpg"),
            local_checksum: "abc".into(),
            download_checksum: None,
        }];
        let failures = flush_pending_state_writes(&db, &pending).await;
        assert_eq!(failures, 0);
        assert_eq!(db.success_count(), 1);
    }

    #[tokio::test]
    async fn flush_pending_state_writes_reports_persistent_failure() {
        // Fail all attempts — must exceed STATE_WRITE_MAX_RETRIES
        let db = FailingStateDb::new(STATE_WRITE_MAX_RETRIES as usize);
        let pending = vec![PendingStateWrite {
            asset_id: "A1".into(),
            version_size: VersionSizeKey::Original,
            download_path: PathBuf::from("/tmp/claude/photo.jpg"),
            local_checksum: "abc".into(),
            download_checksum: None,
        }];
        let failures = flush_pending_state_writes(&db, &pending).await;
        assert_eq!(failures, 1);
        assert_eq!(db.success_count(), 0);
    }

    #[tokio::test]
    async fn flush_pending_state_writes_partial_recovery() {
        // First write exhausts all STATE_WRITE_MAX_RETRIES attempts (reported as failure).
        // Second write fails once more then succeeds on retry.
        let db = FailingStateDb::new(STATE_WRITE_MAX_RETRIES as usize + 1);
        let pending = vec![
            PendingStateWrite {
                asset_id: "A1".into(),
                version_size: VersionSizeKey::Original,
                download_path: PathBuf::from("/tmp/claude/photo1.jpg"),
                local_checksum: "abc".into(),
                download_checksum: None,
            },
            PendingStateWrite {
                asset_id: "A2".into(),
                version_size: VersionSizeKey::Original,
                download_path: PathBuf::from("/tmp/claude/photo2.jpg"),
                local_checksum: "def".into(),
                download_checksum: None,
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
        let db = FailingStateDb::new(2);
        let pending: Vec<PendingStateWrite> = (0..5)
            .map(|i| PendingStateWrite {
                asset_id: format!("ASSET_{i}").into(),
                version_size: VersionSizeKey::Original,
                download_path: PathBuf::from(format!("/tmp/claude/photo_{i}.jpg")),
                local_checksum: format!("ck_{i}"),
                download_checksum: Some(format!("dl_ck_{i}")),
            })
            .collect();

        let failures = flush_pending_state_writes(&db, &pending).await;
        assert_eq!(failures, 0, "all 5 writes should eventually succeed");
        assert_eq!(db.success_count(), 5);
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

    /// NB-1: When a CancellationToken fires during a download pass with
    /// concurrent tasks, the function must return promptly (well within the
    /// Docker stop_grace_period) rather than blocking on the remaining stream.
    #[tokio::test]
    async fn shutdown_cancellation_exits_download_pass_promptly() {
        use crate::download::{DownloadConfig, SyncMode};
        use crate::icloud::photos::PhotoAsset;
        use crate::types::{
            AssetVersionSize, FileMatchPolicy, LivePhotoMode, LivePhotoMovFilenamePolicy,
            RawTreatmentPolicy,
        };
        use futures_util::stream;
        use rustc_hash::FxHashSet;
        use std::time::Instant;

        // Build a slow infinite stream of photo assets — yields one every 50ms.
        // Without cancellation this would run forever.
        let asset_stream = stream::unfold(0u32, |i| async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let asset = TestPhotoAsset::new(&format!("SHUTDOWN_{i}"))
                .orig_size(100)
                .orig_url("http://127.0.0.1:1/photo.jpg")
                .orig_checksum(&format!("ck_{i}"))
                .build();
            Some((Ok(asset) as anyhow::Result<PhotoAsset>, i + 1))
        });

        let dir = TempDir::new().unwrap();

        let config = Arc::new(DownloadConfig {
            directory: dir.path().to_path_buf(),
            folder_structure: "{:%Y/%m/%d}".to_string(),
            size: AssetVersionSize::Original,
            skip_videos: false,
            skip_photos: false,
            skip_created_before: None,
            skip_created_after: None,
            set_exif_datetime: false,
            dry_run: false,
            concurrent_downloads: 10,
            recent: None,
            retry: crate::retry::RetryConfig {
                max_retries: 0,
                base_delay_secs: 0,
                max_delay_secs: 0,
            },
            live_photo_mode: LivePhotoMode::Both,
            live_photo_size: AssetVersionSize::LiveOriginal,
            live_photo_mov_filename_policy: LivePhotoMovFilenamePolicy::Suffix,
            align_raw: RawTreatmentPolicy::Unchanged,
            no_progress_bar: true,
            only_print_filenames: false,
            file_match_policy: FileMatchPolicy::NameSizeDedupWithSuffix,
            force_size: false,
            keep_unicode_in_filenames: false,
            filename_exclude: Vec::new(),
            temp_suffix: ".kei-tmp".to_string(),
            state_db: None,
            retry_only: false,
            max_download_attempts: 10,
            sync_mode: SyncMode::Full,
            album_name: None,
            exclude_asset_ids: Arc::new(FxHashSet::default()),
        });

        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_millis(50))
            .build()
            .expect("client");

        let shutdown_token = CancellationToken::new();
        let token_clone = shutdown_token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            token_clone.cancel();
        });

        let start = Instant::now();
        let result =
            stream_and_download_from_stream(&client, asset_stream, &config, 10_000, shutdown_token)
                .await;
        let elapsed = start.elapsed();

        assert!(
            result.is_ok(),
            "should return Ok after cancellation, got: {result:?}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "should exit promptly after cancellation, took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn test_producer_panic_propagates_as_error() {
        use crate::download::{DownloadConfig, SyncMode};
        use crate::icloud::photos::PhotoAsset;
        use crate::types::{
            AssetVersionSize, FileMatchPolicy, LivePhotoMode, LivePhotoMovFilenamePolicy,
            RawTreatmentPolicy,
        };
        use rustc_hash::FxHashSet;

        let config = Arc::new(DownloadConfig {
            directory: PathBuf::from("/nonexistent/download_filter_tests"),
            folder_structure: "{:%Y/%m/%d}".to_string(),
            size: AssetVersionSize::Original,
            skip_videos: false,
            skip_photos: false,
            skip_created_before: None,
            skip_created_after: None,
            set_exif_datetime: false,
            dry_run: false,
            concurrent_downloads: 1,
            recent: None,
            retry: RetryConfig::default(),
            live_photo_mode: LivePhotoMode::Both,
            live_photo_size: AssetVersionSize::LiveOriginal,
            live_photo_mov_filename_policy: LivePhotoMovFilenamePolicy::Suffix,
            align_raw: RawTreatmentPolicy::Unchanged,
            no_progress_bar: true,
            only_print_filenames: false,
            file_match_policy: FileMatchPolicy::NameSizeDedupWithSuffix,
            force_size: false,
            keep_unicode_in_filenames: false,
            filename_exclude: Vec::new(),
            temp_suffix: ".kei-tmp".to_string(),
            state_db: None,
            retry_only: false,
            max_download_attempts: 10,
            sync_mode: SyncMode::Full,
            album_name: None,
            exclude_asset_ids: Arc::new(FxHashSet::default()),
        });
        let client = reqwest::Client::new();
        let shutdown_token = CancellationToken::new();

        // Stream that panics on first poll — simulates a producer task panic
        let panicking_stream = futures_util::stream::poll_fn(
            |_cx| -> std::task::Poll<Option<anyhow::Result<PhotoAsset>>> {
                panic!("simulated producer panic");
            },
        );

        let err =
            stream_and_download_from_stream(&client, panicking_stream, &config, 0, shutdown_token)
                .await
                .expect_err("should propagate producer panic");
        assert!(
            err.to_string().contains("producer panicked"),
            "Expected producer panic error, got: {err}"
        );
    }
}
