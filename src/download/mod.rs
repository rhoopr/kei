//! Download engine — streaming pipeline that starts downloading as soon as
//! the first API page returns, rather than enumerating the entire library
//! upfront. Uses a two-phase approach: (1) stream-and-download with bounded
//! concurrency, then (2) cleanup pass with fresh CDN URLs for any failures.

pub mod error;
pub mod exif;
pub mod file;
pub mod paths;

use std::collections::HashMap;
use std::fs::FileTimes;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use chrono::{DateTime, Local, Utc};
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::Client;

use std::io::IsTerminal;
use std::path::PathBuf;

use futures_util::stream::{self, StreamExt};
use tokio_util::sync::CancellationToken;

use crate::icloud::photos::types::AssetVersion;
use crate::icloud::photos::{AssetItemType, AssetVersionSize, PhotoAlbum};
use crate::retry::RetryConfig;
use crate::types::{FileMatchPolicy, LivePhotoMovFilenamePolicy, RawTreatmentPolicy};

use error::DownloadError;

/// Normalize a path for collision detection on case-insensitive filesystems.
///
/// On macOS and Windows, filesystems are typically case-insensitive, meaning
/// `IMG_0996.mov` and `IMG_0996.MOV` are the same file. This function converts
/// paths to lowercase for use as HashMap keys to detect such collisions.
///
/// On Linux (case-sensitive filesystem), paths are returned unchanged.
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn normalize_path_for_collision(path: &Path) -> PathBuf {
    PathBuf::from(path.to_string_lossy().to_ascii_lowercase())
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn normalize_path_for_collision(path: &Path) -> PathBuf {
    path.to_path_buf()
}

/// Outcome of a download pass.
#[derive(Debug)]
pub enum DownloadOutcome {
    /// All downloads completed successfully.
    Success,
    /// Session expired mid-sync; caller should re-authenticate and retry.
    SessionExpired { auth_error_count: usize },
    /// Some downloads failed (not session-related).
    PartialFailure { failed_count: usize },
}

/// Subset of application config consumed by the download engine.
/// Decoupled from CLI parsing so the engine can be tested independently.
#[derive(Debug)]
pub struct DownloadConfig {
    pub(crate) directory: std::path::PathBuf,
    pub(crate) folder_structure: String,
    pub(crate) size: AssetVersionSize,
    pub(crate) skip_videos: bool,
    pub(crate) skip_photos: bool,
    pub(crate) skip_created_before: Option<DateTime<Utc>>,
    pub(crate) skip_created_after: Option<DateTime<Utc>>,
    pub(crate) set_exif_datetime: bool,
    pub(crate) dry_run: bool,
    pub(crate) concurrent_downloads: usize,
    pub(crate) recent: Option<u32>,
    pub(crate) retry: RetryConfig,
    pub(crate) skip_live_photos: bool,
    pub(crate) live_photo_size: AssetVersionSize,
    pub(crate) live_photo_mov_filename_policy: LivePhotoMovFilenamePolicy,
    pub(crate) align_raw: RawTreatmentPolicy,
    pub(crate) no_progress_bar: bool,
    pub(crate) file_match_policy: FileMatchPolicy,
}

/// A unit of work produced by the filter phase and consumed by the download phase.
#[derive(Debug)]
struct DownloadTask {
    url: String,
    download_path: PathBuf,
    checksum: String,
    created_local: DateTime<Local>,
    size: u64,
}

/// Eagerly enumerate all albums and build a complete task list.
///
/// Used only by the Phase 2 cleanup pass — re-contacts the API so each call
/// yields fresh CDN URLs that haven't expired during a long download session.
async fn build_download_tasks(
    albums: &[PhotoAlbum],
    config: &DownloadConfig,
    shutdown_token: CancellationToken,
) -> Result<Vec<DownloadTask>> {
    let album_results: Vec<Result<Vec<_>>> = stream::iter(albums)
        .take_while(|_| std::future::ready(!shutdown_token.is_cancelled()))
        .map(|album| async move { album.photos(config.recent).await })
        .buffer_unordered(config.concurrent_downloads)
        .collect()
        .await;

    let mut tasks: Vec<DownloadTask> = Vec::new();
    let mut claimed_paths: HashMap<PathBuf, u64> = HashMap::new();
    for album_result in album_results {
        let assets = album_result?;

        for asset in &assets {
            tasks.extend(filter_asset_to_tasks(asset, config, &mut claimed_paths));
        }
    }

    Ok(tasks)
}

/// Apply the RAW alignment policy by swapping Original and Alternative versions
/// when appropriate, matching Python's `apply_raw_policy()`.
fn apply_raw_policy(
    versions: &HashMap<AssetVersionSize, AssetVersion>,
    policy: RawTreatmentPolicy,
) -> std::borrow::Cow<'_, HashMap<AssetVersionSize, AssetVersion>> {
    if policy == RawTreatmentPolicy::AsIs {
        return std::borrow::Cow::Borrowed(versions);
    }

    let alt = match versions.get(&AssetVersionSize::Alternative) {
        Some(v) => v,
        None => return std::borrow::Cow::Borrowed(versions),
    };

    let should_swap = match policy {
        RawTreatmentPolicy::AsOriginal => alt.asset_type.contains("raw"),
        RawTreatmentPolicy::AsAlternative => versions
            .get(&AssetVersionSize::Original)
            .is_some_and(|v| v.asset_type.contains("raw")),
        RawTreatmentPolicy::AsIs => false,
    };

    if !should_swap {
        return std::borrow::Cow::Borrowed(versions);
    }

    let mut swapped = versions.clone();
    let orig = swapped.remove(&AssetVersionSize::Original);
    let alt = swapped.remove(&AssetVersionSize::Alternative);
    if let Some(o) = orig {
        swapped.insert(AssetVersionSize::Alternative, o);
    }
    if let Some(a) = alt {
        swapped.insert(AssetVersionSize::Original, a);
    }
    std::borrow::Cow::Owned(swapped)
}

/// Apply content filters (type, date range) and local existence check,
/// producing download tasks for assets that need fetching.
/// Returns up to two tasks: the primary photo/video and an optional live photo MOV.
///
/// The `claimed_paths` map tracks paths that have been claimed by earlier tasks
/// in the same download session, preventing race conditions where two assets
/// with the same filename both see "file doesn't exist" during concurrent downloads.
fn filter_asset_to_tasks(
    asset: &crate::icloud::photos::PhotoAsset,
    config: &DownloadConfig,
    claimed_paths: &mut HashMap<PathBuf, u64>,
) -> Vec<DownloadTask> {
    if config.skip_videos && asset.item_type() == Some(AssetItemType::Movie) {
        return Vec::new();
    }
    if config.skip_photos && asset.item_type() == Some(AssetItemType::Image) {
        return Vec::new();
    }

    let created_utc = asset.created();
    if let Some(before) = &config.skip_created_before {
        if created_utc < *before {
            return Vec::new();
        }
    }
    if let Some(after) = &config.skip_created_after {
        if created_utc > *after {
            return Vec::new();
        }
    }

    let filename = match asset.filename() {
        Some(f) => f,
        None => {
            tracing::warn!("Asset {} has no filename, skipping", asset.id());
            return Vec::new();
        }
    };

    let created_local: DateTime<Local> = created_utc.with_timezone(&Local);
    let download_path = paths::local_download_path(
        &config.directory,
        &config.folder_structure,
        &created_local,
        filename,
    );

    let versions = apply_raw_policy(asset.versions(), config.align_raw);
    let mut tasks = Vec::new();

    if let Some(version) = versions.get(&config.size) {
        // Determine the final download path, applying size-based deduplication if needed.
        // Check both on-disk files AND in-flight downloads (claimed_paths) to handle
        // concurrent downloads of assets with the same filename.
        let final_path = if download_path.exists() {
            match config.file_match_policy {
                FileMatchPolicy::NameSizeDedupWithSuffix => {
                    // If file exists with different size, download with size suffix
                    let on_disk_size = std::fs::metadata(&download_path)
                        .map(|m| m.len())
                        .unwrap_or(0);
                    if on_disk_size == version.size {
                        // Same size — likely already downloaded, skip.
                        None
                    } else {
                        // Different size — deduplicate by appending file size to filename.
                        let dedup_filename = paths::add_dedup_suffix(filename, version.size);
                        let dedup_path = paths::local_download_path(
                            &config.directory,
                            &config.folder_structure,
                            &created_local,
                            &dedup_filename,
                        );
                        let dedup_normalized = normalize_path_for_collision(&dedup_path);
                        if dedup_path.exists() || claimed_paths.contains_key(&dedup_normalized) {
                            None // deduped version already downloaded or claimed
                        } else {
                            tracing::debug!(
                                "File collision: {} already exists with different size (on-disk: {}, expected: {}), using {}",
                                download_path.display(),
                                on_disk_size,
                                version.size,
                                dedup_path.display(),
                            );
                            Some(dedup_path)
                        }
                    }
                }
                FileMatchPolicy::NameId7 => {
                    // name-id7 policy adds asset ID to ALL filenames, not just collisions.
                    // If the file exists, it's already downloaded, skip.
                    None
                }
            }
        } else if let Some(&claimed_size) =
            claimed_paths.get(&normalize_path_for_collision(&download_path))
        {
            // Path is claimed by an in-flight download — check for size collision.
            // Use normalized paths for collision detection to handle case-insensitive
            // filesystems (macOS, Windows) where IMG.mov and IMG.MOV are the same file.
            match config.file_match_policy {
                FileMatchPolicy::NameSizeDedupWithSuffix => {
                    if claimed_size == version.size {
                        // Same size — likely duplicate asset, skip.
                        None
                    } else {
                        // Different size — deduplicate by appending file size to filename.
                        let dedup_filename = paths::add_dedup_suffix(filename, version.size);
                        let dedup_path = paths::local_download_path(
                            &config.directory,
                            &config.folder_structure,
                            &created_local,
                            &dedup_filename,
                        );
                        let dedup_normalized = normalize_path_for_collision(&dedup_path);
                        if dedup_path.exists() || claimed_paths.contains_key(&dedup_normalized) {
                            None // deduped version already downloaded or claimed
                        } else {
                            tracing::debug!(
                                "In-flight collision: {} claimed with different size (claimed: {}, expected: {}), using {}",
                                download_path.display(),
                                claimed_size,
                                version.size,
                                dedup_path.display(),
                            );
                            Some(dedup_path)
                        }
                    }
                }
                FileMatchPolicy::NameId7 => None,
            }
        } else {
            Some(download_path.clone())
        };

        if let Some(path) = final_path {
            claimed_paths.insert(normalize_path_for_collision(&path), version.size);
            tasks.push(DownloadTask {
                url: version.url.clone(),
                download_path: path,
                checksum: version.checksum.clone(),
                created_local,
                size: version.size,
            });
        }
    }

    // Live photo MOV companion — only for images
    if !config.skip_live_photos && asset.item_type() == Some(AssetItemType::Image) {
        if let Some(live_version) = versions.get(&config.live_photo_size) {
            let mov_filename = match config.live_photo_mov_filename_policy {
                LivePhotoMovFilenamePolicy::Suffix => paths::live_photo_mov_path_suffix(filename),
                LivePhotoMovFilenamePolicy::Original => {
                    paths::live_photo_mov_path_original(filename)
                }
            };
            let mov_path = paths::local_download_path(
                &config.directory,
                &config.folder_structure,
                &created_local,
                &mov_filename,
            );
            // If the path already exists (on disk or claimed), it may be a different
            // file (e.g. a regular video) that collides with the live photo companion
            // name. Detect this by comparing sizes; on mismatch, deduplicate using
            // the asset ID.
            //
            // Use normalized paths for collision detection to handle case-insensitive
            // filesystems (macOS, Windows) where IMG.mov and IMG.MOV are the same file.
            let mov_normalized = normalize_path_for_collision(&mov_path);
            let final_mov_path = if mov_path.exists() {
                let on_disk_size = std::fs::metadata(&mov_path).map(|m| m.len()).unwrap_or(0);
                if on_disk_size == live_version.size {
                    // Same size — likely already downloaded, skip.
                    None
                } else {
                    // Collision with a different file — deduplicate.
                    let dedup_filename = paths::insert_suffix(&mov_filename, asset.id());
                    let dedup_path = paths::local_download_path(
                        &config.directory,
                        &config.folder_structure,
                        &created_local,
                        &dedup_filename,
                    );
                    let dedup_normalized = normalize_path_for_collision(&dedup_path);
                    if dedup_path.exists() || claimed_paths.contains_key(&dedup_normalized) {
                        None // deduped version already downloaded or claimed
                    } else {
                        tracing::debug!(
                            "Live photo MOV collision: {} already exists with different size, using {}",
                            mov_path.display(),
                            dedup_path.display(),
                        );
                        Some(dedup_path)
                    }
                }
            } else if let Some(&claimed_size) = claimed_paths.get(&mov_normalized) {
                // Path is claimed by an in-flight download
                if claimed_size == live_version.size {
                    None // Same size, likely duplicate
                } else {
                    // Collision with in-flight download — deduplicate.
                    let dedup_filename = paths::insert_suffix(&mov_filename, asset.id());
                    let dedup_path = paths::local_download_path(
                        &config.directory,
                        &config.folder_structure,
                        &created_local,
                        &dedup_filename,
                    );
                    let dedup_normalized = normalize_path_for_collision(&dedup_path);
                    if dedup_path.exists() || claimed_paths.contains_key(&dedup_normalized) {
                        None
                    } else {
                        tracing::debug!(
                            "Live photo MOV in-flight collision: {} claimed, using {}",
                            mov_path.display(),
                            dedup_path.display(),
                        );
                        Some(dedup_path)
                    }
                }
            } else {
                Some(mov_path)
            };
            if let Some(path) = final_mov_path {
                claimed_paths.insert(normalize_path_for_collision(&path), live_version.size);
                tasks.push(DownloadTask {
                    url: live_version.url.clone(),
                    download_path: path,
                    checksum: live_version.checksum.clone(),
                    created_local,
                    size: live_version.size,
                });
            }
        }
    }

    tasks
}

/// Create a progress bar with a consistent template.
///
/// Returns `ProgressBar::hidden()` when the user passed `--no-progress-bar` or
/// stdout is not a TTY (e.g. piped output, cron jobs) — this prevents output
/// corruption and honours the user's preference.
fn create_progress_bar(no_progress_bar: bool, total: u64) -> ProgressBar {
    if no_progress_bar || !std::io::stdout().is_terminal() {
        return ProgressBar::hidden();
    }
    let pb = ProgressBar::new(total);
    pb.set_style(
        ProgressStyle::with_template(
            "[{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({eta}) {msg}",
        )
        .expect("valid template")
        .progress_chars("=> "),
    );
    pb
}

/// Threshold of auth errors before aborting the download pass for re-authentication.
/// Counted cumulatively across both phases (streaming + cleanup).
const AUTH_ERROR_THRESHOLD: usize = 3;

/// Result of the streaming download phase.
#[derive(Debug)]
struct StreamingResult {
    downloaded: usize,
    failed: Vec<DownloadTask>,
    auth_errors: usize,
}

/// Streaming download pipeline — merges per-album streams and pipes assets
/// directly into the download loop as they arrive from the API.
///
/// Eliminates the startup delay of full-library enumeration: the first
/// download begins as soon as the first API page returns. Each album's
/// background task prefetches the next page via a channel buffer, so API
/// latency overlaps with download I/O.
///
/// Returns `StreamingResult` containing download counts, failed tasks, and
/// auth error count. When auth errors exceed the threshold, the function
/// returns early to allow re-authentication.
async fn stream_and_download(
    download_client: &Client,
    albums: &[PhotoAlbum],
    config: &DownloadConfig,
    shutdown_token: CancellationToken,
) -> Result<StreamingResult> {
    // Lightweight count-only API query (HyperionIndexCountLookup) — separate
    // from the page-by-page photo fetch, used to size the progress bar.
    // When --recent is set, cap to that limit since the stream will stop early.
    //
    // Note: the total reflects *photo count*, but each photo may produce
    // multiple download tasks (e.g. live photo MOV companions, RAW
    // alternates). The bar may therefore overshoot pos > len slightly.
    // This matches Python icloudpd's tqdm behavior and keeps the ETA useful.
    let mut total: u64 = 0;
    for album in albums {
        total += album.len().await.unwrap_or(0);
    }
    if let Some(recent) = config.recent {
        total = total.min(recent as u64);
    }
    let pb = create_progress_bar(config.no_progress_bar, total);

    // select_all interleaves across albums so no single large album
    // starves others; each stream's background task provides prefetch.
    let album_streams: Vec<_> = albums
        .iter()
        .map(|album| album.photo_stream(config.recent))
        .collect();

    let mut combined = stream::select_all(album_streams);

    // Track paths claimed by in-flight downloads to detect collisions between
    // assets with the same filename processed in the same session.
    let mut claimed_paths: HashMap<PathBuf, u64> = HashMap::new();

    if config.dry_run {
        let mut count = 0usize;
        while let Some(result) = combined.next().await {
            if shutdown_token.is_cancelled() {
                tracing::info!("Shutdown requested, stopping dry run");
                break;
            }
            let asset = result?;
            let tasks = filter_asset_to_tasks(&asset, config, &mut claimed_paths);
            for task in &tasks {
                tracing::info!("[DRY RUN] Would download {}", task.download_path.display());
            }
            count += tasks.len();
        }
        return Ok(StreamingResult {
            downloaded: count,
            failed: Vec::new(),
            auth_errors: 0,
        });
    }

    let download_client = download_client.clone();
    let retry_config = config.retry;
    let set_exif = config.set_exif_datetime;
    let concurrency = config.concurrent_downloads;

    let mut downloaded = 0usize;
    let mut failed: Vec<DownloadTask> = Vec::new();
    let mut auth_errors = 0usize;

    // Collect all download tasks first, processing assets sequentially to ensure
    // proper collision detection via claimed_paths. This trades some streaming
    // benefit for correctness when multiple assets share the same filename.
    let mut all_tasks: Vec<DownloadTask> = Vec::new();
    while let Some(result) = combined.next().await {
        if shutdown_token.is_cancelled() {
            break;
        }
        match result {
            Ok(asset) => {
                let tasks = filter_asset_to_tasks(&asset, config, &mut claimed_paths);
                if tasks.is_empty() {
                    pb.inc(1);
                } else {
                    all_tasks.extend(tasks);
                }
            }
            Err(e) => {
                pb.suspend(|| tracing::error!("Error fetching asset: {}", e));
            }
        }
    }

    // Now download all tasks concurrently
    let task_stream = stream::iter(all_tasks);

    let download_stream = task_stream
        .map(|task| {
            let client = download_client.clone();
            async move {
                let result = download_single_task(&client, &task, &retry_config, set_exif).await;
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
            Ok(()) => downloaded += 1,
            Err(e) => {
                // Check if this is a session expiry error
                if let Some(download_err) = e.downcast_ref::<DownloadError>() {
                    if download_err.is_session_expired() {
                        auth_errors += 1;
                        pb.suspend(|| {
                            tracing::warn!(
                                "Auth error ({}/{}): {} - {}",
                                auth_errors,
                                AUTH_ERROR_THRESHOLD,
                                task.download_path.display(),
                                e
                            );
                        });
                        if auth_errors >= AUTH_ERROR_THRESHOLD {
                            pb.suspend(|| {
                                tracing::warn!(
                                    "Auth error threshold reached, aborting for re-authentication"
                                );
                            });
                            // Use break instead of return to allow buffered tasks to
                            // complete cleanly, matching graceful shutdown behavior.
                            break;
                        }
                        failed.push(task);
                        pb.inc(1);
                        continue;
                    }
                }
                pb.suspend(|| {
                    tracing::error!("Download failed: {}: {}", task.download_path.display(), e);
                });
                failed.push(task);
            }
        }
        pb.inc(1);
    }

    pb.finish_and_clear();

    Ok(StreamingResult {
        downloaded,
        failed,
        auth_errors,
    })
}

/// Entry point for the download engine.
///
/// Phase 1: Stream assets page-by-page and download immediately with bounded
/// concurrency — no upfront enumeration delay.
///
/// Phase 2 (cleanup): Re-fetch from the API to get fresh CDN URLs (the
/// originals may have expired during a long Phase 1) and retry failures at
/// reduced concurrency to give large files full bandwidth.
///
/// Returns `DownloadOutcome` indicating whether all downloads succeeded,
/// the session expired (requiring re-auth), or some downloads failed.
pub async fn download_photos(
    download_client: &Client,
    albums: &[PhotoAlbum],
    config: &DownloadConfig,
    shutdown_token: CancellationToken,
) -> Result<DownloadOutcome> {
    let started = Instant::now();

    let streaming_result =
        stream_and_download(download_client, albums, config, shutdown_token.clone()).await?;

    let downloaded = streaming_result.downloaded;
    let failed_tasks = streaming_result.failed;
    let auth_errors = streaming_result.auth_errors;

    // If auth errors exceeded threshold, return early for re-authentication
    if auth_errors >= AUTH_ERROR_THRESHOLD {
        return Ok(DownloadOutcome::SessionExpired {
            auth_error_count: auth_errors,
        });
    }

    if downloaded == 0 && failed_tasks.is_empty() {
        if config.dry_run {
            tracing::info!("── Dry Run Summary ──");
            tracing::info!("  0 files would be downloaded");
            tracing::info!("  destination: {}", config.directory.display());
        } else {
            tracing::info!("No new photos to download");
        }
        return Ok(DownloadOutcome::Success);
    }

    if config.dry_run {
        tracing::info!("── Dry Run Summary ──");
        if shutdown_token.is_cancelled() {
            tracing::info!(
                "  Interrupted — scanned {} files before shutdown",
                downloaded
            );
        } else {
            tracing::info!("  {} files would be downloaded", downloaded);
        }
        tracing::info!("  destination: {}", config.directory.display());
        tracing::info!("  concurrency: {}", config.concurrent_downloads);
        return Ok(DownloadOutcome::Success);
    }

    let total = downloaded + failed_tasks.len();

    if failed_tasks.is_empty() {
        tracing::info!("── Summary ──");
        tracing::info!("  {} downloaded, 0 failed, {} total", total, total);
        tracing::info!("  elapsed: {}", format_duration(started.elapsed()));
        return Ok(DownloadOutcome::Success);
    }

    // Phase 2: CDN URLs from Phase 1 may have expired during a long
    // download session. Re-fetch the full task list for fresh URLs and
    // retry at concurrency 1 to give large files full bandwidth.
    let cleanup_concurrency = 1;
    let failure_count = failed_tasks.len();
    tracing::info!(
        "── Cleanup pass: re-fetching URLs and retrying {} failed downloads (concurrency: {}) ──",
        failure_count,
        cleanup_concurrency,
    );

    let fresh_tasks = build_download_tasks(albums, config, shutdown_token.clone()).await?;
    tracing::info!("  Re-fetched {} tasks with fresh URLs", fresh_tasks.len());

    let phase2_task_count = fresh_tasks.len();
    let pass_result = run_download_pass(
        download_client,
        fresh_tasks,
        &config.retry,
        config.set_exif_datetime,
        cleanup_concurrency,
        config.no_progress_bar,
        shutdown_token,
    )
    .await;

    let remaining_failed = pass_result.failed;
    let phase2_auth_errors = pass_result.auth_errors;
    let total_auth_errors = auth_errors + phase2_auth_errors;

    // If auth errors exceeded threshold during phase 2, return for re-auth
    if total_auth_errors >= AUTH_ERROR_THRESHOLD {
        return Ok(DownloadOutcome::SessionExpired {
            auth_error_count: total_auth_errors,
        });
    }

    let failed = remaining_failed.len();
    let phase2_succeeded = phase2_task_count - failed;
    let succeeded = downloaded + phase2_succeeded;
    let final_total = succeeded + failed;
    tracing::info!("── Summary ──");
    tracing::info!(
        "  {} downloaded, {} failed, {} total",
        succeeded,
        failed,
        final_total
    );
    tracing::info!("  elapsed: {}", format_duration(started.elapsed()));

    if failed > 0 {
        for task in &remaining_failed {
            tracing::error!("Download failed: {}", task.download_path.display());
        }
        return Ok(DownloadOutcome::PartialFailure {
            failed_count: failed,
        });
    }

    Ok(DownloadOutcome::Success)
}

/// Result of a download pass.
#[derive(Debug)]
struct PassResult {
    failed: Vec<DownloadTask>,
    auth_errors: usize,
}

/// Execute a download pass over the given tasks, returning any that failed.
async fn run_download_pass(
    client: &Client,
    tasks: Vec<DownloadTask>,
    retry_config: &RetryConfig,
    set_exif: bool,
    concurrency: usize,
    no_progress_bar: bool,
    shutdown_token: CancellationToken,
) -> PassResult {
    let pb = create_progress_bar(no_progress_bar, tasks.len() as u64);
    let client = client.clone();

    let results: Vec<(DownloadTask, Result<()>)> = stream::iter(tasks)
        .take_while(|_| std::future::ready(!shutdown_token.is_cancelled()))
        .map(|task| {
            let client = client.clone();
            async move {
                let result = download_single_task(&client, &task, retry_config, set_exif).await;
                (task, result)
            }
        })
        .buffer_unordered(concurrency)
        .collect()
        .await;

    let mut failed: Vec<DownloadTask> = Vec::new();
    let mut auth_errors = 0usize;

    for (task, result) in results {
        if let Err(e) = &result {
            // Check if this is a session expiry error
            if let Some(download_err) = e.downcast_ref::<DownloadError>() {
                if download_err.is_session_expired() {
                    auth_errors += 1;
                    pb.suspend(|| {
                        tracing::warn!("Auth error: {} - {}", task.download_path.display(), e);
                    });
                }
            } else {
                pb.suspend(|| {
                    tracing::error!("Download failed: {}: {}", task.download_path.display(), e);
                });
            }
            failed.push(task);
        }
        pb.inc(1);
    }

    pb.finish_and_clear();
    PassResult {
        failed,
        auth_errors,
    }
}

/// Download a single task, handling mtime and EXIF stamping on success.
async fn download_single_task(
    client: &Client,
    task: &DownloadTask,
    retry_config: &RetryConfig,
    set_exif: bool,
) -> Result<()> {
    if let Some(parent) = task.download_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    tracing::debug!(
        size_bytes = task.size,
        path = %task.download_path.display(),
        "downloading",
    );

    file::download_file(
        client,
        &task.url,
        &task.download_path,
        &task.checksum,
        false,
        retry_config,
    )
    .await?;

    let mtime_path = task.download_path.clone();
    let ts = task.created_local.timestamp();
    if let Err(e) = tokio::task::spawn_blocking(move || set_file_mtime(&mtime_path, ts)).await? {
        tracing::warn!(
            "Could not set mtime on {}: {}",
            task.download_path.display(),
            e
        );
    }

    tracing::debug!("Downloaded {}", task.download_path.display());

    if set_exif {
        let ext = task
            .download_path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        if matches!(ext.to_ascii_lowercase().as_str(), "jpg" | "jpeg") {
            let exif_path = task.download_path.clone();
            let date_str = task.created_local.format("%Y:%m:%d %H:%M:%S").to_string();
            let exif_result =
                tokio::task::spawn_blocking(move || match exif::get_photo_exif(&exif_path) {
                    Ok(None) => {
                        if let Err(e) = exif::set_photo_exif(&exif_path, &date_str) {
                            tracing::warn!("Failed to set EXIF on {}: {}", exif_path.display(), e);
                        }
                    }
                    Ok(Some(_)) => {}
                    Err(e) => {
                        tracing::warn!("Failed to read EXIF from {}: {}", exif_path.display(), e);
                    }
                })
                .await;
            if let Err(e) = exif_result {
                tracing::warn!("EXIF task panicked: {}", e);
            }
        }
    }

    Ok(())
}

fn format_duration(d: Duration) -> String {
    let total_secs = d.as_secs();
    let hours = total_secs / 3600;
    let mins = (total_secs % 3600) / 60;
    let secs = total_secs % 60;

    if hours > 0 {
        format!("{}h {:02}m {:02}s", hours, mins, secs)
    } else if mins > 0 {
        format!("{}m {:02}s", mins, secs)
    } else {
        format!("{}s", secs)
    }
}

/// Set the modification and access times of a file to the given Unix
/// timestamp. Uses `std::fs::File::set_times` (stable since Rust 1.75).
///
/// Handles negative timestamps (dates before 1970) gracefully by clamping
/// to the Unix epoch.
fn set_file_mtime(path: &Path, timestamp: i64) -> std::io::Result<()> {
    let time = if timestamp >= 0 {
        UNIX_EPOCH + Duration::from_secs(timestamp as u64)
    } else {
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
    use crate::icloud::photos::PhotoAsset;
    use serde_json::json;
    use std::fs;

    /// Cross-platform temp directory for tests
    fn test_tmp_dir(subdir: &str) -> PathBuf {
        std::env::temp_dir().join("claude").join(subdir)
    }

    fn tmp_file(name: &str) -> PathBuf {
        let dir = test_tmp_dir("download_tests");
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        fs::write(&p, b"test").unwrap();
        p
    }

    fn test_config() -> DownloadConfig {
        DownloadConfig {
            directory: test_tmp_dir("download_filter_tests"),
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
            skip_live_photos: false,
            live_photo_size: AssetVersionSize::LiveOriginal,
            live_photo_mov_filename_policy: crate::types::LivePhotoMovFilenamePolicy::Suffix,
            align_raw: RawTreatmentPolicy::AsIs,
            no_progress_bar: true,
            file_match_policy: FileMatchPolicy::NameSizeDedupWithSuffix,
        }
    }

    /// Helper that calls filter_asset_to_tasks with a fresh claimed_paths map.
    /// Use this for simple tests that don't need to track paths across calls.
    fn filter_asset_fresh(asset: &PhotoAsset, config: &DownloadConfig) -> Vec<DownloadTask> {
        let mut claimed_paths = HashMap::new();
        filter_asset_to_tasks(asset, config, &mut claimed_paths)
    }

    fn photo_asset_with_version() -> PhotoAsset {
        PhotoAsset::new(
            json!({"recordName": "TEST_1", "fields": {
                "filenameEnc": {"value": "photo.jpg", "type": "STRING"},
                "itemType": {"value": "public.jpeg"},
                "resOriginalRes": {"value": {
                    "size": 1000,
                    "downloadURL": "https://example.com/orig",
                    "fileChecksum": "abc123"
                }},
                "resOriginalFileType": {"value": "public.jpeg"}
            }}),
            json!({"fields": {"assetDate": {"value": 1736899200000.0}}}),
        )
    }

    #[test]
    fn test_filter_asset_produces_task() {
        let asset = photo_asset_with_version();
        let config = test_config();
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].url, "https://example.com/orig");
        assert_eq!(tasks[0].checksum, "abc123");
        assert_eq!(tasks[0].size, 1000);
    }

    #[test]
    fn test_filter_skips_videos_when_configured() {
        let asset = PhotoAsset::new(
            json!({"recordName": "VID_1", "fields": {
                "filenameEnc": {"value": "movie.mov", "type": "STRING"},
                "itemType": {"value": "com.apple.quicktime-movie"},
                "resOriginalRes": {"value": {
                    "size": 50000,
                    "downloadURL": "https://example.com/vid",
                    "fileChecksum": "vid_ck"
                }},
                "resOriginalFileType": {"value": "com.apple.quicktime-movie"}
            }}),
            json!({"fields": {"assetDate": {"value": 1736899200000.0}}}),
        );
        let mut config = test_config();
        config.skip_videos = true;
        assert!(filter_asset_fresh(&asset, &config).is_empty());
    }

    #[test]
    fn test_filter_video_task_carries_size() {
        let asset = PhotoAsset::new(
            json!({"recordName": "VID_2", "fields": {
                "filenameEnc": {"value": "movie.mov", "type": "STRING"},
                "itemType": {"value": "com.apple.quicktime-movie"},
                "resOriginalRes": {"value": {
                    "size": 500_000_000,
                    "downloadURL": "https://example.com/big_vid",
                    "fileChecksum": "big_ck"
                }},
                "resOriginalFileType": {"value": "com.apple.quicktime-movie"}
            }}),
            json!({"fields": {"assetDate": {"value": 1736899200000.0}}}),
        );
        let config = test_config();
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].size, 500_000_000);
    }

    #[test]
    fn test_filter_skips_photos_when_configured() {
        let asset = photo_asset_with_version();
        let mut config = test_config();
        config.skip_photos = true;
        assert!(filter_asset_fresh(&asset, &config).is_empty());
    }

    #[test]
    fn test_filter_skips_asset_without_filename() {
        let asset = PhotoAsset::new(
            json!({"recordName": "NO_NAME", "fields": {
                "itemType": {"value": "public.jpeg"},
                "resOriginalRes": {"value": {
                    "size": 1000,
                    "downloadURL": "https://example.com/orig",
                    "fileChecksum": "abc123"
                }},
                "resOriginalFileType": {"value": "public.jpeg"}
            }}),
            json!({"fields": {"assetDate": {"value": 1736899200000.0}}}),
        );
        let config = test_config();
        assert!(filter_asset_fresh(&asset, &config).is_empty());
    }

    #[test]
    fn test_filter_skips_asset_without_requested_version() {
        let asset = PhotoAsset::new(
            json!({"recordName": "SMALL_ONLY", "fields": {
                "filenameEnc": {"value": "photo.jpg", "type": "STRING"},
                "itemType": {"value": "public.jpeg"},
                "resJPEGThumbRes": {"value": {
                    "size": 100,
                    "downloadURL": "https://example.com/thumb",
                    "fileChecksum": "th_ck"
                }},
                "resJPEGThumbFileType": {"value": "public.jpeg"}
            }}),
            json!({"fields": {"assetDate": {"value": 1736899200000.0}}}),
        );
        let config = test_config(); // requests Original, but only Thumb available
        assert!(filter_asset_fresh(&asset, &config).is_empty());
    }

    #[test]
    fn test_filter_skips_existing_file() {
        let dir = test_tmp_dir("download_filter_tests");
        fs::create_dir_all(&dir).unwrap();
        let asset = photo_asset_with_version();
        let mut config = test_config();
        config.directory = dir.clone();

        // First call should produce a task (file doesn't exist yet)
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 1);

        // Create the file with matching size (1000 bytes), second call should skip
        fs::create_dir_all(tasks[0].download_path.parent().unwrap()).unwrap();
        fs::write(&tasks[0].download_path, vec![0u8; 1000]).unwrap();
        assert!(filter_asset_fresh(&asset, &config).is_empty());

        // Cleanup
        let _ = fs::remove_file(&tasks[0].download_path);
    }

    #[test]
    fn test_filter_deduplicates_file_with_different_size() {
        let dir = test_tmp_dir("download_filter_tests_dedup");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let asset = photo_asset_with_version(); // version.size = 1000
        let mut config = test_config();
        config.directory = dir.clone();

        // First call: file doesn't exist yet
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 1);
        let original_path = tasks[0].download_path.clone();

        // Create a file with DIFFERENT size (simulating a collision with different content)
        fs::create_dir_all(original_path.parent().unwrap()).unwrap();
        fs::write(&original_path, vec![0u8; 500]).unwrap(); // 500 bytes, not 1000

        // Second call: should produce a task with deduped path (size suffix)
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 1);
        let dedup_path = tasks[0].download_path.to_str().unwrap();
        assert!(
            dedup_path.contains("-1000."),
            "Expected size suffix '-1000.' in deduped path, got: {}",
            dedup_path,
        );

        // Cleanup
        let _ = fs::remove_dir_all(&dir);
    }

    fn photo_asset_with_live_photo() -> PhotoAsset {
        PhotoAsset::new(
            json!({"recordName": "LIVE_1", "fields": {
                "filenameEnc": {"value": "IMG_0001.HEIC", "type": "STRING"},
                "itemType": {"value": "public.heic"},
                "resOriginalRes": {"value": {
                    "size": 2000,
                    "downloadURL": "https://example.com/heic_orig",
                    "fileChecksum": "heic_ck"
                }},
                "resOriginalFileType": {"value": "public.heic"},
                "resOriginalVidComplRes": {"value": {
                    "size": 3000,
                    "downloadURL": "https://example.com/live_mov",
                    "fileChecksum": "mov_ck"
                }},
                "resOriginalVidComplFileType": {"value": "com.apple.quicktime-movie"}
            }}),
            json!({"fields": {"assetDate": {"value": 1736899200000.0}}}),
        )
    }

    #[test]
    fn test_filter_produces_live_photo_mov_task() {
        let asset = photo_asset_with_live_photo();
        let config = test_config();
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].url, "https://example.com/heic_orig");
        assert_eq!(tasks[0].size, 2000);
        assert_eq!(tasks[1].url, "https://example.com/live_mov");
        assert_eq!(tasks[1].size, 3000);
        assert!(tasks[1]
            .download_path
            .to_str()
            .unwrap()
            .contains("IMG_0001_HEVC.MOV"));
    }

    #[test]
    fn test_filter_skips_live_photo_when_configured() {
        let asset = photo_asset_with_live_photo();
        let mut config = test_config();
        config.skip_live_photos = true;
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].url, "https://example.com/heic_orig");
    }

    #[test]
    fn test_filter_live_photo_original_policy() {
        let asset = photo_asset_with_live_photo();
        let mut config = test_config();
        config.live_photo_mov_filename_policy = crate::types::LivePhotoMovFilenamePolicy::Original;
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 2);
        assert!(tasks[1]
            .download_path
            .to_str()
            .unwrap()
            .contains("IMG_0001.MOV"));
    }

    #[test]
    fn test_filter_skips_existing_live_photo_mov() {
        let dir = test_tmp_dir("download_filter_tests_live");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let asset = photo_asset_with_live_photo();
        let mut config = test_config();
        config.directory = dir.clone();

        // First call: both photo and MOV
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 2);

        // Create the MOV file on disk with matching size (3000 bytes)
        fs::create_dir_all(tasks[1].download_path.parent().unwrap()).unwrap();
        fs::write(&tasks[1].download_path, vec![0u8; 3000]).unwrap();

        // Second call: only the photo task (MOV already exists with matching size)
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].url, "https://example.com/heic_orig");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_filter_deduplicates_live_photo_mov_collision() {
        let dir = test_tmp_dir("download_filter_tests_live_dedup");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let asset = photo_asset_with_live_photo();
        let mut config = test_config();
        config.directory = dir.clone();

        // First call to get the expected MOV path
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 2);
        let mov_path = &tasks[1].download_path;

        // Create a file at the MOV path with a DIFFERENT size (simulating a
        // regular video that collides with the live photo companion name).
        fs::create_dir_all(mov_path.parent().unwrap()).unwrap();
        fs::write(mov_path, vec![0u8; 9999]).unwrap();

        // Second call: should produce a deduped MOV path with asset ID suffix
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[1].url, "https://example.com/live_mov");
        let dedup_path = tasks[1].download_path.to_str().unwrap();
        assert!(
            dedup_path.contains("LIVE_1"),
            "Expected asset ID 'LIVE_1' in deduped path, got: {}",
            dedup_path,
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_filter_live_photo_medium_size() {
        let asset = PhotoAsset::new(
            json!({"recordName": "LIVE_MED", "fields": {
                "filenameEnc": {"value": "IMG_0002.HEIC", "type": "STRING"},
                "itemType": {"value": "public.heic"},
                "resOriginalRes": {"value": {
                    "size": 2000,
                    "downloadURL": "https://example.com/heic_orig",
                    "fileChecksum": "heic_ck"
                }},
                "resOriginalFileType": {"value": "public.heic"},
                "resVidMedRes": {"value": {
                    "size": 1500,
                    "downloadURL": "https://example.com/live_med",
                    "fileChecksum": "med_ck"
                }},
                "resVidMedFileType": {"value": "com.apple.quicktime-movie"}
            }}),
            json!({"fields": {"assetDate": {"value": 1736899200000.0}}}),
        );
        let mut config = test_config();
        config.live_photo_size = AssetVersionSize::LiveMedium;
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[1].url, "https://example.com/live_med");
    }

    #[test]
    fn test_filter_no_live_photo_for_videos() {
        let asset = PhotoAsset::new(
            json!({"recordName": "VID_1", "fields": {
                "filenameEnc": {"value": "movie.mov", "type": "STRING"},
                "itemType": {"value": "com.apple.quicktime-movie"},
                "resOriginalRes": {"value": {
                    "size": 50000,
                    "downloadURL": "https://example.com/vid",
                    "fileChecksum": "vid_ck"
                }},
                "resOriginalFileType": {"value": "com.apple.quicktime-movie"},
                "resOriginalVidComplRes": {"value": {
                    "size": 3000,
                    "downloadURL": "https://example.com/live_mov",
                    "fileChecksum": "mov_ck"
                }},
                "resOriginalVidComplFileType": {"value": "com.apple.quicktime-movie"}
            }}),
            json!({"fields": {"assetDate": {"value": 1736899200000.0}}}),
        );
        let config = test_config();
        let tasks = filter_asset_fresh(&asset, &config);
        // Videos should get 1 task (the video itself), not a live photo MOV
        assert_eq!(tasks.len(), 1);
    }

    #[test]
    fn test_set_file_mtime_positive_timestamp() {
        let p = tmp_file("pos.txt");
        set_file_mtime(&p, 1_700_000_000).unwrap();
        let meta = fs::metadata(&p).unwrap();
        let mtime = meta.modified().unwrap();
        assert_eq!(mtime, UNIX_EPOCH + Duration::from_secs(1_700_000_000));
    }

    #[test]
    fn test_set_file_mtime_zero_timestamp() {
        let p = tmp_file("zero.txt");
        set_file_mtime(&p, 0).unwrap();
        let meta = fs::metadata(&p).unwrap();
        let mtime = meta.modified().unwrap();
        assert_eq!(mtime, UNIX_EPOCH);
    }

    #[test]
    fn test_set_file_mtime_negative_timestamp() {
        let p = tmp_file("neg.txt");
        // Should not panic — clamps or uses pre-epoch time
        set_file_mtime(&p, -86400).unwrap();
    }

    #[test]
    fn test_set_file_mtime_nonexistent_file() {
        let p = test_tmp_dir("download_tests").join("nonexistent_file.txt");
        let _ = fs::remove_file(&p); // ensure absent
        assert!(set_file_mtime(&p, 0).is_err());
    }

    fn photo_asset_with_original_and_alternative(orig_type: &str, alt_type: &str) -> PhotoAsset {
        PhotoAsset::new(
            json!({"recordName": "RAW_TEST", "fields": {
                "filenameEnc": {"value": "photo.jpg", "type": "STRING"},
                "itemType": {"value": "public.jpeg"},
                "resOriginalRes": {"value": {
                    "size": 1000,
                    "downloadURL": "https://example.com/orig",
                    "fileChecksum": "orig_ck"
                }},
                "resOriginalFileType": {"value": orig_type},
                "resOriginalAltRes": {"value": {
                    "size": 2000,
                    "downloadURL": "https://example.com/alt",
                    "fileChecksum": "alt_ck"
                }},
                "resOriginalAltFileType": {"value": alt_type}
            }}),
            json!({"fields": {"assetDate": {"value": 1736899200000.0}}}),
        )
    }

    #[test]
    fn test_raw_policy_as_is_no_swap() {
        let asset = photo_asset_with_original_and_alternative("public.jpeg", "com.adobe.raw-image");
        let versions = apply_raw_policy(asset.versions(), RawTreatmentPolicy::AsIs);
        assert_eq!(
            versions[&AssetVersionSize::Original].url,
            "https://example.com/orig"
        );
        assert_eq!(
            versions[&AssetVersionSize::Alternative].url,
            "https://example.com/alt"
        );
    }

    #[test]
    fn test_raw_policy_as_original_swaps_when_alt_is_raw() {
        let asset = photo_asset_with_original_and_alternative("public.jpeg", "com.adobe.raw-image");
        let versions = apply_raw_policy(asset.versions(), RawTreatmentPolicy::AsOriginal);
        // Alternative was RAW → swap: Original now has alt URL
        assert_eq!(
            versions[&AssetVersionSize::Original].url,
            "https://example.com/alt"
        );
        assert_eq!(
            versions[&AssetVersionSize::Alternative].url,
            "https://example.com/orig"
        );
    }

    #[test]
    fn test_raw_policy_as_alternative_swaps_when_orig_is_raw() {
        let asset = photo_asset_with_original_and_alternative("com.adobe.raw-image", "public.jpeg");
        let versions = apply_raw_policy(asset.versions(), RawTreatmentPolicy::AsAlternative);
        // Original was RAW → swap: Alternative now has orig URL
        assert_eq!(
            versions[&AssetVersionSize::Original].url,
            "https://example.com/alt"
        );
        assert_eq!(
            versions[&AssetVersionSize::Alternative].url,
            "https://example.com/orig"
        );
    }

    #[test]
    fn test_raw_policy_as_original_no_swap_when_alt_not_raw() {
        let asset = photo_asset_with_original_and_alternative("public.jpeg", "public.jpeg");
        let versions = apply_raw_policy(asset.versions(), RawTreatmentPolicy::AsOriginal);
        assert_eq!(
            versions[&AssetVersionSize::Original].url,
            "https://example.com/orig"
        );
    }

    #[test]
    fn test_raw_policy_as_alternative_no_swap_when_orig_not_raw() {
        let asset = photo_asset_with_original_and_alternative("public.jpeg", "public.jpeg");
        let versions = apply_raw_policy(asset.versions(), RawTreatmentPolicy::AsAlternative);
        assert_eq!(
            versions[&AssetVersionSize::Original].url,
            "https://example.com/orig"
        );
    }

    #[test]
    fn test_raw_policy_no_alternative_no_swap() {
        let asset = photo_asset_with_version(); // only has Original
        let versions = apply_raw_policy(asset.versions(), RawTreatmentPolicy::AsOriginal);
        assert_eq!(
            versions[&AssetVersionSize::Original].url,
            "https://example.com/orig"
        );
        assert!(!versions.contains_key(&AssetVersionSize::Alternative));
    }

    #[test]
    fn test_filter_asset_uses_raw_policy_swap() {
        let asset = photo_asset_with_original_and_alternative("public.jpeg", "com.adobe.raw-image");
        let mut config = test_config();
        config.align_raw = RawTreatmentPolicy::AsOriginal;
        // With AsOriginal and RAW alternative, the swap makes Original point to alt URL
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].url, "https://example.com/alt");
        assert_eq!(tasks[0].checksum, "alt_ck");
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
        let pb = create_progress_bar(true, 100);
        assert!(pb.is_hidden());
    }

    #[test]
    fn test_filter_detects_case_insensitive_collision() {
        // On case-insensitive filesystems (macOS, Windows), IMG_0996.mov and IMG_0996.MOV
        // are the same file. Test that claimed_paths detects this collision.
        let dir = test_tmp_dir("download_filter_tests_case");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        // First asset: regular video IMG_0996.mov
        let video_asset = PhotoAsset::new(
            json!({"recordName": "VID_0996", "fields": {
                "filenameEnc": {"value": "IMG_0996.mov", "type": "STRING"},
                "itemType": {"value": "com.apple.quicktime-movie"},
                "resOriginalRes": {"value": {
                    "size": 258592890,
                    "downloadURL": "https://example.com/vid",
                    "fileChecksum": "vid_ck"
                }},
                "resOriginalFileType": {"value": "com.apple.quicktime-movie"}
            }}),
            json!({"fields": {"assetDate": {"value": 1713657600000.0}}}), // 2025/04/21
        );

        // Second asset: live photo IMG_0996.JPG whose MOV companion would be IMG_0996.MOV
        let photo_asset = PhotoAsset::new(
            json!({"recordName": "IMG_0996", "fields": {
                "filenameEnc": {"value": "IMG_0996.JPG", "type": "STRING"},
                "itemType": {"value": "public.jpeg"},
                "resOriginalRes": {"value": {
                    "size": 5000,
                    "downloadURL": "https://example.com/jpg",
                    "fileChecksum": "jpg_ck"
                }},
                "resOriginalFileType": {"value": "public.jpeg"},
                "resOriginalVidComplRes": {"value": {
                    "size": 124037918,
                    "downloadURL": "https://example.com/live_mov",
                    "fileChecksum": "mov_ck"
                }},
                "resOriginalVidComplFileType": {"value": "com.apple.quicktime-movie"}
            }}),
            json!({"fields": {"assetDate": {"value": 1713657600000.0}}}), // Same date
        );

        let mut config = test_config();
        config.directory = dir.clone();

        // Process both assets through claimed_paths
        let mut claimed_paths = HashMap::new();
        let video_tasks = filter_asset_to_tasks(&video_asset, &config, &mut claimed_paths);
        assert_eq!(video_tasks.len(), 1);
        let video_path = &video_tasks[0].download_path;
        eprintln!("Video path: {:?}", video_path);

        let photo_tasks = filter_asset_to_tasks(&photo_asset, &config, &mut claimed_paths);
        assert_eq!(photo_tasks.len(), 2, "Expected 2 tasks (photo + MOV)");

        let mov_task = &photo_tasks[1];
        let mov_path = &mov_task.download_path;
        eprintln!("Live MOV path: {:?}", mov_path);
        eprintln!(
            "Claimed paths: {:?}",
            claimed_paths.keys().collect::<Vec<_>>()
        );

        // Check the MOV filename based on platform
        let mov_filename = mov_path.file_name().unwrap().to_str().unwrap();

        // On case-insensitive filesystems (macOS, Windows), IMG_0996.mov and IMG_0996.MOV
        // collide, so the MOV should be deduped with asset ID suffix.
        // On case-sensitive filesystems (Linux), they're different files, no dedup needed.
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            assert!(
                mov_filename.contains("-IMG_0996"),
                "Case-insensitive collision: MOV should be deduped with asset ID suffix. \
                Got: {}",
                mov_filename
            );
        }

        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            // On Linux (case-sensitive), both files can coexist without dedup
            assert_eq!(
                mov_filename, "IMG_0996.MOV",
                "On case-sensitive FS, MOV should keep original name"
            );
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_create_progress_bar_with_total() {
        // When not disabled, the bar should have the correct length.
        // In CI/test environments stdout may not be a TTY, so the bar
        // may be hidden — we test both branches.
        let pb = create_progress_bar(false, 42);
        if std::io::stdout().is_terminal() {
            assert!(!pb.is_hidden());
            assert_eq!(pb.length(), Some(42));
        } else {
            // Non-TTY: bar is hidden regardless of the flag
            assert!(pb.is_hidden());
        }
    }

    #[test]
    fn test_filter_asset_as_is_downloads_original() {
        let asset = photo_asset_with_original_and_alternative("public.jpeg", "com.adobe.raw-image");
        let config = test_config(); // align_raw defaults to AsIs
        let tasks = filter_asset_fresh(&asset, &config);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].url, "https://example.com/orig");
        assert_eq!(tasks[0].checksum, "orig_ck");
    }

    // These tests overflow the stack in debug builds due to large async futures
    // from reqwest and stream combinators. Run with: cargo test --release
    #[tokio::test]
    #[ignore = "stack overflow in debug builds; run with --release"]
    async fn test_run_download_pass_skips_all_tasks_when_cancelled() {
        let token = CancellationToken::new();
        token.cancel();

        let tasks = vec![
            DownloadTask {
                url: "https://example.com/a".into(),
                download_path: test_tmp_dir("shutdown_test").join("a.jpg"),
                checksum: "aaa".into(),
                created_local: chrono::Local::now(),
                size: 1000,
            },
            DownloadTask {
                url: "https://example.com/b".into(),
                download_path: test_tmp_dir("shutdown_test").join("b.jpg"),
                checksum: "bbb".into(),
                created_local: chrono::Local::now(),
                size: 2000,
            },
        ];

        let client = Client::new();
        let retry = RetryConfig::default();

        // Pre-cancelled token: take_while stops immediately, no downloads attempted.
        let result = run_download_pass(&client, tasks, &retry, false, 1, true, token).await;
        assert!(result.failed.is_empty());
    }

    #[tokio::test]
    #[ignore = "stack overflow in debug builds; run with --release"]
    async fn test_run_download_pass_processes_tasks_when_not_cancelled() {
        let token = CancellationToken::new();

        let tasks = vec![DownloadTask {
            url: "https://0.0.0.0:1/nonexistent".into(),
            download_path: test_tmp_dir("shutdown_test").join("c.jpg"),
            checksum: "ccc".into(),
            created_local: chrono::Local::now(),
            size: 500,
        }];

        let client = Client::new();
        let retry = RetryConfig {
            max_retries: 0,
            base_delay_secs: 0,
            max_delay_secs: 0,
        };

        // Non-cancelled token: task is attempted (and fails since URL is bogus).
        let result = run_download_pass(&client, tasks, &retry, false, 1, true, token).await;
        assert_eq!(result.failed.len(), 1);
    }
}
