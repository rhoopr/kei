//! Full enumeration, bounded streams, album snapshots, and pass-token evidence.

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::{DateTime, Utc};
use futures_util::{Stream, StreamExt, stream};
use reqwest::Client;
use rustc_hash::FxHashSet;
use tokio_util::sync::CancellationToken;

use crate::download::metadata_rewrite;
use crate::download::pipeline::{
    MetadataFlags, StreamRuntime, StreamingResult, build_download_outcome, format_duration,
    stream_and_download_from_stream,
};
use crate::icloud::photos::PhotoAsset;

use super::config::DownloadConfig;
use super::context::{DownloadContext, preload_download_context};
use super::models::{
    DATE_BOUNDED_FULL_ENUMERATION_REASON, DownloadControls, DownloadStore, FullEnumerationReason,
    ICLOUD_ALBUM_COUNT_ERROR_REASON, PassKey, RECENT_LIMITED_FULL_ENUMERATION_REASON,
    RecoveryAction, SyncResult, merge_streaming_result, merge_token_recovery_result,
    set_full_enumeration_reason, sync_token_blocked_explanation, sync_token_blocked_source,
};
use super::selection::{
    build_pass_configs_resolving_deferred_excludes, build_pass_configs_with_download_concurrency,
    deferred_unfiled_index, should_record_album_snapshots,
};

const MAX_SAME_CYCLE_CHECKPOINT_RECOVERY_ROUNDS: usize = 1;

#[derive(Debug)]
struct PerPassStreamingResult {
    pass_index: usize,
    kind: crate::commands::PassKind,
    label: String,
    count: u64,
    elapsed: std::time::Duration,
    token_rx: tokio::sync::oneshot::Receiver<Option<String>>,
    result: StreamingResult,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PassTokenResult {
    Present(String),
    Missing,
    Blank,
    ReceiverDropped,
    EnumerationIncomplete,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PassTokenObservation {
    pass: PassKey,
    result: PassTokenResult,
}

struct PassTokenReceiver {
    pass: PassKey,
    receiver: tokio::sync::oneshot::Receiver<Option<String>>,
    enumeration_complete: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokenGap {
    Missing,
    Blank,
    ReceiverDropped,
    EnumerationIncomplete,
    Mismatch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ZoneTokenEvidence {
    Complete {
        token: String,
    },
    Recoverable {
        passes: Vec<PassKey>,
        reason: TokenGap,
    },
    Incomplete {
        reason: TokenGap,
    },
}

fn classify_zone_token_evidence(observations: &[PassTokenObservation]) -> ZoneTokenEvidence {
    let passes_without_tokens = || {
        observations
            .iter()
            .filter(|observation| !matches!(observation.result, PassTokenResult::Present(_)))
            .map(|observation| observation.pass.clone())
            .collect()
    };

    for (result, reason) in [
        (
            PassTokenResult::EnumerationIncomplete,
            TokenGap::EnumerationIncomplete,
        ),
        (PassTokenResult::ReceiverDropped, TokenGap::ReceiverDropped),
        (PassTokenResult::Blank, TokenGap::Blank),
        (PassTokenResult::Missing, TokenGap::Missing),
    ] {
        if observations.iter().any(|observation| {
            std::mem::discriminant(&observation.result) == std::mem::discriminant(&result)
        }) {
            return ZoneTokenEvidence::Recoverable {
                passes: passes_without_tokens(),
                reason,
            };
        }
    }

    let tokens: Vec<&str> = observations
        .iter()
        .filter_map(|observation| match &observation.result {
            PassTokenResult::Present(token) => Some(token.as_str()),
            _ => None,
        })
        .collect();
    let Some(first) = tokens.first() else {
        return ZoneTokenEvidence::Incomplete {
            reason: TokenGap::Missing,
        };
    };
    if tokens.iter().all(|token| token == first) {
        ZoneTokenEvidence::Complete {
            token: (*first).to_string(),
        }
    } else {
        ZoneTokenEvidence::Recoverable {
            passes: observations
                .iter()
                .map(|observation| observation.pass.clone())
                .collect(),
            reason: TokenGap::Mismatch,
        }
    }
}

type DownloadPhotoStream = Pin<Box<dyn Stream<Item = anyhow::Result<PhotoAsset>> + Send + 'static>>;

const DEFERRED_UNFILED_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

const DEFERRED_UNFILED_HEARTBEAT_ASSETS: u64 = 1_000;

#[derive(Debug)]
struct DeferredUnfiledHeartbeat {
    library: Arc<str>,
    expected_assets: Option<u64>,
    started: Instant,
    last_log: Instant,
    last_logged_assets: u64,
    assets_enumerated: u64,
}

impl DeferredUnfiledHeartbeat {
    fn start(library: Arc<str>, expected_assets: Option<u64>) -> Self {
        let now = Instant::now();
        tracing::info!(
            library = %library,
            pass_type = "unfiled",
            expected_assets = ?expected_assets,
            assets_enumerated = 0_u64,
            "Deferred unfiled enumeration started"
        );
        Self {
            library,
            expected_assets,
            started: now,
            last_log: now,
            last_logged_assets: 0,
            assets_enumerated: 0,
        }
    }

    fn record_asset(&mut self) {
        self.assets_enumerated = self.assets_enumerated.saturating_add(1);
        let now = Instant::now();
        let asset_delta = self
            .assets_enumerated
            .saturating_sub(self.last_logged_assets);
        if asset_delta < DEFERRED_UNFILED_HEARTBEAT_ASSETS
            && now.duration_since(self.last_log) < DEFERRED_UNFILED_HEARTBEAT_INTERVAL
        {
            return;
        }

        self.last_log = now;
        self.last_logged_assets = self.assets_enumerated;
        tracing::info!(
            library = %self.library,
            pass_type = "unfiled",
            assets_enumerated = self.assets_enumerated,
            expected_assets = ?self.expected_assets,
            elapsed = %format_duration(self.started.elapsed()),
            "Deferred unfiled enumeration progress"
        );
    }

    fn complete(&self) {
        tracing::info!(
            library = %self.library,
            pass_type = "unfiled",
            assets_enumerated = self.assets_enumerated,
            expected_assets = ?self.expected_assets,
            elapsed = %format_duration(self.started.elapsed()),
            "Deferred unfiled enumeration complete"
        );
    }
}

fn track_deferred_unfiled_heartbeat(
    stream: DownloadPhotoStream,
    library: Arc<str>,
    expected_assets: Option<u64>,
) -> DownloadPhotoStream {
    let heartbeat = DeferredUnfiledHeartbeat::start(library, expected_assets);
    Box::pin(stream::unfold(
        (stream, heartbeat),
        |(mut stream, mut heartbeat)| async move {
            match stream.next().await {
                Some(item) => {
                    if item.is_ok() {
                        heartbeat.record_asset();
                    }
                    Some((item, (stream, heartbeat)))
                }
                None => {
                    heartbeat.complete();
                    None
                }
            }
        },
    ))
}

fn open_photo_stream_for_controls(
    album: &crate::icloud::photos::PhotoAlbum,
    limit: Option<u32>,
    total_count: Option<u64>,
    fast_concurrency: usize,
    download_concurrency: usize,
    controls: DownloadControls,
    treat_empty_tail_as_error: bool,
) -> (
    DownloadPhotoStream,
    tokio::sync::oneshot::Receiver<Option<String>>,
) {
    if controls.run_mode.is_dry_run() || controls.run_mode.only_print_filenames() {
        album.photo_stream_with_token_policy(
            limit,
            total_count,
            fast_concurrency,
            treat_empty_tail_as_error,
        )
    } else {
        album.photo_stream_with_token_for_download_policy(
            limit,
            total_count,
            download_concurrency,
            treat_empty_tail_as_error,
        )
    }
}

struct RecentFrontier {
    asset_ids: Arc<FxHashSet<String>>,
    oldest_created: Option<DateTime<Utc>>,
}

struct FullPassStreamOptions {
    pass_index: usize,
    controls: DownloadControls,
    count: u64,
    kind: crate::commands::PassKind,
    shutdown_token: CancellationToken,
    download_ctx: Option<Arc<DownloadContext>>,
    album_snapshot: Option<AlbumSnapshotRecorder>,
}

#[derive(Clone)]
struct AlbumSnapshotRecorder {
    db: Arc<dyn DownloadStore>,
    library: Arc<str>,
    container_id: Arc<str>,
    generation: i64,
    write_failed: Arc<AtomicBool>,
}

impl AlbumSnapshotRecorder {
    async fn start_for_pass(
        db: Option<Arc<dyn DownloadStore>>,
        pass: &crate::commands::AlbumPass,
        enum_config_hash: Option<&str>,
    ) -> Result<Option<Self>> {
        if pass.kind != crate::commands::PassKind::Album {
            return Ok(None);
        }
        let Some(db) = db else {
            return Ok(None);
        };
        let Some(container_id) = pass.album.container_id() else {
            tracing::debug!(
                album = %pass.album.name,
                library = %pass.album.zone_name(),
                "Album pass has no container ID; skipping membership snapshot"
            );
            return Ok(None);
        };
        let library = pass.album.zone_name();
        if let Err(e) = db
            .upsert_album_container(library, container_id, &pass.album.name, "album")
            .await
        {
            tracing::warn!(
                album = %pass.album.name,
                library,
                container_id,
                error = %e,
                "Failed to upsert album container; skipping membership snapshot"
            );
            return Err(e.into());
        }
        let generation = match db
            .start_album_membership_snapshot(library, container_id, enum_config_hash)
            .await
        {
            Ok(generation) => generation,
            Err(e) => {
                tracing::warn!(
                    album = %pass.album.name,
                    library,
                    container_id,
                    error = %e,
                    "Failed to start album membership snapshot"
                );
                return Err(e.into());
            }
        };
        Ok(Some(Self {
            db,
            library: Arc::from(library),
            container_id: Arc::from(container_id),
            generation,
            write_failed: Arc::new(AtomicBool::new(false)),
        }))
    }

    async fn record_asset(&self, asset: &PhotoAsset) {
        if let Err(e) = self
            .db
            .add_album_membership_to_snapshot(
                &self.library,
                &self.container_id,
                self.generation,
                asset.asset_record_name(),
                Some(asset.id()),
                "icloud",
            )
            .await
        {
            self.write_failed.store(true, Ordering::Relaxed);
            tracing::warn!(
                asset_id = %asset.id(),
                asset_record_name = %asset.asset_record_name(),
                library = %self.library,
                container_id = %self.container_id,
                generation = self.generation,
                error = %e,
                "Failed to record album membership snapshot row"
            );
        }
    }

    async fn complete_if_clean(&self, result: &mut StreamingResult) {
        if self.write_failed.load(Ordering::Relaxed) {
            result.state_write_failures += 1;
            return;
        }
        if result.enumeration_errors > 0 || !result.enumeration_complete {
            tracing::debug!(
                library = %self.library,
                container_id = %self.container_id,
                generation = self.generation,
                write_failed = self.write_failed.load(Ordering::Relaxed),
                enumeration_errors = result.enumeration_errors,
                enumeration_complete = result.enumeration_complete,
                "Leaving album membership snapshot incomplete"
            );
            return;
        }
        if let Err(e) = self
            .db
            .complete_album_membership_snapshot(&self.library, &self.container_id, self.generation)
            .await
        {
            result.state_write_failures += 1;
            tracing::warn!(
                library = %self.library,
                container_id = %self.container_id,
                generation = self.generation,
                error = %e,
                "Failed to complete album membership snapshot"
            );
        }
    }
}

fn should_use_scope_recent_frontier(passes: &[crate::commands::AlbumPass]) -> bool {
    passes
        .iter()
        .any(|pass| pass.kind != crate::commands::PassKind::Unfiled || !pass.exclude_ids.is_empty())
}

fn should_use_global_recent_frontier(
    passes: &[crate::commands::AlbumPass],
    config: &DownloadConfig,
) -> bool {
    config.recent.is_some()
        && config.recent_scope == crate::cli::RecentScope::Global
        && should_use_scope_recent_frontier(passes)
}

async fn build_recent_frontier(
    passes: &[crate::commands::AlbumPass],
    config: &DownloadConfig,
    controls: DownloadControls,
    shutdown_token: CancellationToken,
) -> Result<Option<RecentFrontier>> {
    let Some(recent) = config.recent else {
        return Ok(None);
    };
    if !should_use_global_recent_frontier(passes, config) {
        return Ok(None);
    }

    let Some(frontier_source) = passes
        .iter()
        .find(|pass| pass.kind == crate::commands::PassKind::Unfiled)
        .map(|pass| pass.album.clone_as_library_wide())
        .or_else(|| {
            passes
                .first()
                .map(|pass| pass.album.clone_as_library_wide())
        })
    else {
        return Ok(None);
    };

    let (stream, _token_rx) = open_photo_stream_for_controls(
        &frontier_source,
        Some(recent),
        None,
        config.concurrent_downloads,
        config.concurrent_downloads,
        controls,
        false,
    );
    tokio::pin!(stream);

    let mut asset_ids = FxHashSet::default();
    let mut oldest_created: Option<DateTime<Utc>> = None;
    while let Some(item) = stream.next().await {
        if shutdown_token.is_cancelled() {
            break;
        }
        let asset = item?;
        let created = asset.created();
        if enumeration_created_lower_bound(config)
            .map(|boundary| created < boundary)
            .unwrap_or(false)
        {
            break;
        }
        oldest_created = Some(oldest_created.map_or(created, |oldest| oldest.min(created)));
        asset_ids.insert(asset.asset_record_name().to_string());
    }
    Ok(Some(RecentFrontier {
        asset_ids: Arc::new(asset_ids),
        oldest_created,
    }))
}

/// Convert a capture-local date lower bound into a conservative UTC
/// enumeration bound.  Capture offsets can move the local midnight by almost
/// a day, so stopping at the unadjusted UTC midnight could omit valid assets.
fn enumeration_created_lower_bound(config: &DownloadConfig) -> Option<DateTime<Utc>> {
    config
        .skip_created_before
        .map(crate::config::CreatedDateFilter::conservative_utc_lower_bound)
}

fn stream_created_lower_bound(
    config: &DownloadConfig,
    frontier: Option<&RecentFrontier>,
) -> Option<DateTime<Utc>> {
    frontier
        .and_then(|frontier| frontier.oldest_created)
        .into_iter()
        .chain(enumeration_created_lower_bound(config))
        .max()
}

fn filter_stream_to_enumeration_bounds(
    stream: DownloadPhotoStream,
    config: &DownloadConfig,
    frontier: Option<&RecentFrontier>,
    bounds_truncated: Arc<AtomicBool>,
) -> DownloadPhotoStream {
    let asset_ids = frontier.map(|frontier| Arc::clone(&frontier.asset_ids));
    let lower_created_bound = stream_created_lower_bound(config, frontier);
    Box::pin(
        stream
            .take_while(move |item| {
                std::future::ready(match item {
                    Ok(asset) => {
                        let in_bounds = lower_created_bound
                            .map(|boundary| asset.created() >= boundary)
                            .unwrap_or(true);
                        if !in_bounds {
                            bounds_truncated.store(true, Ordering::Relaxed);
                        }
                        in_bounds
                    }
                    Err(_) => true,
                })
            })
            .filter_map(move |item| {
                std::future::ready(match item {
                    Ok(asset)
                        if asset_ids
                            .as_ref()
                            .map(|ids| ids.contains(asset.asset_record_name()))
                            .unwrap_or(true) =>
                    {
                        Some(Ok(asset))
                    }
                    Ok(_) => None,
                    Err(e) => Some(Err(e)),
                })
            }),
    )
}

fn scope_frontier_limit(
    config: &DownloadConfig,
    recent_frontier: Option<&RecentFrontier>,
) -> Option<u32> {
    recent_frontier.map_or(config.recent, |_| None)
}

async fn run_full_pass_stream<S>(
    download_client: Client,
    stream: S,
    token_rx: tokio::sync::oneshot::Receiver<Option<String>>,
    pass_config: Arc<DownloadConfig>,
    options: FullPassStreamOptions,
) -> Result<PerPassStreamingResult>
where
    S: futures_util::Stream<Item = anyhow::Result<PhotoAsset>> + Send + 'static,
{
    // Per-album bar: the bar represents only this album's progress,
    // not the cumulative grand total. When the divider is active
    // (multi-pass friendly), the bar plus divider together give the
    // user per-album awareness; the divider's done lines accumulate
    // in scrollback so completed albums don't disappear.
    let pass_start = Instant::now();
    let progress = crate::personality::progress::for_passes(
        options.controls.reporting.no_progress_bar,
        options.controls.run_mode.only_print_filenames(),
        options.count,
        options.controls.reporting.personality_mode,
    );
    let pass_pb = progress.bar;
    let pass_bytes = progress.bytes;

    let snapshot = options.album_snapshot.clone();
    let stream: DownloadPhotoStream = match snapshot {
        Some(recorder) => Box::pin(stream.then(move |item| {
            let recorder = recorder.clone();
            async move {
                if let Ok(asset) = &item {
                    recorder.record_asset(asset).await;
                }
                item
            }
        })),
        None => Box::pin(stream),
    };

    let mut result = stream_and_download_from_stream(
        &download_client,
        stream,
        &pass_config,
        options.controls,
        options.count,
        options.shutdown_token,
        StreamRuntime::with_context(
            Some(pass_pb.clone()),
            Some(std::sync::Arc::clone(&pass_bytes)),
            options.download_ctx,
        )
        .deferring_metadata_drain(),
    )
    .await?;
    if let Some(snapshot) = &options.album_snapshot {
        snapshot.complete_if_clean(&mut result).await;
    }

    let elapsed = pass_start.elapsed();
    pass_pb.finish_and_clear();
    Ok(PerPassStreamingResult {
        pass_index: options.pass_index,
        kind: options.kind,
        label: pass_config.pass_label().to_string(),
        count: options.count,
        elapsed,
        token_rx,
        result,
    })
}

pub(super) async fn download_photos_full_with_reason(
    download_client: &Client,
    passes: &[crate::commands::AlbumPass],
    config: &Arc<DownloadConfig>,
    controls: DownloadControls,
    shutdown_token: CancellationToken,
    reason: FullEnumerationReason,
) -> Result<SyncResult> {
    let mut result =
        download_photos_full_with_token(download_client, passes, config, controls, shutdown_token)
            .await?;
    set_full_enumeration_reason(&mut result, reason);
    Ok(result)
}

/// Fold per-pass `album.len()` results into a `(counts, error_count)` tuple,
/// logging a `warn!` for each failure. Errors are mapped to a count of 0 so
/// downstream progress and concurrency math still has a value. The returned
/// error count is diagnostic only: callers must not turn a failed count
/// side-channel into a semantic completeness bound.
fn fold_pass_count_results(
    results: Vec<anyhow::Result<u64>>,
    passes: &[crate::commands::AlbumPass],
) -> (Vec<u64>, usize) {
    let mut errors: usize = 0;
    let counts: Vec<u64> = results
        .into_iter()
        .zip(passes)
        .map(|(result, pass)| match result {
            Ok(n) => n,
            Err(e) => {
                errors += 1;
                tracing::warn!(
                    album = %pass.album,
                    error = %e,
                    "Failed to query album length; treating count as a display-only \
                     zero and relying on the record stream for completeness"
                );
                0
            }
        })
        .collect();
    (counts, errors)
}

#[derive(Debug)]
struct PassCountPlan {
    display_counts: Vec<u64>,
    stream_total_counts: Vec<Option<u64>>,
    exact_total: Option<u64>,
    len_errors: usize,
}

fn capped_exact_total(counts: &[u64], recent: Option<u32>) -> u64 {
    let total = counts.iter().sum::<u64>();
    total.min(recent.map(u64::from).unwrap_or(u64::MAX))
}

fn display_total_for_recent_scope(counts: &[u64], config: &DownloadConfig) -> u64 {
    match (config.recent, config.recent_scope) {
        (Some(recent), crate::cli::RecentScope::Global) => capped_exact_total(counts, Some(recent)),
        _ => counts.iter().sum(),
    }
}

fn should_skip_pass_count_fetch(config: &DownloadConfig) -> bool {
    // Recent-limited and lower-date-bounded runs deliberately enumerate only a
    // prefix of each newest-first pass. The Hyperion count endpoint reports the
    // full pass size, not how many complete assets will be yielded inside that
    // bounded prefix, so using it as an exact undercount bound false-fires on
    // live accounts with sparse windows. These bounded full syncs suppress the
    // sync token below because they are not complete zone enumerations.
    config.recent.is_some() || config.skip_created_before.is_some()
}

async fn build_pass_count_plan(
    passes: &[crate::commands::AlbumPass],
    config: &DownloadConfig,
    _controls: DownloadControls,
) -> PassCountPlan {
    if should_skip_pass_count_fetch(config) {
        let display_count = config.recent.map(u64::from).unwrap_or(0);
        return PassCountPlan {
            display_counts: vec![display_count; passes.len()],
            stream_total_counts: vec![None; passes.len()],
            exact_total: None,
            len_errors: 0,
        };
    }

    // Album counts share CloudKit's `/internal/records/query/batch`
    // endpoint, so the same-library pass set can fetch all counts with one
    // HTTP call. This matters for default multi-pass syncs and especially
    // `-a all`, where the old per-pass count probe scaled linearly before
    // the first byte of the first download.
    // Capture per-pass `len()` errors separately from stream errors. A failed
    // count endpoint is not itself proof that records/query was incomplete,
    // so token safety below is decided by natural stream completion and
    // sync-token usability instead of a fabricated count bound.
    let pass_albums: Vec<&crate::icloud::photos::PhotoAlbum> =
        passes.iter().map(|pass| &pass.album).collect();
    let pass_count_results = crate::icloud::photos::PhotoAlbum::len_many(&pass_albums).await;
    let (display_counts, len_errors) = fold_pass_count_results(pass_count_results, passes);
    let stream_total_counts = if len_errors > 0 {
        vec![None; passes.len()]
    } else {
        display_counts.iter().copied().map(Some).collect()
    };
    let exact_total = (len_errors == 0).then(|| capped_exact_total(&display_counts, config.recent));

    PassCountPlan {
        display_counts,
        stream_total_counts,
        exact_total,
        len_errors,
    }
}

/// Classification of how the producer-observed asset count compared with the
/// pre-enumeration API total.
///
/// A positive shortfall means the count side-channel claimed there were assets
/// that the producer stream never observed. Duplicate asset IDs can explain
/// some provider count drift because the producer intentionally counts unique
/// assets after duplicate suppression. Any remaining gap is diagnostic-only:
/// the records/query stream, token capture, and write outcomes decide whether
/// the sync token can advance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaginationShortfall {
    Match,
    DuplicateCompensated { shortfall: u64 },
    Shortfall { shortfall: u64 },
}

/// Pure classifier for the pagination-undercount gate. `total` is the
/// pre-enumeration API count (post `--recent` cap and known filters);
/// `unique_seen` is the producer's `assets_seen` count after duplicate asset
/// IDs have been suppressed. Caller is responsible for the `total > 0` guard
/// and any dry-run / print-only suppression.
fn classify_pagination_shortfall(
    total: u64,
    unique_seen: u64,
    duplicate_asset_ids: u64,
) -> PaginationShortfall {
    if unique_seen >= total {
        return PaginationShortfall::Match;
    }

    let raw_seen = unique_seen.saturating_add(duplicate_asset_ids);
    if raw_seen >= total {
        return PaginationShortfall::DuplicateCompensated {
            shortfall: total - unique_seen,
        };
    }

    let shortfall = total - raw_seen;
    PaginationShortfall::Shortfall { shortfall }
}

/// Resolve the zone sync token from every full-enumeration pass that reported
/// one. All passes for a zone must agree before the token can advance; picking
/// the first completed pass would hide snapshot drift between album-scoped
/// enumerations.
/// Full enumeration with syncToken capture.
///
/// Uses `photo_stream_with_token` to capture the zone-level syncToken
/// while running the standard streaming download pipeline. The token
/// is returned alongside the download outcome.
pub(in crate::download) async fn download_photos_full_with_token(
    download_client: &Client,
    passes: &[crate::commands::AlbumPass],
    config: &Arc<DownloadConfig>,
    controls: DownloadControls,
    shutdown_token: CancellationToken,
) -> Result<SyncResult> {
    download_photos_full_with_token_policy(
        download_client,
        passes,
        config,
        controls,
        shutdown_token,
        true,
    )
    .await
}

pub(super) async fn download_photos_full_with_token_policy(
    download_client: &Client,
    passes: &[crate::commands::AlbumPass],
    config: &Arc<DownloadConfig>,
    controls: DownloadControls,
    shutdown_token: CancellationToken,
    repair_token_gaps: bool,
) -> Result<SyncResult> {
    let started = Instant::now();
    let record_album_snapshots = should_record_album_snapshots(passes, config, controls);
    let needs_per_pass = config.requires_per_pass_paths() || record_album_snapshots;

    // Mark every unique zone as in-progress so an interrupted full
    // enumeration leaves a trail the next startup can surface to the
    // operator. Clears once the enumeration returns normally.
    let mut enum_zones: Vec<String> = passes
        .iter()
        .map(|p| p.album.zone_name().to_string())
        .collect();
    enum_zones.sort();
    enum_zones.dedup();
    if let Some(db) = &config.state_db {
        for zone in &enum_zones {
            if let Err(e) = db.begin_enum_progress(zone).await {
                tracing::debug!(error = %e, zone, "Failed to mark enumeration start");
            }
        }
    }

    let pass_count_plan = build_pass_count_plan(passes, config, controls).await;
    let pass_counts = pass_count_plan.display_counts;
    let pass_stream_counts = pass_count_plan.stream_total_counts;
    let mut pagination_counts = pass_counts.clone();
    let mut exact_total = pass_count_plan.exact_total;
    let mut pagination_count_deduction = 0u64;
    let len_errors = pass_count_plan.len_errors;
    let display_total = display_total_for_recent_scope(&pass_counts, config);
    let deferred_unfiled = deferred_unfiled_index(passes);
    let recent_frontier =
        build_recent_frontier(passes, config, controls, shutdown_token.clone()).await?;
    let bounds_truncated = Arc::new(AtomicBool::new(false));
    let strict_empty_tail_errors = config.recent.is_none()
        && config.skip_created_before.is_none()
        && !controls.run_mode.only_print_filenames()
        && !controls.run_mode.is_dry_run();

    // Pass-specific path mode still needs one derived config per pass so
    // `{album}` / `{smart-folder}` / `{library}` expand correctly, but the
    // CloudKit streams are independent. Run those pass streams concurrently
    // instead of serializing round trips across albums. Download workers are
    // divided across active pass pipelines so real downloads do not multiply
    // the user-selected `[download].threads` by the number of albums.
    let (mut streaming_result, token_receivers) = if needs_per_pass {
        let mut combined_result = StreamingResult {
            // Enumeration is "complete" only when every pass finished
            // its stream cleanly. Start optimistic; flip to false on the
            // first pass that ended early (shutdown, channel-close, or
            // panic) so the marker stays set and the next startup logs
            // the interruption.
            enumeration_complete: !passes.is_empty(),
            ..StreamingResult::default()
        };
        let pass_parallelism = passes.len().min(config.concurrent_downloads).max(1);
        // Round down so every set of active passes, including replacement
        // passes, fits the total worker budget. Leave the remainder unused
        // rather than introducing a second worker-admission mechanism.
        let per_pass_download_concurrency = (config.concurrent_downloads / pass_parallelism).max(1);
        let pass_configs = build_pass_configs_with_download_concurrency(
            passes,
            config,
            per_pass_download_concurrency,
        );
        let shared_download_ctx = if controls.run_mode.is_dry_run() {
            None
        } else {
            Some(preload_download_context(config).await)
        };

        // Build per-pass labels for the album divider. Friendly multi-pass
        // syncs print a done line (✓) above the bar after each album
        // completes. Single-pass and off-mode syncs skip the divider.
        let pass_labels: Vec<(&str, u64)> = passes
            .iter()
            .zip(&pass_counts)
            .map(|(pass, &count)| {
                let label: &str = pass.album.name.as_ref();
                let label = if label.is_empty() { "unfiled" } else { label };
                (label, count)
            })
            .collect();
        let divider = crate::personality::album_divider::AlbumDivider::new(
            controls.reporting.personality_mode,
            &pass_labels,
        );

        let deferred_ids = deferred_unfiled
            .map(|_| Arc::new(std::sync::Mutex::new(FxHashSet::<String>::default())));

        let non_unfiled_results = stream::iter(
            passes
                .iter()
                .enumerate()
                .zip(&pass_counts)
                .zip(&pass_stream_counts)
                .zip(pass_configs.iter().cloned())
                .filter(|((((index, _pass), _count), _total_count), _config)| {
                    Some(*index) != deferred_unfiled
                }),
        )
        .map(
            |((((pass_index, pass), &count), total_count), pass_config)| {
                let shutdown_token = shutdown_token.clone();
                let download_client = download_client.clone();
                let deferred_ids = deferred_ids.clone();
                let recent_frontier = recent_frontier.as_ref();
                let download_ctx = shared_download_ctx.clone();
                let bounds_truncated = Arc::clone(&bounds_truncated);
                async move {
                    let album_snapshot = if record_album_snapshots {
                        AlbumSnapshotRecorder::start_for_pass(
                            config.state_db.clone(),
                            pass,
                            config.enum_config_hash.as_deref(),
                        )
                        .await?
                    } else {
                        None
                    };
                    let (stream, token_rx) = open_photo_stream_for_controls(
                        &pass.album,
                        scope_frontier_limit(config, recent_frontier),
                        *total_count,
                        config.concurrent_downloads,
                        pass_config.concurrent_downloads,
                        controls,
                        strict_empty_tail_errors,
                    );
                    let stream = filter_stream_to_enumeration_bounds(
                        stream,
                        config,
                        recent_frontier,
                        Arc::clone(&bounds_truncated),
                    );

                    if pass.kind == crate::commands::PassKind::Album
                        && let Some(deferred_ids) = deferred_ids
                    {
                        let stream = stream.map(move |item| {
                            if let Ok(asset) = &item
                                && let Ok(mut ids) = deferred_ids.lock()
                            {
                                ids.insert(asset.asset_record_name().to_string());
                            }
                            item
                        });
                        return run_full_pass_stream(
                            download_client,
                            stream,
                            token_rx,
                            pass_config,
                            FullPassStreamOptions {
                                pass_index,
                                controls,
                                count,
                                kind: pass.kind,
                                shutdown_token,
                                download_ctx: download_ctx.clone(),
                                album_snapshot,
                            },
                        )
                        .await;
                    }

                    run_full_pass_stream(
                        download_client,
                        stream,
                        token_rx,
                        pass_config,
                        FullPassStreamOptions {
                            pass_index,
                            controls,
                            count,
                            kind: pass.kind,
                            shutdown_token,
                            download_ctx,
                            album_snapshot,
                        },
                    )
                    .await
                }
            },
        )
        .buffer_unordered(pass_parallelism)
        .collect::<Vec<Result<PerPassStreamingResult>>>();

        let pass_results = non_unfiled_results.await;

        let mut token_receivers = Vec::with_capacity(passes.len());
        let mut deferred_exclusions_complete = true;
        for pass_result in pass_results {
            let PerPassStreamingResult {
                pass_index,
                kind,
                label,
                count,
                elapsed,
                token_rx,
                result,
            } = pass_result?;

            if deferred_unfiled.is_some()
                && kind == crate::commands::PassKind::Album
                && (result.enumeration_errors > 0 || !result.enumeration_complete)
            {
                deferred_exclusions_complete = false;
            }
            if deferred_unfiled.is_some()
                && kind == crate::commands::PassKind::Album
                && count > 0
                && result.assets_seen == 0
                && result.enumeration_errors == 0
                && result.enumeration_complete
            {
                // A deferred-unfiled run has one library-wide stream that can
                // cover assets counted by album-side passes. If an album pass
                // contributes a count but no records to this cycle, keeping
                // that count in the summed pagination total compares
                // pass-count bookkeeping against producer-observed records and
                // creates a false shortfall.
                pagination_count_deduction = pagination_count_deduction.saturating_add(count);
            }

            token_receivers.push(PassTokenReceiver {
                pass: PassKey {
                    index: pass_index,
                    kind,
                    label: label.clone(),
                },
                receiver: token_rx,
                enumeration_complete: result.enumeration_complete && result.enumeration_errors == 0,
            });
            let downloaded_u64 = u64::try_from(result.downloaded).unwrap_or(u64::MAX);
            divider.mark_done(&label, downloaded_u64, count, elapsed);

            merge_streaming_result(&mut combined_result, result);
        }

        if let Some(index) = deferred_unfiled {
            if deferred_exclusions_complete {
                let excluded_ids = deferred_ids
                    .as_ref()
                    .and_then(|ids| ids.lock().ok().map(|guard| guard.clone()))
                    .unwrap_or_default();
                if let (Some(pass), Some(pass_config)) =
                    (passes.get(index), pass_configs.get(index).cloned())
                {
                    let stream_total_count = pass_stream_counts.get(index).copied().flatten();
                    let (stream, token_rx) = open_photo_stream_for_controls(
                        &pass.album,
                        scope_frontier_limit(config, recent_frontier.as_ref()),
                        stream_total_count,
                        config.concurrent_downloads,
                        pass_config.concurrent_downloads,
                        controls,
                        strict_empty_tail_errors,
                    );
                    let library = Arc::<str>::from(pass.album.zone_name());
                    let stream = filter_stream_to_enumeration_bounds(
                        stream,
                        config,
                        recent_frontier.as_ref(),
                        Arc::clone(&bounds_truncated),
                    );
                    let stream =
                        track_deferred_unfiled_heartbeat(stream, library, stream_total_count)
                            .filter(move |item| {
                                let keep = item.as_ref().map_or(true, |asset| {
                                    !excluded_ids.contains(asset.asset_record_name())
                                });
                                std::future::ready(keep)
                            });
                    let pass_result = run_full_pass_stream(
                        download_client.clone(),
                        stream,
                        token_rx,
                        pass_config,
                        FullPassStreamOptions {
                            pass_index: index,
                            controls,
                            count: pass_counts.get(index).copied().unwrap_or(0),
                            kind: crate::commands::PassKind::Unfiled,
                            shutdown_token: shutdown_token.clone(),
                            download_ctx: shared_download_ctx.clone(),
                            album_snapshot: None,
                        },
                    )
                    .await?;
                    let filtered_count = pass_result.result.assets_seen;
                    if let Some(slot) = pagination_counts.get_mut(index) {
                        *slot = filtered_count;
                    }
                    let downloaded_u64 =
                        u64::try_from(pass_result.result.downloaded).unwrap_or(u64::MAX);
                    divider.mark_done(
                        &pass_result.label,
                        downloaded_u64,
                        pass_result.count,
                        pass_result.elapsed,
                    );
                    token_receivers.push(PassTokenReceiver {
                        pass: PassKey {
                            index: pass_result.pass_index,
                            kind: pass_result.kind,
                            label: pass_result.label.clone(),
                        },
                        receiver: pass_result.token_rx,
                        enumeration_complete: pass_result.result.enumeration_complete
                            && pass_result.result.enumeration_errors == 0,
                    });
                    merge_streaming_result(&mut combined_result, pass_result.result);
                }
            } else {
                combined_result.enumeration_complete = false;
            }
        }
        divider.finish();

        (combined_result, token_receivers)
    } else {
        let merged_exclude_ids = passes
            .first()
            .map(|p| Arc::clone(&p.exclude_ids))
            .unwrap_or_else(|| Arc::new(FxHashSet::default()));
        let merged_config = if Arc::ptr_eq(&merged_exclude_ids, &config.exclude_asset_ids) {
            Arc::clone(config)
        } else {
            Arc::new(config.with_exclude_ids(merged_exclude_ids))
        };
        let mut token_receivers = Vec::with_capacity(passes.len());
        let streams: Vec<_> = passes
            .iter()
            .enumerate()
            .zip(&pass_stream_counts)
            .map(|((pass_index, pass), total_count)| {
                let (stream, token_rx) = open_photo_stream_for_controls(
                    &pass.album,
                    scope_frontier_limit(config, recent_frontier.as_ref()),
                    *total_count,
                    config.concurrent_downloads,
                    config.concurrent_downloads,
                    controls,
                    strict_empty_tail_errors,
                );
                let label = if pass.album.name.is_empty() {
                    "unfiled".to_string()
                } else {
                    pass.album.name.to_string()
                };
                token_receivers.push(PassTokenReceiver {
                    pass: PassKey {
                        index: pass_index,
                        kind: pass.kind,
                        label,
                    },
                    receiver: token_rx,
                    enumeration_complete: true,
                });
                filter_stream_to_enumeration_bounds(
                    stream,
                    config,
                    recent_frontier.as_ref(),
                    Arc::clone(&bounds_truncated),
                )
            })
            .collect();

        let combined = stream::select_all(streams);
        // Merged-stream branch already runs as a single call, so it creates
        // one bar internally; no shared-bar plumbing needed.
        let result = stream_and_download_from_stream(
            download_client,
            combined,
            &merged_config,
            controls,
            display_total,
            shutdown_token.clone(),
            StreamRuntime::new(None, None).deferring_metadata_drain(),
        )
        .await?;

        (result, token_receivers)
    };

    // Count-probe failures stay diagnostic unless the primary records/query
    // stream also proves unsafe. Do not fold them into `enumeration_errors`
    // or force `enumeration_complete = false`; otherwise a flaky count
    // endpoint can trap users in repeated full enumeration after CloudKit
    // delivered a naturally drained stream and a usable token.
    if exact_total.is_some() {
        exact_total = Some(
            capped_exact_total(&pagination_counts, config.recent)
                .saturating_sub(pagination_count_deduction),
        );
    }
    let api_total_at_start = if len_errors == 0
        && config.recent.is_none()
        && config.skip_created_before.is_none()
        && !controls.run_mode.only_print_filenames()
        && !controls.run_mode.is_dry_run()
    {
        exact_total
    } else {
        None
    };

    // Check if enumeration saw significantly fewer assets than the API reported.
    // The count side-channel can include assets outside the stream's effective
    // scope, so a mismatch is diagnostic-only. Token advancement is still gated
    // below by the records/query stream completing naturally, the returned
    // syncToken being usable and unanimous, and the download/state outcome
    // proving all streamed work was handled.
    let mut pagination_shortfall_assets = 0u64;
    let mut pagination_shortfall_warnings = 0usize;
    let count_lookup_failed = len_errors > 0;
    if !count_lookup_failed
        && !controls.run_mode.only_print_filenames()
        && !controls.run_mode.is_dry_run()
        && let Some(total) = exact_total.filter(|total| *total > 0)
    {
        let duplicate_asset_ids =
            u64::try_from(streaming_result.skip_summary.duplicates).unwrap_or(u64::MAX);
        let decision =
            classify_pagination_shortfall(total, streaming_result.assets_seen, duplicate_asset_ids);
        match decision {
            PaginationShortfall::Match => {}
            PaginationShortfall::DuplicateCompensated { shortfall } => {
                tracing::warn!(
                    expected = total,
                    seen = streaming_result.assets_seen,
                    shortfall,
                    duplicate_asset_ids,
                    "Enumeration count shortfall was explained by duplicate asset IDs; \
                         continuing sync token capture"
                );
            }
            PaginationShortfall::Shortfall { shortfall } => {
                pagination_shortfall_assets = shortfall;
                pagination_shortfall_warnings = 1;
                tracing::warn!(
                    expected = total,
                    seen = streaming_result.assets_seen,
                    duplicate_asset_ids,
                    shortfall,
                    "Enumeration saw fewer assets than the count side-channel reported; \
                         recording diagnostic and continuing sync token capture"
                );
            }
        }
    }

    // Collect the sync token from every album's token receiver and require
    // agreement before advancing. In practice, all passes for a zone should
    // report the same token; disagreement means the full enumeration did not
    // observe one coherent snapshot.
    // Don't advance the token for read-only operations or when the producer
    // stream was incomplete (would permanently skip missed assets).
    // Bounded runs are eligible to report a token only when the producer can
    // prove the bound did not actually truncate the stream. A count-limited
    // stream suppresses its token when it hits the cap, and lower-date-bound /
    // global-recent filtering marks `bounds_truncated` when it stops before
    // natural EOF.
    let token_attempt_allowed =
        !controls.run_mode.only_print_filenames() && !controls.run_mode.is_dry_run();
    let mut token_block_reason: Option<&'static str> = None;
    let mut token_expected_receivers: Option<usize> = None;
    let mut token_receivers_with_token: Option<usize> = None;
    let mut token_receivers_missing: Option<usize> = None;
    let mut token_receivers_blank: Option<usize> = None;
    let mut token_receivers_dropped: Option<usize> = None;
    let mut token_unique_values: Option<usize> = None;
    let mut same_cycle_recovery_attempts = 0usize;
    let mut same_cycle_recovery_successes = 0usize;
    let mut checkpoint_retry_passes = Vec::new();
    let sync_token = if token_attempt_allowed && streaming_result.provider_auth_errors == 0 {
        let expected_token_count = token_receivers.len();
        token_expected_receivers = Some(expected_token_count);
        let mut observations = Vec::with_capacity(expected_token_count);
        for receiver in token_receivers {
            let result = if !receiver.enumeration_complete {
                let _ = receiver.receiver.await;
                PassTokenResult::EnumerationIncomplete
            } else {
                match receiver.receiver.await {
                    Ok(Some(token)) if token.trim().is_empty() => PassTokenResult::Blank,
                    Ok(Some(token)) => PassTokenResult::Present(token.trim().to_string()),
                    Ok(None) => PassTokenResult::Missing,
                    Err(_) => PassTokenResult::ReceiverDropped,
                }
            };
            observations.push(PassTokenObservation {
                pass: receiver.pass,
                result,
            });
        }
        let tokens: Vec<&str> = observations
            .iter()
            .filter_map(|observation| match &observation.result {
                PassTokenResult::Present(token) => Some(token.as_str()),
                _ => None,
            })
            .collect();
        let missing_tokens = observations
            .iter()
            .filter(|observation| matches!(observation.result, PassTokenResult::Missing))
            .count();
        let blank_tokens = observations
            .iter()
            .filter(|observation| matches!(observation.result, PassTokenResult::Blank))
            .count();
        let dropped_receivers = observations
            .iter()
            .filter(|observation| matches!(observation.result, PassTokenResult::ReceiverDropped))
            .count();
        let unique_token_count = tokens.iter().copied().collect::<FxHashSet<_>>().len();
        token_receivers_with_token = Some(tokens.len());
        token_receivers_missing = Some(missing_tokens);
        token_receivers_blank = Some(blank_tokens);
        token_receivers_dropped = Some(dropped_receivers);
        token_unique_values = Some(unique_token_count);
        let bounded_stream_truncated = bounds_truncated.load(Ordering::Relaxed);
        let repair_entire_enumeration = (!streaming_result.enumeration_complete
            || streaming_result.enumeration_errors > 0)
            && !bounded_stream_truncated;
        let initial_evidence = if bounded_stream_truncated {
            ZoneTokenEvidence::Incomplete {
                reason: TokenGap::EnumerationIncomplete,
            }
        } else if repair_entire_enumeration {
            ZoneTokenEvidence::Recoverable {
                passes: observations
                    .iter()
                    .map(|observation| observation.pass.clone())
                    .collect(),
                reason: TokenGap::EnumerationIncomplete,
            }
        } else {
            classify_zone_token_evidence(&observations)
        };
        match initial_evidence {
            ZoneTokenEvidence::Complete { token } => Some(token),
            ZoneTokenEvidence::Recoverable { passes, reason } if !repair_token_gaps => {
                checkpoint_retry_passes = passes;
                token_block_reason = Some(match reason {
                    TokenGap::ReceiverDropped => "kei_internal_token_receiver_dropped",
                    TokenGap::Blank => "icloud_blank_sync_token",
                    TokenGap::Mismatch => "icloud_sync_token_mismatch",
                    TokenGap::Missing | TokenGap::EnumerationIncomplete => {
                        "icloud_sync_token_missing"
                    }
                });
                None
            }
            ZoneTokenEvidence::Recoverable {
                reason,
                passes: recovery_passes,
            } => {
                let recovery_action = RecoveryAction::RetryPasses(recovery_passes);
                tracing::warn!(
                    reason = ?reason,
                    passes = ?recovery_action.retry_passes(),
                    "Provider checkpoint preserved: pass proof is incomplete; retrying only affected passes now"
                );
                same_cycle_recovery_attempts = MAX_SAME_CYCLE_CHECKPOINT_RECOVERY_ROUNDS;
                let recovery_configs =
                    match build_pass_configs_resolving_deferred_excludes(passes, config).await {
                        Ok(configs) => configs,
                        Err(error) => {
                            tracing::warn!(
                                error = %error,
                                "Same-cycle pass recovery could not rebuild pass exclusions"
                            );
                            Vec::new()
                        }
                    };
                let mut recovery_enumeration_complete = true;
                let mut recovery_enumeration_errors = 0usize;
                let expected_recovery_passes = recovery_action.retry_passes().len();
                let mut completed_recovery_passes = 0usize;
                for pass_key in recovery_action.retry_passes().iter().cloned() {
                    let Some(pass) = passes.get(pass_key.index) else {
                        recovery_enumeration_complete = false;
                        continue;
                    };
                    let Some(pass_config) = recovery_configs.get(pass_key.index).cloned() else {
                        recovery_enumeration_complete = false;
                        continue;
                    };
                    let total_count = pass_stream_counts.get(pass_key.index).copied().flatten();
                    let (stream, token_rx) = open_photo_stream_for_controls(
                        &pass.album,
                        scope_frontier_limit(config, recent_frontier.as_ref()),
                        total_count,
                        config.concurrent_downloads,
                        pass_config.concurrent_downloads,
                        controls,
                        strict_empty_tail_errors,
                    );
                    let stream = filter_stream_to_enumeration_bounds(
                        stream,
                        config,
                        recent_frontier.as_ref(),
                        Arc::clone(&bounds_truncated),
                    );
                    let recovered = run_full_pass_stream(
                        download_client.clone(),
                        stream,
                        token_rx,
                        pass_config,
                        FullPassStreamOptions {
                            pass_index: pass_key.index,
                            controls,
                            count: pass_counts.get(pass_key.index).copied().unwrap_or(0),
                            kind: pass.kind,
                            shutdown_token: shutdown_token.clone(),
                            download_ctx: None,
                            album_snapshot: None,
                        },
                    )
                    .await;
                    let recovered = match recovered {
                        Ok(recovered) => recovered,
                        Err(error) => {
                            recovery_enumeration_complete = false;
                            recovery_enumeration_errors =
                                recovery_enumeration_errors.saturating_add(1);
                            if let Some(observation) = observations
                                .iter_mut()
                                .find(|observation| observation.pass.index == pass_key.index)
                            {
                                observation.result = PassTokenResult::EnumerationIncomplete;
                            }
                            tracing::warn!(
                                pass = ?pass_key,
                                error = %error,
                                "Same-cycle pass recovery did not complete"
                            );
                            continue;
                        }
                    };
                    completed_recovery_passes += 1;
                    let recovered_result = if !recovered.result.enumeration_complete
                        || recovered.result.enumeration_errors > 0
                    {
                        let _ = recovered.token_rx.await;
                        PassTokenResult::EnumerationIncomplete
                    } else {
                        match recovered.token_rx.await {
                            Ok(Some(token)) if token.trim().is_empty() => PassTokenResult::Blank,
                            Ok(Some(token)) => PassTokenResult::Present(token.trim().to_string()),
                            Ok(None) => PassTokenResult::Missing,
                            Err(_) => PassTokenResult::ReceiverDropped,
                        }
                    };
                    recovery_enumeration_complete =
                        recovery_enumeration_complete && recovered.result.enumeration_complete;
                    recovery_enumeration_errors = recovery_enumeration_errors
                        .saturating_add(recovered.result.enumeration_errors);
                    if let Some(observation) = observations
                        .iter_mut()
                        .find(|observation| observation.pass.index == pass_key.index)
                    {
                        observation.result = recovered_result;
                    }
                    merge_token_recovery_result(&mut streaming_result, recovered.result);
                }

                if completed_recovery_passes != expected_recovery_passes {
                    recovery_enumeration_complete = false;
                    recovery_enumeration_errors = recovery_enumeration_errors.saturating_add(
                        expected_recovery_passes.saturating_sub(completed_recovery_passes),
                    );
                }

                if repair_entire_enumeration {
                    streaming_result.enumeration_complete = recovery_enumeration_complete;
                    streaming_result.enumeration_errors = recovery_enumeration_errors;
                }

                let final_tokens: Vec<&str> = observations
                    .iter()
                    .filter_map(|observation| match &observation.result {
                        PassTokenResult::Present(token) => Some(token.as_str()),
                        _ => None,
                    })
                    .collect();
                token_receivers_with_token = Some(final_tokens.len());
                token_receivers_missing = Some(
                    observations
                        .iter()
                        .filter(|observation| {
                            matches!(observation.result, PassTokenResult::Missing)
                        })
                        .count(),
                );
                token_receivers_blank = Some(
                    observations
                        .iter()
                        .filter(|observation| matches!(observation.result, PassTokenResult::Blank))
                        .count(),
                );
                token_receivers_dropped = Some(
                    observations
                        .iter()
                        .filter(|observation| {
                            matches!(observation.result, PassTokenResult::ReceiverDropped)
                        })
                        .count(),
                );
                token_unique_values =
                    Some(final_tokens.iter().copied().collect::<FxHashSet<_>>().len());

                match classify_zone_token_evidence(&observations) {
                    ZoneTokenEvidence::Complete { token }
                        if streaming_result.enumeration_complete
                            && streaming_result.enumeration_errors == 0 =>
                    {
                        same_cycle_recovery_successes = 1;
                        tracing::info!(
                            passes = ?observations.iter().map(|observation| &observation.pass).collect::<Vec<_>>(),
                            "Provider checkpoint proof repaired in the current cycle"
                        );
                        Some(token)
                    }
                    ZoneTokenEvidence::Complete { .. } => {
                        token_block_reason = Some("icloud_sync_token_missing");
                        None
                    }
                    ZoneTokenEvidence::Recoverable { passes, reason } => {
                        checkpoint_retry_passes = passes;
                        token_block_reason = Some(match reason {
                            TokenGap::ReceiverDropped => "kei_internal_token_receiver_dropped",
                            TokenGap::Blank => "icloud_blank_sync_token",
                            TokenGap::Mismatch => "icloud_sync_token_mismatch",
                            TokenGap::Missing | TokenGap::EnumerationIncomplete => {
                                "icloud_sync_token_missing"
                            }
                        });
                        None
                    }
                    ZoneTokenEvidence::Incomplete { reason } => {
                        checkpoint_retry_passes = observations
                            .iter()
                            .map(|observation| observation.pass.clone())
                            .collect();
                        token_block_reason = Some(match reason {
                            TokenGap::ReceiverDropped => "kei_internal_token_receiver_dropped",
                            TokenGap::Blank => "icloud_blank_sync_token",
                            TokenGap::Mismatch => "icloud_sync_token_mismatch",
                            TokenGap::Missing | TokenGap::EnumerationIncomplete => {
                                "icloud_sync_token_missing"
                            }
                        });
                        None
                    }
                }
            }
            ZoneTokenEvidence::Incomplete { reason } => {
                checkpoint_retry_passes = observations
                    .iter()
                    .map(|observation| observation.pass.clone())
                    .collect();
                token_block_reason = Some(match reason {
                    TokenGap::ReceiverDropped => "kei_internal_token_receiver_dropped",
                    TokenGap::Blank => "icloud_blank_sync_token",
                    TokenGap::Mismatch => "icloud_sync_token_mismatch",
                    TokenGap::Missing | TokenGap::EnumerationIncomplete => {
                        "icloud_sync_token_missing"
                    }
                });
                None
            }
        }
    } else {
        None
    };
    let token_eligible = token_attempt_allowed
        && streaming_result.enumeration_complete
        && streaming_result.enumeration_errors == 0;

    // Capture the enumeration-complete signal before
    // `build_download_outcome` consumes `streaming_result`. The marker
    // gate below uses this signal directly so a partial-failure run
    // whose enumeration phase finished still clears the marker.
    let enumeration_complete = streaming_result.enumeration_complete;
    let enumeration_errors = streaming_result.enumeration_errors;
    let tail_probes = if config.recent.is_none() && config.skip_created_before.is_none() {
        pass_stream_counts
            .iter()
            .filter(|count| count.is_some())
            .count()
    } else {
        0
    };
    let count_undercount_assets = exact_total
        .map(|count| streaming_result.assets_seen.saturating_sub(count))
        .unwrap_or(0);

    // Both enumeration branches defer their rewrite queue to here, so a full
    // enumeration drains exactly once. Per-pass pipelines run concurrently and
    // separate drains can otherwise write the same file, where the later write
    // wins whether or not it holds the newer metadata.
    let metadata_flags = MetadataFlags::from(config.as_ref());
    if controls.run_mode.downloads_files()
        && !config.refresh_metadata
        && metadata_flags.has_any_write()
        && let Some(db) = &config.state_db
    {
        streaming_result.exif_failures += metadata_rewrite::run_pending(
            db.as_ref(),
            metadata_flags,
            Arc::clone(&config.temp_suffix),
            &shutdown_token,
        )
        .await
        .failed;
    }

    // Build the outcome using the same logic as download_photos
    let (outcome, mut stats) = build_download_outcome(
        download_client,
        passes,
        config,
        controls,
        streaming_result,
        started,
        shutdown_token,
    )
    .await?;
    stats.pagination_shortfall_warnings = pagination_shortfall_warnings;
    stats.pagination_shortfall_assets = pagination_shortfall_assets;
    stats.tail_probes = tail_probes;
    stats.count_undercount_assets = count_undercount_assets;
    stats.count_probe_failures = len_errors;
    stats.api_total_at_start = api_total_at_start;
    stats.same_cycle_recovery_attempts = same_cycle_recovery_attempts;
    stats.same_cycle_recovery_successes = same_cycle_recovery_successes;
    stats.checkpoint_retry_passes = checkpoint_retry_passes;
    if token_attempt_allowed {
        stats.sync_token_expected_receivers = token_expected_receivers;
        stats.sync_token_receivers_with_token = token_receivers_with_token;
        stats.sync_token_receivers_missing = token_receivers_missing;
        stats.sync_token_receivers_blank = token_receivers_blank;
        stats.sync_token_receivers_dropped = token_receivers_dropped;
        stats.sync_token_unique_values = token_unique_values;
    }
    let bounded_run = config.recent.is_some() || config.skip_created_before.is_some();
    let bounded_limit_truncated = bounded_run
        && (bounds_truncated.load(Ordering::Relaxed)
            || (token_eligible
                && sync_token.is_none()
                && matches!(
                    token_block_reason,
                    Some("icloud_sync_token_missing")
                        | Some("kei_internal_token_receiver_dropped")
                        | Some("sync_token_unavailable")
                        | None
                )));
    if count_lookup_failed && token_eligible && sync_token.is_some() {
        if !stats.sync_token_blocked {
            tracing::warn!(
                count_probe_failures = len_errors,
                "Count probes failed, but records/query completed naturally with a usable \
                 sync token; recording diagnostic and allowing token advancement"
            );
        }
    } else if bounded_limit_truncated
        && !controls.run_mode.only_print_filenames()
        && !controls.run_mode.is_dry_run()
        && enumeration_errors == 0
    {
        let bounded_reason = if config.recent.is_some() {
            RECENT_LIMITED_FULL_ENUMERATION_REASON
        } else {
            DATE_BOUNDED_FULL_ENUMERATION_REASON
        };
        stats.sync_token_blocked = true;
        stats.sync_token_blocked_reason = Some(bounded_reason);
        stats.sync_token_blocked_source = Some(sync_token_blocked_source(bounded_reason));
        stats.sync_token_blocked_explanation = Some(sync_token_blocked_explanation(bounded_reason));
    } else if token_eligible && sync_token.is_none() {
        let reason = token_block_reason.unwrap_or("sync_token_unavailable");
        stats.sync_token_blocked = true;
        stats.sync_token_blocked_reason = Some(reason);
        stats.sync_token_blocked_source = Some(sync_token_blocked_source(reason));
        stats.sync_token_blocked_explanation = Some(sync_token_blocked_explanation(reason));
    } else if count_lookup_failed && !stats.sync_token_blocked {
        stats.sync_token_blocked = true;
        stats.sync_token_blocked_reason = Some(ICLOUD_ALBUM_COUNT_ERROR_REASON);
        stats.sync_token_blocked_source =
            Some(sync_token_blocked_source(ICLOUD_ALBUM_COUNT_ERROR_REASON));
        stats.sync_token_blocked_explanation = Some(sync_token_blocked_explanation(
            ICLOUD_ALBUM_COUNT_ERROR_REASON,
        ));
    } else if token_attempt_allowed
        && sync_token.is_none()
        && let Some(reason) = token_block_reason
    {
        stats.sync_token_blocked = true;
        stats.sync_token_blocked_reason = Some(reason);
        stats.sync_token_blocked_source = Some(sync_token_blocked_source(reason));
        stats.sync_token_blocked_explanation = Some(sync_token_blocked_explanation(reason));
    }

    // Clear enumeration-in-progress markers when the producer reached the
    // natural end of the API stream. The gate ignores download-side
    // failures so a partial-failure cycle whose enumeration finished
    // doesn't leave the marker set forever. Shutdown still suppresses the
    // clear because the producer's cancellation path leaves
    // `enumeration_complete = false`.
    if enumeration_complete && let Some(db) = &config.state_db {
        for zone in &enum_zones {
            if let Err(e) = db.end_enum_progress(zone).await {
                tracing::debug!(error = %e, zone, "Failed to clear enumeration marker");
            }
        }
    }

    Ok(SyncResult {
        outcome,
        sync_token,
        stats,
        full_enumeration_ran: true,
    })
}

#[cfg(test)]
mod tests;
