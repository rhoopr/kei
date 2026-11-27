//! The file operations the sync core needs, on top of the existing path
//! derivation and transfer owners. Publication rules stay in `file/`:
//! a verified `.part`, no-overwrite publication unless the caller proves the
//! current bytes, and a durable parent directory before success is returned.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::Client;
use tokio_util::sync::CancellationToken;

use super::error::DownloadError;
use super::file::{
    DownloadLimits, DownloadOpts, ExistingFileFingerprint, FinalPublication,
    download_file_with_mode,
};
use super::filter::{PathDerivationConfig, derive_expected_paths, is_asset_filtered};
use crate::icloud::photos::PhotoAsset;
use crate::retry::RetryConfig;

/// One file the path rules place for an asset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExpectedFile {
    pub(crate) rendition: &'static str,
    pub(crate) path: PathBuf,
    pub(crate) size: u64,
    pub(crate) checksum: String,
    /// Empty or a stored placeholder unless the asset came from a fresh lookup.
    pub(crate) url: String,
}

/// The files `config` places for `asset`, or nothing if its filters exclude it.
pub(crate) fn expected_files(
    asset: &PhotoAsset,
    config: &PathDerivationConfig,
) -> Vec<ExpectedFile> {
    if is_asset_filtered(asset, config).is_some() {
        return Vec::new();
    }
    derive_expected_paths(asset, config)
        .into_iter()
        .map(|derived| ExpectedFile {
            rendition: derived.version_size.as_str(),
            path: derived.path,
            size: derived.size,
            checksum: derived.checksum.into(),
            url: derived.url.into(),
        })
        .collect()
}

/// The bytes kei last recorded for a file it may replace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Recorded {
    pub(crate) size: u64,
    pub(crate) sha256: [u8; 32],
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum FetchError {
    /// The download URL is no longer accepted; a fresh lookup may fix it.
    #[error("download URL expired")]
    Expired,
    #[error("interrupted")]
    Interrupted,
    #[error(transparent)]
    Failed(anyhow::Error),
}

pub(crate) struct Fetch<'a> {
    pub(crate) client: &'a Client,
    pub(crate) url: &'a str,
    pub(crate) path: &'a Path,
    pub(crate) checksum: &'a str,
    pub(crate) size: u64,
    pub(crate) temp_suffix: &'a str,
    pub(crate) retry: &'a RetryConfig,
    /// Replace only these exact bytes; `None` publishes without overwriting.
    pub(crate) replace: Option<Recorded>,
    /// Capture time for the file's modification time.
    pub(crate) modified: i64,
    pub(crate) cancel: &'a CancellationToken,
    pub(crate) bandwidth: Option<&'a super::BandwidthLimiter>,
}

/// Download one copy, verify it, publish it and sync its parent directory.
/// Returns the published bytes' SHA-256 as lowercase hex.
pub(crate) async fn fetch(request: Fetch<'_>) -> Result<String, FetchError> {
    let publication = match request.replace {
        Some(recorded) => FinalPublication::ReplaceLocal {
            fingerprint: ExistingFileFingerprint {
                size: recorded.size,
                sha256: recorded.sha256,
            },
            modified: None,
        },
        None => FinalPublication::NoReplace,
    };
    let result = download_file_with_mode(
        request.client,
        request.url,
        request.path,
        request.checksum,
        request.retry,
        request.temp_suffix,
        DownloadOpts {
            skip_rename: true,
            expected_size: Some(request.size),
            publication,
        },
        DownloadLimits {
            shutdown_token: Some(request.cancel),
            bandwidth_limiter: request.bandwidth,
            ..DownloadLimits::default()
        },
        crate::personality::Mode::Off,
    )
    .await;
    let mut downloaded = match result {
        Ok(downloaded) => downloaded,
        Err(DownloadError::Interrupted { .. }) => return Err(FetchError::Interrupted),
        Err(error) if is_expired(&error) => return Err(FetchError::Expired),
        Err(error) => return Err(FetchError::Failed(error.into())),
    };
    let fingerprint = downloaded.fingerprint().await.map_err(FetchError::Failed)?;
    downloaded
        .set_times(file_times(request.modified))
        .await
        .map_err(FetchError::Failed)?;
    // Installed and identical-existing bytes both leave exactly the verified
    // content at the path; the recorded hash is the same either way.
    let _installed_or_identical = downloaded
        .publish(request.path, publication)
        .await
        .map_err(FetchError::Failed)?;
    crate::fs_util::fsync_parent_dir_async(request.path)
        .await
        .map_err(|error| FetchError::Failed(error.into()))?;
    Ok(hex(&fingerprint.sha256))
}

/// Finish or undo a replacement at `path` that a crash interrupted, so its
/// bytes can be compared. Does nothing when no replacement was in progress.
pub(crate) async fn recover_replacement(path: &Path) -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    {
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || super::file::recover_file_replacement(&path)).await??;
    }
    #[cfg(not(target_os = "linux"))]
    let _ = path;
    Ok(())
}

/// SHA-256 and size of a regular file kei is about to adopt or replace.
pub(crate) async fn hash_file(path: &Path) -> anyhow::Result<Option<(u64, String)>> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => anyhow::bail!("{} is not a regular file", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let fingerprint = super::file::fingerprint_regular_file(path).await?;
    Ok(Some((fingerprint.size, hex(&fingerprint.sha256))))
}

pub(crate) fn parse_sha256(value: &str) -> Option<[u8; 32]> {
    let mut out = [0u8; 32];
    if value.len() != 64 {
        return None;
    }
    for (index, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(value.get(index * 2..index * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

fn hex(bytes: &[u8; 32]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(64), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

fn is_expired(error: &DownloadError) -> bool {
    matches!(
        error,
        DownloadError::HttpStatus {
            status: 401 | 403 | 404 | 410,
            ..
        } | DownloadError::Http {
            status: 401 | 403 | 404 | 410,
            ..
        }
    )
}

fn file_times(timestamp: i64) -> std::fs::FileTimes {
    let time = if timestamp >= 0 {
        UNIX_EPOCH + Duration::from_secs(timestamp.unsigned_abs())
    } else {
        UNIX_EPOCH
            .checked_sub(Duration::from_secs(timestamp.unsigned_abs()))
            .unwrap_or(SystemTime::UNIX_EPOCH)
    };
    std::fs::FileTimes::new()
        .set_modified(time)
        .set_accessed(time)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_hex_round_trips() {
        let bytes = [0xabu8; 32];
        assert_eq!(parse_sha256(&hex(&bytes)), Some(bytes));
        assert_eq!(parse_sha256("zz"), None);
    }
}
