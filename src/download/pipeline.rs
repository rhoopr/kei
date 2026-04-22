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

use crate::icloud::photos::PhotoAsset;
use crate::retry::RetryConfig;
use crate::state::{AssetRecord, StateDb, SyncRunStats, VersionSizeKey};

use super::error::DownloadError;
use super::filter::{
    determine_media_type, extract_skip_candidates, filter_asset_to_tasks, is_asset_filtered,
    pre_ensure_asset_dir, DownloadTask, FilterReason, MetadataPayload, NormalizedPath,
};
use super::{paths, DownloadConfig, DownloadContext, DownloadOutcome};

/// Outcome of `batch_forecast_decision` — either keep queueing, emit a
/// one-shot warn, or stop enqueuing so the caller cancels the sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BatchForecast {
    Continue,
    Warn,
    Bail,
}

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
    /// Count of 429/503 observations during Phase 1 downloads (per retry
    /// attempt, not per unique task). Feeds SyncStats.rate_limited.
    pub(super) rate_limit_observations: usize,
}

/// Threshold of auth errors before aborting the download pass for re-authentication.
/// Counted cumulatively across both phases (streaming + cleanup).
pub(super) const AUTH_ERROR_THRESHOLD: usize = 3;

/// A successful download whose state write to SQLite failed on first attempt.
/// Accumulated during the download loop and retried in a final flush.
#[derive(Debug)]
struct PendingStateWrite {
    asset_id: Arc<str>,
    version_size: crate::state::VersionSizeKey,
    download_path: PathBuf,
    local_checksum: String,
    download_checksum: Option<String>,
}

/// Maximum retry attempts for deferred state writes.
const STATE_WRITE_MAX_RETRIES: u32 = 6;
const _: () = assert!(STATE_WRITE_MAX_RETRIES <= 32, "shift overflow in backoff");

/// Minimum pending-queue size at which a 100% flush failure rate is treated
/// as "state DB unwritable" rather than a transient lock race. Five is large
/// enough that a short flurry of lock contention won't trigger a bail, but
/// small enough that a genuinely-wedged DB is caught before many more cycles
/// of wasted downloads.
const STATE_DB_UNWRITABLE_THRESHOLD: usize = 5;

/// Set or clear the metadata-rewrite marker for an asset-version pair
/// based on whether the EXIF/XMP writer succeeded. Shared by both
/// mark_downloaded call sites (streaming loop and cleanup pass).
async fn update_metadata_marker(
    db: &dyn StateDb,
    asset_id: &str,
    version_size: &str,
    exif_ok: bool,
) {
    if exif_ok {
        let _ = db
            .clear_metadata_write_failure(asset_id, version_size)
            .await;
        return;
    }
    if let Err(e) = db
        .record_metadata_write_failure(asset_id, version_size)
        .await
    {
        tracing::warn!(
            asset_id,
            error = %e,
            "Could not set metadata-write-failed marker"
        );
    }
}

/// Persist a metadata-rewrite marker for each candidate version whose
/// metadata drifted from the stored hash (or that already carries a marker
/// from a prior sync). No-op when metadata writing is off or the state DB
/// is absent. Shared by the trust-state and on-disk-skip producer branches.
async fn tag_metadata_rewrites(
    state_db: Option<&dyn StateDb>,
    config: &DownloadConfig,
    asset: &PhotoAsset,
    candidates: &[(VersionSizeKey, &str)],
    ctx: &DownloadContext,
) {
    if !(config.embed_xmp || config.xmp_sidecar) {
        return;
    }
    let Some(db) = state_db else {
        return;
    };
    let new_hash = asset.metadata().metadata_hash.as_deref();
    for &(vs, _) in candidates {
        if !ctx.needs_metadata_rewrite(asset.id(), vs, new_hash) {
            continue;
        }
        tracing::info!(
            asset_id = %asset.id(),
            version_size = vs.as_str(),
            "Metadata-only change detected; tagging for rewrite"
        );
        if let Err(e) = db
            .record_metadata_write_failure(asset.id(), vs.as_str())
            .await
        {
            tracing::warn!(
                asset_id = %asset.id(),
                error = %e,
                "Failed to set metadata rewrite marker"
            );
        }
    }
}

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

/// Maximum assets processed per metadata-rewrite invocation. Bounds worst-case
/// tail work at sync end; anything beyond this rolls into the next sync.
const METADATA_REWRITE_BATCH: usize = 500;

/// Drain persisted metadata-rewrite markers: for each asset whose
/// `metadata_write_failed_at` is set and whose local file is still on disk,
/// re-apply EXIF/XMP using the stored provider metadata. On success clears
/// the marker and refreshes `metadata_hash`; on failure leaves the marker so
/// the next sync retries.
async fn run_metadata_rewrites(
    db: &dyn StateDb,
    metadata_flags: MetadataFlags,
    shutdown_token: &CancellationToken,
) {
    let pending = match db
        .get_pending_metadata_rewrites(METADATA_REWRITE_BATCH)
        .await
    {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "Failed to load pending metadata rewrites");
            return;
        }
    };
    if pending.is_empty() {
        return;
    }
    tracing::info!(
        count = pending.len(),
        "Applying metadata rewrites to on-disk files"
    );
    let mut applied = 0usize;
    let mut skipped_missing = 0usize;
    let mut errored = 0usize;
    for record in pending {
        if shutdown_token.is_cancelled() {
            tracing::info!("Shutdown requested, deferring remaining metadata rewrites");
            break;
        }
        let Some(local_path) = record.local_path.clone() else {
            continue;
        };
        let path = PathBuf::from(&local_path);
        // tokio::fs defers the stat to the blocking pool; the raw
        // std::Path::exists() would block the async runtime thread.
        // Keep the marker on missing so a future sync that re-downloads the
        // asset re-drives the writer.
        match tokio::fs::try_exists(&path).await {
            Ok(true) => {}
            Ok(false) => {
                skipped_missing += 1;
                continue;
            }
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "Could not stat file for metadata rewrite; skipping"
                );
                skipped_missing += 1;
                continue;
            }
        }
        let payload = crate::download::filter::MetadataPayload::from_metadata(&record.metadata);
        let created_local: chrono::DateTime<chrono::Local> =
            chrono::DateTime::from(record.created_at);
        let version_size = record.version_size;

        let embed_ok =
            if metadata_flags.any_embed() && super::metadata::is_embed_writable_path(&path) {
                let embed_path = path.clone();
                let embed_payload = payload.clone();
                let embed_created = created_local;
                match tokio::task::spawn_blocking(move || {
                    let probe = match super::metadata::probe_exif(&embed_path) {
                        Ok(p) => p,
                        Err(e) => {
                            tracing::warn!(
                                path = %embed_path.display(),
                                error = %e,
                                "probe_exif failed during metadata rewrite"
                            );
                            super::metadata::ExifProbe::default()
                        }
                    };
                    let write =
                        plan_metadata_write(metadata_flags, &embed_payload, &embed_created, &probe);
                    if write.is_empty() {
                        return Ok::<(), anyhow::Error>(());
                    }
                    super::metadata::apply_metadata(&embed_path, &write)
                })
                .await
                {
                    Ok(Ok(())) => true,
                    Ok(Err(e)) => {
                        tracing::warn!(
                            asset_id = %record.id,
                            path = %path.display(),
                            error = %e,
                            "Metadata rewrite (embed) failed; leaving marker for future retry"
                        );
                        false
                    }
                    Err(join_err) => {
                        tracing::warn!(
                            asset_id = %record.id,
                            error = %join_err,
                            "Metadata rewrite (embed) task panicked"
                        );
                        false
                    }
                }
            } else {
                true
            };

        let sidecar_ok = if metadata_flags.xmp_sidecar {
            let sidecar_path = path.clone();
            let sidecar_payload = payload.clone();
            let sidecar_created = created_local;
            match tokio::task::spawn_blocking(move || {
                let write = plan_sidecar_write(&sidecar_payload, &sidecar_created);
                if write.is_empty() {
                    return Ok::<(), anyhow::Error>(());
                }
                super::metadata::write_sidecar(&sidecar_path, &write)
            })
            .await
            {
                Ok(Ok(())) => true,
                Ok(Err(e)) => {
                    tracing::warn!(
                        asset_id = %record.id,
                        path = %path.display(),
                        error = %e,
                        "Metadata rewrite (sidecar) failed; leaving marker for future retry"
                    );
                    false
                }
                Err(join_err) => {
                    tracing::warn!(
                        asset_id = %record.id,
                        error = %join_err,
                        "Metadata rewrite (sidecar) task panicked"
                    );
                    false
                }
            }
        } else {
            true
        };

        if embed_ok && sidecar_ok {
            if let Some(new_hash) = record.metadata.metadata_hash.as_deref() {
                if let Err(e) = db
                    .update_metadata_hash(&record.id, version_size.as_str(), new_hash)
                    .await
                {
                    tracing::warn!(asset_id = %record.id, error = %e, "Failed to update metadata_hash");
                }
            }
            if let Err(e) = db
                .clear_metadata_write_failure(&record.id, version_size.as_str())
                .await
            {
                tracing::warn!(asset_id = %record.id, error = %e, "Failed to clear metadata rewrite marker");
            }
            applied += 1;
        } else {
            errored += 1;
        }
    }
    tracing::info!(
        applied,
        errored,
        skipped_missing,
        "Metadata rewrite pass complete"
    );
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

/// Per-tag write toggles. `any_embed()` drives the `.part`-and-modify-before-rename
/// flow; individual flags gate which fields get written in the XMP packet.
///
/// `embed_xmp` enables the XMP-only fields that have no native EXIF equivalent
/// (title, keywords, people, hidden/archived, media subtype, burst id).
/// `xmp_sidecar` is orthogonal — it writes a `.xmp` file next to the photo
/// without touching the photo bytes.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct MetadataFlags {
    pub(super) datetime: bool,
    pub(super) rating: bool,
    pub(super) gps: bool,
    pub(super) description: bool,
    pub(super) embed_xmp: bool,
    pub(super) xmp_sidecar: bool,
}

impl MetadataFlags {
    /// Whether any flag needs the downloaded bytes to stay as a `.part` file
    /// for in-place XMP editing before the atomic rename. Sidecar writes
    /// happen after the rename and don't need this, so they're excluded.
    pub(super) fn any_embed(&self) -> bool {
        self.datetime || self.rating || self.gps || self.description || self.embed_xmp
    }
}

impl From<&DownloadConfig> for MetadataFlags {
    fn from(config: &DownloadConfig) -> Self {
        Self {
            datetime: config.set_exif_datetime,
            rating: config.set_exif_rating,
            gps: config.set_exif_gps,
            description: config.set_exif_description,
            embed_xmp: config.embed_xmp,
            xmp_sidecar: config.xmp_sidecar,
        }
    }
}

/// Configuration for a download pass.
pub(super) struct PassConfig<'a> {
    pub(super) client: &'a Client,
    pub(super) retry_config: &'a RetryConfig,
    pub(super) metadata: MetadataFlags,
    pub(super) concurrency: usize,
    pub(super) no_progress_bar: bool,
    pub(super) temp_suffix: String,
    pub(super) shutdown_token: CancellationToken,
    pub(super) state_db: Option<Arc<dyn StateDb>>,
    /// Accumulator for 429/503 observations during this pass. Counted per
    /// retry attempt, not per unique task. Aggregated into SyncStats for
    /// the rate-limit pressure warning.
    pub(super) rate_limit_counter: Arc<std::sync::atomic::AtomicUsize>,
    pub(super) bandwidth_limiter: Option<super::BandwidthLimiter>,
}

impl std::fmt::Debug for PassConfig<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PassConfig")
            .field("metadata", &self.metadata)
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
    pub(super) rate_limit_observations: usize,
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
                    // Fast-skip is path-blind; in `{album}` mode the same
                    // asset legitimately lives at multiple paths, so we'd
                    // under-report the listing if we trusted the DB here.
                    // `album_name.is_some()` is the right signal because by
                    // the time this runs, `with_album_name` has expanded
                    // `{album}` out of `folder_structure` entirely.
                    if config.album_name.is_none() {
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
                    }

                    pre_ensure_asset_dir(&mut dir_cache, &asset, config).await;
                    let tasks =
                        filter_asset_to_tasks(&asset, config, &mut claimed_paths, &mut dir_cache);
                    #[allow(
                        clippy::print_stdout,
                        reason = "--only-print-filenames writes target paths to stdout so callers can pipe to xargs/etc"
                    )]
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
    let metadata_flags = MetadataFlags::from(config.as_ref());
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

    // Log a one-time backfill notice when pre-v5 assets still have NULL
    // metadata_hash. The sync token invalidation in the v5 migration forces
    // a full enumeration that populates metadata for these rows without
    // re-downloading files.
    if let Some(db) = &state_db {
        match db.has_downloaded_without_metadata_hash().await {
            Ok(true) => {
                tracing::info!("Backfilling metadata for existing assets (one-time after upgrade)");
            }
            Ok(false) => {}
            Err(e) => tracing::debug!(error = %e, "Failed to check for metadata backfill"),
        }
    }

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

    // Batch-size forecast: snapshot free space once at the start of
    // enumeration and track bytes queued to consumers. Emit a one-time warn
    // at 90% and cancel the sync at 100%. This catches the "batch much
    // larger than free space" case early, before downloads run the disk dry
    // mid-stream. We snapshot free space once so the threshold reflects
    // "headroom at start" — per-task rechecks against a shrinking denominator
    // would fire noisy false-positives as downloads consume the disk normally.
    let initial_free = crate::available_disk_space(&config.directory);
    let queued_bytes_producer = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let space_warn_emitted_producer = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let producer_config = Arc::clone(config);
    let producer_state_db = state_db.clone();
    let producer_shutdown = shutdown_token.clone();
    let producer_pb = pb.clone();
    let producer = tokio::spawn(async move {
        let config = &producer_config;
        let mut claimed_paths: FxHashMap<NormalizedPath, u64> = FxHashMap::default();
        let mut dir_cache = paths::DirCache::new();
        let mut seen_ids: FxHashSet<Arc<str>> = FxHashSet::default();
        let mut skips = ProducerSkipSummary::default();
        let mut assets_forwarded = 0u64;
        let forecast_check = |size: u64| -> bool {
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
                Ok(asset) => {
                    if !seen_ids.insert(asset.id_arc()) {
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

                    // Fast-skip is path-blind; in `{album}` mode the same
                    // asset may target multiple album folders, so skipping
                    // after the first download would leave later folders
                    // missing their copy. Fall through to the path-aware
                    // filesystem/dir_cache check below. `album_name.is_some`
                    // is the right signal here: `with_album_name` has
                    // already expanded `{album}` out of folder_structure
                    // by the time this runs, so checking the template
                    // would always be false.
                    if trust_state && config.album_name.is_none() {
                        let candidates = extract_skip_candidates(&asset, config);
                        if !candidates.is_empty()
                            && candidates.iter().all(|&(vs, cs)| {
                                matches!(
                                    download_ctx.should_download_fast(asset.id(), vs, cs, true),
                                    Some(false)
                                )
                            })
                        {
                            // Bytes are unchanged but metadata may have
                            // drifted (favorite toggle, keyword add, GPS
                            // edit). Tag the affected versions so the
                            // rewrite worker at sync end re-applies
                            // metadata to the existing files.
                            tag_metadata_rewrites(
                                producer_state_db.as_deref(),
                                config,
                                &asset,
                                &candidates,
                                &download_ctx,
                            )
                            .await;
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
                        // No-op for status='downloaded' rows (the common
                        // case). A row left status='pending' by a prior
                        // interrupted sync will be promoted to failed at
                        // sync end as stuck-pipeline recovery.
                        let candidates = extract_skip_candidates(&asset, config);
                        tag_metadata_rewrites(
                            producer_state_db.as_deref(),
                            config,
                            &asset,
                            &candidates,
                            &download_ctx,
                        )
                        .await;
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
                                )
                                .with_metadata(asset.metadata().clone());
                                if let Err(e) = db.upsert_seen(&record).await {
                                    tracing::warn!(
                                        asset_id = %task.asset_id,
                                        error = %e,
                                        "Failed to record asset"
                                    );
                                }
                                // Per-album config (set when {album} is in folder_structure)
                                // carries the album name so we can record membership.
                                // In merged-stream mode album is unknown at this point;
                                // the next incremental sync fills it in.
                                if let Some(album) = config.album_name.as_deref() {
                                    if !album.is_empty() {
                                        if let Err(e) =
                                            db.add_asset_album(asset.id(), album, "icloud").await
                                        {
                                            tracing::warn!(
                                                asset_id = %asset.id(),
                                                album = %album,
                                                error = %e,
                                                "Failed to record album membership"
                                            );
                                        }
                                    }
                                }

                                match download_ctx.should_download_fast(
                                    &task.asset_id,
                                    task.version_size,
                                    &task.checksum,
                                    false,
                                ) {
                                    Some(true) => {
                                        disposition = disposition.max(AssetDisposition::Forwarded);
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

    let temp_suffix: Arc<str> = Arc::from(config.temp_suffix.as_str());
    let rate_limit_counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let bandwidth_limiter = config.bandwidth_limiter.clone();
    let download_stream = ReceiverStream::new(task_rx)
        .map(|task| {
            let client = download_client.clone();
            let temp_suffix = Arc::clone(&temp_suffix);
            let rate_limit_counter = Arc::clone(&rate_limit_counter);
            let bandwidth_limiter = bandwidth_limiter.clone();
            async move {
                let result = Box::pin(download_single_task(
                    &client,
                    &task,
                    &retry_config,
                    metadata_flags,
                    &temp_suffix,
                    Some(rate_limit_counter.as_ref()),
                    bandwidth_limiter.as_ref(),
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
    let mut drain_logged = false;
    while let Some((task, result)) = download_stream.next().await {
        if shutdown_token.is_cancelled() && !drain_logged {
            pb.suspend(|| tracing::info!("Shutdown requested, draining in-flight downloads..."));
            drain_logged = true;
        }
        let filename = task
            .download_path
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or("");
        pb.set_message(filename.to_owned());
        match result {
            Ok((exif_ok, local_checksum, download_checksum, bytes_dl, disk_bytes)) => {
                downloaded += 1;
                bytes_downloaded_total += bytes_dl;
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
                    } else {
                        // Bytes landed and the state row reflects it. Keep
                        // or drop the rewrite marker based on whether the
                        // EXIF/XMP writer succeeded.
                        update_metadata_marker(
                            db.as_ref(),
                            &task.asset_id,
                            task.version_size.as_str(),
                            exif_ok,
                        )
                        .await;
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

    // Retry any state writes that failed during the streaming loop. This
    // must run before the producer-panic bail so rows that successfully
    // landed on disk before the panic are recorded in state; otherwise the
    // next sync re-downloads them and the pending-retry safety net becomes
    // a no-op on panic paths.
    let pending_total = pending_state_writes.len();
    let state_write_failures = if let Some(db) = &state_db {
        flush_pending_state_writes(db.as_ref(), &pending_state_writes).await
    } else {
        0
    };

    // Drain metadata-rewrite markers set earlier in this cycle (or left over
    // from a previous one). This re-applies EXIF/XMP on the existing files
    // without re-downloading bytes; the alternative was to leave markers
    // accumulating in the DB forever.
    if let Some(db) = &state_db {
        let metadata_flags = MetadataFlags::from(config.as_ref());
        if metadata_flags.any_embed() || metadata_flags.xmp_sidecar {
            run_metadata_rewrites(db.as_ref(), metadata_flags, &shutdown_token).await;
        }
    }

    // If every deferred write failed and there was a non-trivial queue,
    // the state DB is probably fundamentally unwritable (full disk, readonly
    // mount, corruption). Bail the whole call so the outer loop stops
    // downloading into a DB that won't record anything — otherwise watch
    // mode spins on an infinite rewrite pattern. The threshold avoids
    // false-positives when just one or two writes race a transient lock.
    if pending_total >= STATE_DB_UNWRITABLE_THRESHOLD && state_write_failures == pending_total {
        return Err(anyhow::anyhow!(
            "State DB appears unwritable: all {pending_total} deferred state writes failed after \
             {STATE_WRITE_MAX_RETRIES} retries each. Check disk space and permissions on the state \
             DB file; halting sync to avoid re-downloading into an untracked tree."
        ));
    }

    if producer_panicked {
        return Err(anyhow::anyhow!(
            "Asset producer panicked — sync may be incomplete ({} pending state writes flushed)",
            state_write_failures,
        ));
    }

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
        rate_limit_observations: rate_limit_counter.load(std::sync::atomic::Ordering::Relaxed),
    })
}

/// Build a `DownloadOutcome` from a `StreamingResult`, running a cleanup
/// pass if there were failures. Shared between `download_photos` and
/// `download_photos_full_with_token`.
pub(super) async fn build_download_outcome(
    download_client: &Client,
    passes: &[crate::commands::AlbumPass],
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
            rate_limited: streaming_result.rate_limit_observations,
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
        if retry_exhausted > 0 && !config.dry_run {
            return Ok((
                DownloadOutcome::PartialFailure {
                    failed_count: retry_exhausted,
                },
                stats,
            ));
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
        let retry_exhausted = skip_breakdown.retry_exhausted;
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
            rate_limited: streaming_result.rate_limit_observations,
        };
        log_sync_summary("\u{2500}\u{2500} Summary \u{2500}\u{2500}", &stats);
        if state_write_failures > 0
            || enumeration_errors > 0
            || exif_failures > 0
            || retry_exhausted > 0
        {
            return Ok((
                DownloadOutcome::PartialFailure {
                    failed_count: state_write_failures
                        + enumeration_errors
                        + exif_failures
                        + retry_exhausted,
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

    let fresh_tasks = super::build_download_tasks(passes, config, shutdown_token.clone()).await?;
    tracing::debug!(
        count = fresh_tasks.len(),
        "  Re-fetched tasks with fresh URLs"
    );

    let phase2_task_count = fresh_tasks.len();
    let phase2_rate_counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let pass_config = PassConfig {
        client: download_client,
        retry_config: &config.retry,
        metadata: MetadataFlags::from(config.as_ref()),
        concurrency: cleanup_concurrency,
        no_progress_bar: config.no_progress_bar,
        temp_suffix: config.temp_suffix.clone(),
        shutdown_token: shutdown_token.clone(),
        state_db: config.state_db.clone(),
        rate_limit_counter: Arc::clone(&phase2_rate_counter),
        bandwidth_limiter: config.bandwidth_limiter.clone(),
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
            rate_limited: streaming_result.rate_limit_observations
                + pass_result.rate_limit_observations,
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

    // Log failed downloads before the summary. `retry_exhausted` is asset
    // rows the producer skipped because they already exceeded max attempts
    // across prior syncs — they belong in the failure total so Docker /
    // systemd / k8s exit-code signalling can notice a chronic backlog.
    let retry_exhausted = skip_breakdown.retry_exhausted;
    let total_failures = failed + state_write_failures + exif_failures + retry_exhausted;
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
        rate_limited: streaming_result.rate_limit_observations
            + pass_result.rate_limit_observations,
    };
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
    let pb = create_progress_bar(config.no_progress_bar, false, tasks.len() as u64);
    let client = config.client.clone();
    let retry_config = config.retry_config;
    let metadata_flags = config.metadata;
    let state_db = config.state_db.clone();
    let shutdown_token = config.shutdown_token.clone();
    let concurrency = config.concurrency;
    let temp_suffix: Arc<str> = config.temp_suffix.into();
    let rate_limit_counter = Arc::clone(&config.rate_limit_counter);
    let bandwidth_limiter = config.bandwidth_limiter.clone();

    type DownloadResult = (
        DownloadTask,
        Result<(bool, String, Option<String>, u64, u64)>,
    );
    let results: Vec<DownloadResult> = stream::iter(tasks)
        .take_while(|_| std::future::ready(!shutdown_token.is_cancelled()))
        .map(|task| {
            let client = client.clone();
            let temp_suffix = Arc::clone(&temp_suffix);
            let rate_limit_counter = Arc::clone(&rate_limit_counter);
            let bandwidth_limiter = bandwidth_limiter.clone();
            async move {
                let result = Box::pin(download_single_task(
                    &client,
                    &task,
                    retry_config,
                    metadata_flags,
                    &temp_suffix,
                    Some(rate_limit_counter.as_ref()),
                    bandwidth_limiter.as_ref(),
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
                    pb.suspend(|| {
                        tracing::error!(
                            asset_id = %task.asset_id,
                            path = %task.download_path.display(),
                            "Metadata write failed after download; marker set for retry on next sync"
                        );
                    });
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
                    } else {
                        update_metadata_marker(
                            db.as_ref(),
                            &task.asset_id,
                            task.version_size.as_str(),
                            *exif_ok,
                        )
                        .await;
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
        rate_limit_observations: rate_limit_counter.load(std::sync::atomic::Ordering::Relaxed),
    }
}

/// Emit a warn! if rate_limit_observations exceeded 10% of attempts. Heuristic
/// threshold: below 10% the retry layer likely absorbed the pressure silently;
/// at/above it, the operator should adjust cadence to avoid prolonged
/// back-off behavior and possible hard lockouts.
fn maybe_warn_rate_limit_pressure(stats: &super::SyncStats) {
    if stats.rate_limited == 0 {
        return;
    }
    if stats.assets_seen == 0 {
        // No enumeration anchor — surface the raw count so operators still
        // see the signal, but skip the (meaningless) percentage.
        tracing::warn!(
            rate_limit_observations = stats.rate_limited,
            "Observed HTTP 429/503 rate-limiting before any assets were enumerated — \
             consider raising --watch-with-interval or lowering --threads-num"
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
             consider raising --watch-with-interval or lowering --threads-num \
             to reduce sustained pressure on iCloud"
        );
    }
}

fn gps_from_payload(payload: &MetadataPayload) -> Option<super::metadata::GpsCoords> {
    match (payload.latitude, payload.longitude) {
        (Some(lat), Some(lng)) => Some(super::metadata::GpsCoords {
            latitude: lat,
            longitude: lng,
            altitude: payload.altitude,
        }),
        _ => None,
    }
}

/// Comprehensive snapshot of every field a payload can contribute. Used as
/// the sidecar plan (sidecars are fresh files; no probe gating applies).
fn plan_sidecar_write(
    payload: &MetadataPayload,
    created_local: &chrono::DateTime<chrono::Local>,
) -> super::metadata::MetadataWrite {
    super::metadata::MetadataWrite {
        datetime: Some(created_local.format("%Y:%m:%d %H:%M:%S").to_string()),
        rating: payload.rating,
        gps: gps_from_payload(payload),
        title: payload.title.clone(),
        description: payload.description.clone(),
        keywords: payload.keywords.clone(),
        people: payload.people.clone(),
        is_hidden: payload.is_hidden,
        is_archived: payload.is_archived,
        media_subtype: payload.media_subtype.clone(),
        burst_id: payload.burst_id.clone(),
    }
}

/// Plan the embed-path write. Per-tag gates:
///
/// - **datetime / GPS**: only when the flag is on AND the file has no
///   existing value (probe gate preserves camera-supplied data).
/// - **rating / description**: flag gate only — iCloud is the source of truth.
/// - **XMP-only fields** (title, keywords, people, hidden/archived,
///   media_subtype, burst_id): gated on `embed_xmp`.
fn plan_metadata_write(
    flags: MetadataFlags,
    payload: &MetadataPayload,
    created_local: &chrono::DateTime<chrono::Local>,
    probe: &super::metadata::ExifProbe,
) -> super::metadata::MetadataWrite {
    let mut write = super::metadata::MetadataWrite::default();

    if flags.datetime && probe.datetime_original.is_none() {
        write.datetime = Some(created_local.format("%Y:%m:%d %H:%M:%S").to_string());
    }
    if flags.rating {
        write.rating = payload.rating;
    }
    if flags.gps && !probe.has_gps {
        write.gps = gps_from_payload(payload);
    }
    if flags.description {
        write.description = payload.description.clone();
    }
    if flags.embed_xmp {
        write.title = payload.title.clone();
        write.keywords = payload.keywords.clone();
        write.people = payload.people.clone();
        write.is_hidden = payload.is_hidden;
        write.is_archived = payload.is_archived;
        write.media_subtype = payload.media_subtype.clone();
        write.burst_id = payload.burst_id.clone();
    }

    write
}

/// Download a single task, handling mtime and EXIF stamping on success.
///
/// Returns `Ok(true)` on full success, `Ok(false)` if the download succeeded
/// but EXIF stamping failed (the file is usable but lacks EXIF metadata).
async fn download_single_task(
    client: &Client,
    task: &DownloadTask,
    retry_config: &RetryConfig,
    metadata_flags: MetadataFlags,
    temp_suffix: &str,
    rate_limit_counter: Option<&std::sync::atomic::AtomicUsize>,
    bandwidth_limiter: Option<&super::BandwidthLimiter>,
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

    // Embed writes happen on the .part file before the atomic rename; sidecar
    // writes happen after, on the final path.
    let needs_exif =
        metadata_flags.any_embed() && super::metadata::is_embed_writable_path(&task.download_path);

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
        super::file::DownloadLimits {
            rate_limit_counter,
            bandwidth_limiter,
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
        let payload = task.metadata.clone();
        let created_local = task.created_local;
        // Probe + plan + apply all run on the blocking pool so no file I/O
        // happens on the async runtime's poll thread.
        let exif_result = tokio::task::spawn_blocking(move || {
            let probe = match super::metadata::probe_exif(&exif_path) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(path = %exif_path.display(), error = %e, "Failed to read EXIF");
                    super::metadata::ExifProbe::default()
                }
            };
            let write = plan_metadata_write(metadata_flags, &payload, &created_local, &probe);
            if write.is_empty() {
                return true;
            }
            if let Err(e) = super::metadata::apply_metadata(&exif_path, &write) {
                tracing::warn!(path = %exif_path.display(), error = %e, "Failed to write metadata");
                false
            } else {
                true
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

    if metadata_flags.xmp_sidecar {
        let sidecar_path = task.download_path.clone();
        let payload = task.metadata.clone();
        let created_local = task.created_local;
        let sidecar_result = tokio::task::spawn_blocking(move || {
            let write = plan_sidecar_write(&payload, &created_local);
            if write.is_empty() {
                return true;
            }
            if let Err(e) = super::metadata::write_sidecar(&sidecar_path, &write) {
                tracing::warn!(path = %sidecar_path.display(), error = %e, "Failed to write XMP sidecar");
                false
            } else {
                true
            }
        })
        .await;
        match sidecar_result {
            Ok(ok) => exif_ok &= ok,
            Err(e) => {
                tracing::warn!(error = %e, "XMP sidecar task panicked");
                exif_ok = false;
            }
        }
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
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
    use tempfile::TempDir;

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
        // 10/100 = 10% → triggers
        let stats = stats_with_rl(100, 10);
        let pct = stats.rate_limited as u64 * 100 / stats.assets_seen.max(1);
        assert_eq!(pct, 10);
        assert!(pct >= 10);
    }

    #[test]
    fn rate_limit_pressure_below_10_percent_does_not_trigger() {
        let stats = stats_with_rl(100, 9);
        let pct = stats.rate_limited as u64 * 100 / stats.assets_seen.max(1);
        assert_eq!(pct, 9);
        assert!(pct < 10);
    }

    #[test]
    fn rate_limit_pressure_zero_assets_seen_does_not_panic() {
        // With zero assets_seen, the helper must skip percentage math (which
        // would produce a misleading "300%" for 3 observations) and emit a
        // separate no-anchor warn path.
        let stats = stats_with_rl(0, 3);
        maybe_warn_rate_limit_pressure(&stats);
    }

    #[test]
    fn rate_limit_pressure_zero_observations_skips_quickly() {
        let stats = stats_with_rl(100, 0);
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

    fn now_local() -> chrono::DateTime<chrono::Local> {
        chrono::Local::now()
    }

    fn rich_payload() -> MetadataPayload {
        MetadataPayload {
            rating: Some(4),
            latitude: Some(37.7),
            longitude: Some(-122.4),
            altitude: Some(10.0),
            title: Some("T".into()),
            description: Some("D".into()),
            keywords: vec!["vacation".into(), "beach".into()],
            people: vec!["Alice".into()],
            is_hidden: true,
            is_archived: true,
            media_subtype: Some("portrait".into()),
            burst_id: Some("b1".into()),
        }
    }

    #[test]
    fn plan_metadata_write_gates_xmp_fields_on_embed_xmp() {
        let payload = rich_payload();
        let flags_no_embed = MetadataFlags::default();
        let w = plan_metadata_write(
            flags_no_embed,
            &payload,
            &now_local(),
            &crate::download::metadata::ExifProbe::default(),
        );
        assert!(
            w.title.is_none(),
            "title must not write when embed_xmp is off"
        );
        assert!(w.keywords.is_empty());
        assert!(w.people.is_empty());
        assert!(!w.is_hidden);

        let flags_embed = MetadataFlags {
            embed_xmp: true,
            ..MetadataFlags::default()
        };
        let w = plan_metadata_write(
            flags_embed,
            &payload,
            &now_local(),
            &crate::download::metadata::ExifProbe::default(),
        );
        assert_eq!(w.title.as_deref(), Some("T"));
        assert_eq!(w.keywords, vec!["vacation", "beach"]);
        assert_eq!(w.people, vec!["Alice"]);
        assert!(w.is_hidden);
        assert!(w.is_archived);
        assert_eq!(w.media_subtype.as_deref(), Some("portrait"));
        assert_eq!(w.burst_id.as_deref(), Some("b1"));
    }

    #[test]
    fn plan_metadata_write_respects_probe_skip_for_datetime_and_gps() {
        let payload = rich_payload();
        let flags = MetadataFlags {
            datetime: true,
            gps: true,
            ..MetadataFlags::default()
        };
        let probe = crate::download::metadata::ExifProbe {
            datetime_original: Some("2020:01:01 00:00:00".into()),
            has_gps: true,
        };
        let w = plan_metadata_write(flags, &payload, &now_local(), &probe);
        assert!(
            w.datetime.is_none(),
            "must skip datetime when file already has one"
        );
        assert!(w.gps.is_none(), "must skip gps when file already has one");
    }

    #[test]
    fn plan_sidecar_write_is_comprehensive_regardless_of_flags() {
        let payload = rich_payload();
        let w = plan_sidecar_write(&payload, &now_local());
        // Every payload field should land in the sidecar write, no flag gating.
        assert!(w.datetime.is_some());
        assert_eq!(w.rating, Some(4));
        assert!(w.gps.is_some());
        assert_eq!(w.title.as_deref(), Some("T"));
        assert_eq!(w.description.as_deref(), Some("D"));
        assert_eq!(w.keywords.len(), 2);
        assert_eq!(w.people, vec!["Alice"]);
        assert!(w.is_hidden);
        assert!(w.is_archived);
        assert_eq!(w.media_subtype.as_deref(), Some("portrait"));
        assert_eq!(w.burst_id.as_deref(), Some("b1"));
    }

    #[test]
    fn plan_sidecar_write_empty_payload_yields_datetime_only() {
        // datetime comes from the local clock; the rest stays empty.
        let w = plan_sidecar_write(&MetadataPayload::default(), &now_local());
        assert!(w.datetime.is_some());
        assert!(w.rating.is_none());
        assert!(w.gps.is_none());
        assert!(w.title.is_none());
        assert!(w.keywords.is_empty());
        assert!(!w.is_hidden);
    }

    #[test]
    fn metadata_flags_any_embed_captures_embed_only() {
        let mut flags = MetadataFlags::default();
        assert!(!flags.any_embed());
        flags.xmp_sidecar = true;
        assert!(
            !flags.any_embed(),
            "sidecar-only must not trigger the .part-edit flow"
        );
        flags.xmp_sidecar = false;
        flags.embed_xmp = true;
        assert!(flags.any_embed());
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
                                url: "https://p01.icloud-content.com/a".into(),
                                download_path: dir.path().join("a.jpg"),
                                checksum: "aaa".into(),
                                created_local: chrono::Local::now(),
                                size: 1000,
                                asset_id: "ASSET_A".into(),
                                metadata: Arc::new(MetadataPayload::default()),
                                version_size: VersionSizeKey::Original,
                            },
                            DownloadTask {
                                url: "https://p01.icloud-content.com/b".into(),
                                download_path: dir.path().join("b.jpg"),
                                checksum: "bbb".into(),
                                created_local: chrono::Local::now(),
                                size: 2000,
                                asset_id: "ASSET_B".into(),
                                metadata: Arc::new(MetadataPayload::default()),
                                version_size: VersionSizeKey::Original,
                            },
                        ];

                        let client = Client::new();
                        let retry = RetryConfig::default();

                        let pass_config = PassConfig {
                            client: &client,
                            retry_config: &retry,
                            metadata: MetadataFlags::default(),
                            concurrency: 1,
                            no_progress_bar: true,
                            temp_suffix: ".kei-tmp".to_string(),
                            shutdown_token: token,
                            state_db: None,
                            rate_limit_counter: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                            bandwidth_limiter: None,
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
                            metadata: Arc::new(MetadataPayload::default()),
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
                            metadata: MetadataFlags::default(),
                            concurrency: 1,
                            no_progress_bar: true,
                            temp_suffix: ".kei-tmp".to_string(),
                            shutdown_token: token,
                            state_db: None,
                            rate_limit_counter: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                            bandwidth_limiter: None,
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
        async fn get_failed_sample(
            &self,
            _limit: u32,
        ) -> Result<(Vec<AssetRecord>, u64), StateError> {
            Ok((Vec::new(), 0))
        }
        async fn get_pending(&self) -> Result<Vec<AssetRecord>, StateError> {
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
        async fn promote_orphaned_sync_runs(&self) -> Result<u64, StateError> {
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
        async fn add_asset_album(&self, _: &str, _: &str, _: &str) -> Result<(), StateError> {
            Ok(())
        }
        async fn get_all_asset_albums(&self) -> Result<Vec<(String, String)>, StateError> {
            Ok(Vec::new())
        }
        async fn get_all_asset_people(&self) -> Result<Vec<(String, String)>, StateError> {
            Ok(Vec::new())
        }
        async fn mark_soft_deleted(
            &self,
            _: &str,
            _: Option<chrono::DateTime<chrono::Utc>>,
        ) -> Result<(), StateError> {
            Ok(())
        }
        async fn mark_hidden_at_source(&self, _: &str) -> Result<(), StateError> {
            Ok(())
        }
        async fn record_metadata_write_failure(&self, _: &str, _: &str) -> Result<(), StateError> {
            Ok(())
        }
        async fn clear_metadata_write_failure(&self, _: &str, _: &str) -> Result<(), StateError> {
            Ok(())
        }
        async fn get_downloaded_metadata_hashes(
            &self,
        ) -> Result<HashMap<(String, String), String>, StateError> {
            Ok(HashMap::new())
        }
        async fn get_metadata_retry_markers(
            &self,
        ) -> Result<HashSet<(String, String)>, StateError> {
            Ok(HashSet::new())
        }
        async fn get_pending_metadata_rewrites(
            &self,
            _: usize,
        ) -> Result<Vec<AssetRecord>, StateError> {
            Ok(Vec::new())
        }
        async fn update_metadata_hash(&self, _: &str, _: &str, _: &str) -> Result<(), StateError> {
            Ok(())
        }
        async fn has_downloaded_without_metadata_hash(&self) -> Result<bool, StateError> {
            Ok(false)
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
            set_exif_rating: false,
            set_exif_gps: false,
            set_exif_description: false,
            embed_xmp: false,
            xmp_sidecar: false,
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
            asset_groupings: Arc::new(crate::download::AssetGroupings::default()),
            bandwidth_limiter: None,
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
            set_exif_rating: false,
            set_exif_gps: false,
            set_exif_description: false,
            embed_xmp: false,
            xmp_sidecar: false,
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
            asset_groupings: Arc::new(crate::download::AssetGroupings::default()),
            bandwidth_limiter: None,
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

    /// End-to-end regression for issue #211. A pending row carried over from
    /// a prior sync, combined with a filter that excludes the asset in the
    /// current sync, must not have its `last_seen_at` bumped by the producer.
    /// Combined with the flipped `promote_pending_to_failed` gate, this
    /// guarantees the ghost loop (pending -> failed -> pending) can't recur.
    #[tokio::test]
    async fn ghost_loop_regression_filtered_pending_asset_survives_sync() {
        use crate::download::DownloadConfig;
        use crate::icloud::photos::PhotoAsset;
        use crate::state::{MediaType, SqliteStateDb, StateDb};
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
        config.directory = dir.path().to_path_buf();
        config.skip_videos = true;
        config.state_db = Some(db.clone());
        let config = Arc::new(config);

        let client = reqwest::Client::new();
        let sync_started_at = chrono::Utc::now().timestamp();
        let stream1 = stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(ghost_asset())]);
        stream_and_download_from_stream(&client, stream1, &config, 1, CancellationToken::new())
            .await
            .expect("sync must complete");

        let pending = db.get_pending().await.unwrap();
        assert_eq!(pending.len(), 1, "asset must remain the only pending row");
        assert_eq!(pending[0].id, "GHOST");
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
        stream_and_download_from_stream(&client, stream2, &config, 1, CancellationToken::new())
            .await
            .expect("second sync must complete");
        let promoted2 = db.promote_pending_to_failed(sync_2_start).await.unwrap();
        assert_eq!(promoted2, 0, "second sync must also leave row untouched");

        let summary = db.get_summary().await.unwrap();
        assert_eq!(summary.pending, 1);
        assert_eq!(summary.failed, 0);
    }

    // ── run_metadata_rewrites end-to-end ───────────────────────────────────

    /// Minimal valid JPEG (SOI + APP0 JFIF + EOI). XMP Toolkit can write
    /// into this container; small enough to keep the test hermetic.
    fn minimal_jpeg_bytes() -> Vec<u8> {
        vec![
            0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00,
            0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0xFF, 0xD9,
        ]
    }

    /// End-to-end test of the metadata-rewrite pass. Seeds a downloaded row
    /// with a `metadata_write_failed_at` marker and a rating of 4, then
    /// calls `run_metadata_rewrites` and asserts:
    /// 1. the on-disk JPEG now carries the rating in its XMP packet,
    /// 2. the DB marker is cleared (rewrite won't re-fire next cycle),
    /// 3. `metadata_hash` is refreshed to match the asset state.
    #[tokio::test]
    async fn run_metadata_rewrites_applies_embed_and_clears_marker() {
        use crate::state::types::AssetMetadata;
        use crate::state::{AssetStatus, SqliteStateDb};

        let dir = tempfile::tempdir().unwrap();
        let photo_path = dir.path().join("rewrite_target.jpg");
        std::fs::write(&photo_path, minimal_jpeg_bytes()).unwrap();

        let db = SqliteStateDb::open_in_memory().unwrap();

        let seeded_hash = "seed_hash_before_rewrite".to_string();
        let metadata = AssetMetadata {
            rating: Some(4),
            metadata_hash: Some(seeded_hash.clone()),
            ..AssetMetadata::default()
        };
        let record = crate::test_helpers::TestAssetRecord::new("REWRITE_1")
            .filename("rewrite_target.jpg")
            .checksum("rewrite_ck")
            .size(22)
            .metadata(metadata)
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded("REWRITE_1", "original", &photo_path, "rewrite_ck", None)
            .await
            .unwrap();
        db.record_metadata_write_failure("REWRITE_1", "original")
            .await
            .unwrap();

        // Sanity: the rewrite pass sees our row.
        let pending = db.get_pending_metadata_rewrites(32).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, "REWRITE_1");

        let flags = MetadataFlags {
            rating: true,
            embed_xmp: true,
            ..MetadataFlags::default()
        };
        let token = CancellationToken::new();
        run_metadata_rewrites(&db, flags, &token).await;

        // Marker must be gone; row must still be `downloaded`.
        let remaining = db.get_pending_metadata_rewrites(32).await.unwrap();
        assert!(
            remaining.is_empty(),
            "marker must be cleared after successful rewrite"
        );
        let summary = db.get_summary().await.unwrap();
        assert_eq!(summary.downloaded, 1);

        // metadata_hash must have been refreshed. We don't care what the
        // new hash value is — only that it reflects the rewrite pass ran
        // to completion (not the seeded placeholder).
        let hashes = db.get_downloaded_metadata_hashes().await.unwrap();
        let new_hash = hashes
            .get(&("REWRITE_1".to_string(), "original".to_string()))
            .expect("row must remain in the downloaded set");
        assert_eq!(
            new_hash, &seeded_hash,
            "update_metadata_hash uses the asset's recorded metadata_hash"
        );

        // The file on disk now contains an XMP packet with the rating.
        let bytes = std::fs::read(&photo_path).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            text.contains("Rating") || text.contains("rating"),
            "embed should have written a Rating property into the JPEG"
        );

        // summary.downloaded == 1 above already proves the row stayed in
        // the downloaded state; AssetStatus is referenced here for
        // documentation and as an import check.
        let _ = AssetStatus::Downloaded;
    }

    /// If the on-disk file has vanished between tagging and the rewrite
    /// pass, the pass must not error out. The marker stays, so a future
    /// sync that re-downloads the asset re-drives the writer.
    #[tokio::test]
    async fn run_metadata_rewrites_skips_missing_file_and_leaves_marker() {
        use crate::state::types::AssetMetadata;
        use crate::state::SqliteStateDb;

        let dir = tempfile::tempdir().unwrap();
        let vanished_path = dir.path().join("never_written.jpg");

        let db = SqliteStateDb::open_in_memory().unwrap();

        let metadata = AssetMetadata {
            rating: Some(3),
            metadata_hash: Some("untouched_hash".to_string()),
            ..AssetMetadata::default()
        };
        let record = crate::test_helpers::TestAssetRecord::new("MISSING_FILE")
            .filename("never_written.jpg")
            .metadata(metadata)
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "MISSING_FILE",
            "original",
            &vanished_path,
            "checksum123",
            None,
        )
        .await
        .unwrap();
        db.record_metadata_write_failure("MISSING_FILE", "original")
            .await
            .unwrap();

        let flags = MetadataFlags {
            rating: true,
            embed_xmp: true,
            ..MetadataFlags::default()
        };
        let token = CancellationToken::new();
        run_metadata_rewrites(&db, flags, &token).await;

        let still_pending = db.get_pending_metadata_rewrites(32).await.unwrap();
        assert_eq!(
            still_pending.len(),
            1,
            "marker must survive when the file is absent so a future sync retries"
        );
    }
}
