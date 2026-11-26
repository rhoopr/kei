//! Durable pending-retry identity resolution and targeted provider revalidation.

use std::path::Path;
use std::sync::Arc;

use super::{DownloadTask, RecordedLocalFile, file};
use crate::state::VersionSizeKey;
use anyhow::{Context, Result};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct PendingRetryTarget {
    pub(super) library: Arc<str>,
    pub(super) asset_id: Arc<str>,
    pub(super) version_size: VersionSizeKey,
}

impl PendingRetryTarget {
    pub(super) fn from_record(record: &crate::state::AssetRecord) -> Self {
        Self {
            library: Arc::clone(&record.library),
            asset_id: Arc::from(record.id.as_ref()),
            version_size: record.version_size,
        }
    }

    pub(super) fn from_task(task: &DownloadTask) -> Self {
        Self {
            library: Arc::clone(&task.library),
            asset_id: Arc::clone(&task.asset_id),
            version_size: task.version_size,
        }
    }
}

#[derive(Debug)]
pub(super) struct PendingRetryEvidence {
    checksum: Arc<str>,
    local_file: Option<RecordedLocalFile>,
    downloaded_at: Option<chrono::DateTime<chrono::Utc>>,
    size_bytes: u64,
    last_error: Option<Arc<str>>,
}

impl PendingRetryEvidence {
    pub(super) fn from_record(record: &crate::state::AssetRecord) -> Self {
        Self {
            checksum: Arc::from(record.checksum.as_ref()),
            local_file: record.local_path.clone().map(|path| RecordedLocalFile {
                path,
                local_checksum: record.local_checksum.as_deref().map(Into::into),
                download_checksum: record.download_checksum.as_deref().map(Into::into),
            }),
            downloaded_at: record.downloaded_at,
            size_bytes: record.size_bytes,
            last_error: record.last_error.as_deref().map(Into::into),
        }
    }

    pub(super) async fn truncated_repair_fingerprint(
        &self,
        enabled: bool,
    ) -> Result<Option<file::ExistingFileFingerprint>> {
        if !enabled
            || self.last_error.as_deref() != Some(crate::commands::reconcile::FILE_TRUNCATED_REASON)
            || self.downloaded_at.is_none()
        {
            return Ok(None);
        }
        let Some(local_file) = &self.local_file else {
            return Ok(None);
        };
        match tokio::fs::symlink_metadata(&local_file.path).await {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "Could not inspect truncated repair path {}",
                        local_file.path.display()
                    )
                });
            }
        }
        let fingerprint = file::fingerprint_regular_file(&local_file.path).await?;
        if self.size_bytes == 0 || fingerprint.size >= self.size_bytes {
            return Ok(None);
        }

        let metadata_changed_download = matches!(
            (
                local_file.local_checksum.as_deref(),
                local_file.download_checksum.as_deref()
            ),
            (Some(local), Some(download)) if local != download
        );
        if metadata_changed_download {
            let actual_checksum = data_encoding::HEXLOWER.encode(&fingerprint.sha256);
            if local_file.local_checksum.as_deref() == Some(actual_checksum.as_str()) {
                return Ok(None);
            }
        }
        Ok(Some(fingerprint))
    }

    pub(super) fn matches_provider_version(&self, task: &DownloadTask) -> bool {
        self.checksum.as_ref() == task.checksum.as_ref() && self.size_bytes == task.size
    }

    pub(super) fn local_path_under<'a>(&'a self, directory: &Path) -> Option<&'a Path> {
        self.local_file
            .as_ref()
            .map(|file| file.path.as_path())
            .filter(|path| path.starts_with(directory))
    }
}
