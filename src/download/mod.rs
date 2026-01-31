//! Download engine: filters iCloud photos, downloads with retry, and stamps
//! EXIF metadata. Uses a three-phase approach (filter → download → cleanup)
//! to handle expired CDN URLs gracefully on large libraries.

pub mod error;
pub mod exif;
pub mod file;
pub mod paths;

use std::fs::FileTimes;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use chrono::{DateTime, Local, Utc};
use reqwest::Client;

use std::path::PathBuf;

use futures_util::stream::{self, StreamExt};

use crate::icloud::photos::{AssetItemType, AssetVersionSize, PhotoAlbum};
use crate::retry::RetryConfig;

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
}

/// A unit of work produced by the filter phase and consumed by the download phase.
struct DownloadTask {
    url: String,
    download_path: PathBuf,
    checksum: String,
    created_local: DateTime<Local>,
}

/// Fetch photos from all albums and build filtered download tasks.
///
/// This re-contacts the iCloud API, so each call yields fresh download URLs.
/// Files that already exist on disk are skipped automatically.
async fn build_download_tasks(
    albums: &[PhotoAlbum],
    config: &DownloadConfig,
) -> Result<Vec<DownloadTask>> {
    let album_results: Vec<Result<Vec<_>>> = stream::iter(albums)
        .map(|album| async move { album.photos(config.recent).await })
        .buffer_unordered(config.concurrent_downloads)
        .collect()
        .await;

    let mut tasks: Vec<DownloadTask> = Vec::new();
    for album_result in album_results {
        let assets = album_result?;

        for asset in &assets {
            if config.skip_videos && asset.item_type() == Some(AssetItemType::Movie) {
                continue;
            }
            if config.skip_photos && asset.item_type() == Some(AssetItemType::Image) {
                continue;
            }

            let created_utc = asset.created();
            if let Some(before) = &config.skip_created_before {
                if created_utc < *before {
                    continue;
                }
            }
            if let Some(after) = &config.skip_created_after {
                if created_utc > *after {
                    continue;
                }
            }

            let filename = match asset.filename() {
                Some(f) => f,
                None => {
                    tracing::warn!("Asset {} has no filename, skipping", asset.id());
                    continue;
                }
            };

            let created_local: DateTime<Local> = created_utc.with_timezone(&Local);
            let download_path = paths::local_download_path(
                &config.directory,
                &config.folder_structure,
                &created_local,
                filename,
            );

            if download_path.exists() {
                tracing::debug!("{} already exists", download_path.display());
                continue;
            }

            let versions = match asset.versions() {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("Skipping asset {}: {}", asset.id(), e);
                    continue;
                }
            };
            if let Some(version) = versions.get(&config.size) {
                tasks.push(DownloadTask {
                    url: version.url.clone(),
                    download_path,
                    checksum: version.checksum.clone(),
                    created_local,
                });
            }
        }
    }

    Ok(tasks)
}

/// Main download loop: fetch all photos from each album, apply filters,
/// check local existence, download missing files, and optionally stamp
/// EXIF datetime.
///
/// Uses a three-phase approach:
/// 1. Filter pass — builds a list of download tasks with fresh URLs
/// 2. Parallel download — consumes tasks with bounded concurrency
/// 3. Cleanup pass — re-fetches URLs from API and retries failures
pub async fn download_photos(
    client: &Client,
    albums: &[PhotoAlbum],
    config: &DownloadConfig,
) -> Result<()> {
    // ── Phase 1: Build download tasks ────────────────────────────────
    let tasks = build_download_tasks(albums, config).await?;

    if tasks.is_empty() {
        tracing::info!("No new photos to download");
        return Ok(());
    }

    tracing::info!(
        "Found {} photos to download (concurrency: {})",
        tasks.len(),
        config.concurrent_downloads
    );

    if config.dry_run {
        for task in &tasks {
            tracing::info!("[DRY RUN] Would download {}", task.download_path.display());
        }
        print_summary(&tasks, config);
        return Ok(());
    }

    // ── Phase 2: Parallel download ───────────────────────────────────
    let total = tasks.len();
    let failed_tasks = run_download_pass(
        client,
        tasks,
        &config.retry,
        config.set_exif_datetime,
        config.concurrent_downloads,
    )
    .await;

    if failed_tasks.is_empty() {
        tracing::info!("── Summary ──");
        tracing::info!("  {} downloaded, 0 failed, {} total", total, total);
        return Ok(());
    }

    // Phase 3: Re-fetch from API to get fresh CDN URLs (old ones may have
    // expired during a long Phase 2), then retry at concurrency 1 to give
    // large files full bandwidth.
    let cleanup_concurrency = 1;
    let failure_count = failed_tasks.len();
    tracing::info!(
        "── Cleanup pass: re-fetching URLs and retrying {} failed downloads (concurrency: {}) ──",
        failure_count,
        cleanup_concurrency,
    );

    let fresh_tasks = build_download_tasks(albums, config).await?;
    tracing::info!(
        "  Re-fetched {} tasks with fresh URLs",
        fresh_tasks.len()
    );

    let remaining_failed = run_download_pass(
        client,
        fresh_tasks,
        &config.retry,
        config.set_exif_datetime,
        cleanup_concurrency,
    )
    .await;

    let failed = remaining_failed.len();
    let succeeded = total - failed;
    tracing::info!("── Summary ──");
    tracing::info!("  {} downloaded, {} failed, {} total", succeeded, failed, total);

    if failed > 0 {
        for task in &remaining_failed {
            tracing::error!("Download failed: {}", task.download_path.display());
        }
        anyhow::bail!("{} of {} downloads failed", failed, total);
    }

    Ok(())
}

/// Execute a download pass over the given tasks, returning any that failed.
async fn run_download_pass(
    client: &Client,
    tasks: Vec<DownloadTask>,
    retry_config: &RetryConfig,
    set_exif: bool,
    concurrency: usize,
) -> Vec<DownloadTask> {
    let client = client.clone();
    let retry_config = retry_config.clone();

    let results: Vec<(DownloadTask, Result<()>)> = stream::iter(tasks)
        .map(|task| {
            let client = client.clone();
            let retry_config = retry_config.clone();
            async move {
                let result = download_single_task(&client, &task, &retry_config, set_exif).await;
                (task, result)
            }
        })
        .buffer_unordered(concurrency)
        .collect()
        .await;

    results
        .into_iter()
        .filter_map(|(task, result)| {
            if let Err(e) = &result {
                tracing::error!("Download failed: {}: {}", task.download_path.display(), e);
                Some(task)
            } else {
                None
            }
        })
        .collect()
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
    if let Err(e) = tokio::task::spawn_blocking(move || {
        set_file_mtime(&mtime_path, ts)
    })
    .await?
    {
        tracing::warn!(
            "Could not set mtime on {}: {}",
            task.download_path.display(),
            e
        );
    }

    tracing::info!("Downloaded {}", task.download_path.display());

    if set_exif {
        let ext = task
            .download_path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        if matches!(ext.to_ascii_lowercase().as_str(), "jpg" | "jpeg") {
            let exif_path = task.download_path.clone();
            let date_str = task
                .created_local
                .format("%Y:%m:%d %H:%M:%S")
                .to_string();
            let exif_result = tokio::task::spawn_blocking(move || {
                match exif::get_photo_exif(&exif_path) {
                    Ok(None) => {
                        if let Err(e) =
                            exif::set_photo_exif(&exif_path, &date_str)
                        {
                            tracing::warn!(
                                "Failed to set EXIF on {}: {}",
                                exif_path.display(),
                                e
                            );
                        }
                    }
                    Ok(Some(_)) => {}
                    Err(e) => {
                        tracing::warn!(
                            "Failed to read EXIF from {}: {}",
                            exif_path.display(),
                            e
                        );
                    }
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

/// Print a dry-run summary of what would be downloaded.
fn print_summary(tasks: &[DownloadTask], config: &DownloadConfig) {
    tracing::info!("── Dry Run Summary ──");
    tracing::info!("  {} files would be downloaded", tasks.len());
    tracing::info!("  destination: {}", config.directory.display());
    tracing::info!("  concurrency: {}", config.concurrent_downloads);
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
    use std::fs;

    fn tmp_file(name: &str) -> PathBuf {
        let dir = PathBuf::from("/tmp/claude/download_tests");
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        fs::write(&p, b"test").unwrap();
        p
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
        let p = PathBuf::from("/tmp/claude/download_tests/nonexistent_file.txt");
        let _ = fs::remove_file(&p); // ensure absent
        assert!(set_file_mtime(&p, 0).is_err());
    }
}
