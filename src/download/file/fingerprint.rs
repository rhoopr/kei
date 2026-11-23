//! File hashes and same-read size and media-prefix snapshots.

use std::path::Path;
use std::sync::Arc;

use crate::fs_util::{ConfinedParents, ConfinedPath};
use anyhow::Context;
use tokio::fs;

/// Exact bytes that retry planning authorized the publisher to replace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::download) struct ExistingFileFingerprint {
    pub(in crate::download) size: u64,
    pub(in crate::download) sha256: [u8; 32],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::download) struct ExistingFileSnapshot {
    pub(in crate::download) fingerprint: ExistingFileFingerprint,
    pub(in crate::download) prefix: [u8; 12],
    pub(in crate::download) prefix_len: usize,
}

/// Compute the SHA-256 hash of a file, returning a hex-encoded string.
///
/// Used for `local_checksum` / `download_checksum` in the state DB and
/// by `verify --checksums` for integrity checks.
pub(crate) async fn compute_sha256(path: &Path) -> anyhow::Result<String> {
    let fingerprint = fingerprint_file(path).await?;
    Ok(data_encoding::HEXLOWER.encode(&fingerprint.sha256))
}

/// Read one file handle to capture the size and SHA-256 of the same bytes.
pub(in crate::download) async fn fingerprint_file(
    path: &Path,
) -> anyhow::Result<ExistingFileFingerprint> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || fingerprint_file_blocking(&path)).await?
}

/// Read a recorded destination without following leaf or ancestor links.
pub(in crate::download) async fn fingerprint_downloaded_path(
    root: &Path,
    path: &Path,
) -> anyhow::Result<ExistingFileFingerprint> {
    let root = root.to_path_buf();
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let confined = ConfinedPath::open(&root, &path, ConfinedParents::Existing)?;
        let mut file = confined.open_regular()?;
        let identity = crate::fs_util::file_identity(&file)?;
        let fingerprint = fingerprint_open_file_snapshot_blocking(&mut file, &path)?.fingerprint;
        confined.validate_identity(identity)?;
        Ok(fingerprint)
    })
    .await?
}

/// Keep the confined file, bytes and observed mtime alive through pending adoption.
#[derive(Debug)]
pub(in crate::download) struct RetainedPendingFile {
    path: ConfinedPath,
    file: std::fs::File,
    pub(in crate::download) fingerprint: ExistingFileFingerprint,
    pub(in crate::download) modified: Option<std::time::SystemTime>,
}

/// Facts from one confined read of an existing selection destination.
pub(in crate::download) struct ObservedFile {
    pub(in crate::download) fingerprint: ExistingFileFingerprint,
    pub(in crate::download) modified: Option<std::time::SystemTime>,
}

impl ObservedFile {
    pub(in crate::download) async fn read(
        root: &Path,
        path: &Path,
    ) -> anyhow::Result<Option<Self>> {
        Ok(retain_pending_file(root, path).await?.map(|file| Self {
            fingerprint: file.fingerprint,
            modified: file.modified,
        }))
    }
}

impl RetainedPendingFile {
    pub(super) fn identity(&self) -> std::io::Result<crate::fs_util::FileIdentity> {
        crate::fs_util::file_identity(&self.file)
    }

    pub(in crate::download) async fn validate(self: &Arc<Self>) -> anyhow::Result<()> {
        let retained = Arc::clone(self);
        tokio::task::spawn_blocking(move || retained.validate_blocking()).await?
    }

    pub(in crate::download) fn link_count(&self) -> std::io::Result<u64> {
        crate::fs_util::file_link_count(&self.file)
    }

    /// Recheck held bytes, mtime and namespace before finalization.
    pub(in crate::download) fn validate_blocking(&self) -> anyhow::Result<()> {
        let identity = crate::fs_util::file_identity(&self.file)?;
        let mut file = self.path.validate_identity(identity)?;
        anyhow::ensure!(
            file.metadata()?.modified().ok() == self.modified,
            "Reserved pending file modification time changed before adoption finalization"
        );
        anyhow::ensure!(
            fingerprint_open_file_snapshot_blocking(&mut file, self.path.path())?.fingerprint
                == self.fingerprint,
            "Reserved pending file changed before adoption finalization"
        );
        anyhow::ensure!(
            file.metadata()?.modified().ok() == self.modified,
            "Reserved pending file modification time changed during adoption verification"
        );
        self.path.validate_identity(identity)?;
        Ok(())
    }
}

/// A writer output hashed before opening the completion transaction.
pub(in crate::download) struct WriterSnapshot {
    path: ConfinedPath,
    file: std::fs::File,
    pub(in crate::download) fingerprint: ExistingFileFingerprint,
    #[cfg(unix)]
    change_time: (i64, i64),
}

#[cfg(unix)]
fn change_time(file: &std::fs::File) -> std::io::Result<(i64, i64)> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata()?;
    Ok((metadata.ctime(), metadata.ctime_nsec()))
}

#[cfg(test)]
tokio::task_local! {
    pub(in crate::download) static WRITER_HASH_HOOK: Arc<dyn Fn(&Path) + Send + Sync>;
}

impl WriterSnapshot {
    pub(in crate::download) async fn read(root: &Path, path: &Path) -> anyhow::Result<Self> {
        Self::read_with_timing(root, path, HashTiming::WaitForTick).await
    }

    /// A freshly changed file can use the normal two hashes immediately.
    /// Don't add a one-second wait to the backfill hot path.
    #[cfg(feature = "xmp")]
    pub(in crate::download) async fn for_sidecar(path: &Path) -> Option<Self> {
        Self::read_with_timing(path.parent()?, path, HashTiming::AlreadySettled)
            .await
            .ok()
    }

    #[cfg(feature = "xmp")]
    pub(in crate::download) fn source_handle(&self) -> std::io::Result<std::fs::File> {
        self.file.try_clone()
    }

    async fn read_with_timing(
        root: &Path,
        path: &Path,
        timing: HashTiming,
    ) -> anyhow::Result<Self> {
        #[cfg(windows)]
        let _ = timing;
        let root = root.to_owned();
        let path = path.to_owned();
        #[cfg(test)]
        let hook = WRITER_HASH_HOOK.try_with(Arc::clone).ok();
        tokio::task::spawn_blocking(move || {
            let path = ConfinedPath::open(&root, &path, ConfinedParents::Existing)?;
            #[cfg(windows)]
            let mut file = path.open_stable_read()?;
            #[cfg(unix)]
            let mut file = path.open_regular()?;
            #[cfg(unix)]
            let change_time = {
                let stamp = change_time(&file)?;
                // Coarse or unknown timestamps cannot distinguish a same-tick edit.
                anyhow::ensure!(
                    stamp.1 > 0,
                    "No subsecond inode change time for writer reuse"
                );
                let changed = chrono::DateTime::from_timestamp(stamp.0, u32::try_from(stamp.1)?)
                    .context("Invalid inode change time")?;
                let age = (chrono::Utc::now() - changed)
                    .to_std()
                    .context("Future inode change time")?;
                // Start hashing outside the observed timestamp's clock tick.
                // Older library files never wait. An edit before the hash starts
                // must still pass the writer's SHA-256 check.
                if let Some(remaining) = std::time::Duration::from_secs(1).checked_sub(age) {
                    anyhow::ensure!(
                        matches!(timing, HashTiming::WaitForTick),
                        "Writer input changed recently"
                    );
                    std::thread::sleep(remaining);
                }
                stamp
            };
            #[cfg(test)]
            if let Some(hook) = hook {
                hook(path.path());
            }
            let fingerprint =
                fingerprint_open_file_snapshot_blocking(&mut file, path.path())?.fingerprint;
            Ok(Self {
                path,
                file,
                fingerprint,
                #[cfg(unix)]
                change_time,
            })
        })
        .await?
    }

    /// Bounded namespace and change checks only. Never read file contents here.
    pub(in crate::download) fn still_matches(&self) -> anyhow::Result<()> {
        let current = self
            .path
            .validate_identity(crate::fs_util::file_identity(&self.file)?)?;
        #[cfg(unix)]
        anyhow::ensure!(
            change_time(&current)? == self.change_time,
            "Writer output changed"
        );
        #[cfg(windows)]
        drop(current); // The retained handle still excludes writes and replacement.
        Ok(())
    }
}

enum HashTiming {
    WaitForTick,
    #[cfg(feature = "xmp")]
    AlreadySettled,
}

/// Missing regular files may be downloaded; unsafe entries must not be adopted.
pub(in crate::download) async fn retain_pending_file(
    root: &Path,
    path: &Path,
) -> anyhow::Result<Option<Arc<RetainedPendingFile>>> {
    let root = root.to_path_buf();
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let confined = match ConfinedPath::open(&root, &path, ConfinedParents::Existing) {
            Ok(confined) => confined,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let Some(mut file) = confined.open_optional_regular()? else {
            return Ok(None);
        };
        let identity = crate::fs_util::file_identity(&file)?;
        // Keep the filesystem time from the same confined handle as the bytes.
        // An unavailable time remains unknown; it is never replaced with now.
        let modified = file.metadata()?.modified().ok();
        let fingerprint = fingerprint_open_file_snapshot_blocking(&mut file, &path)?.fingerprint;
        anyhow::ensure!(
            file.metadata()?.modified().ok() == modified,
            "Reserved pending file modification time changed while reading"
        );
        confined.validate_identity(identity)?;
        Ok(Some(Arc::new(RetainedPendingFile {
            path: confined,
            file,
            fingerprint,
            modified,
        })))
    })
    .await?
}

/// Import's durable-ownership exception uses the same confined, same-handle
/// fingerprint as sync. A matching hash through a symlink is not ownership proof.
pub(crate) async fn imported_path_matches_receipt(
    root: &Path,
    path: &Path,
    expected_size: u64,
    local_checksum: &str,
) -> bool {
    let Ok(fingerprint) = fingerprint_downloaded_path(root, path).await else {
        return false;
    };
    fingerprint.size == expected_size
        && data_encoding::HEXLOWER.encode(&fingerprint.sha256) == local_checksum
}

fn fingerprint_file_blocking(path: &Path) -> anyhow::Result<ExistingFileFingerprint> {
    Ok(fingerprint_file_snapshot_blocking(path)?.fingerprint)
}

fn fingerprint_file_snapshot_blocking(path: &Path) -> anyhow::Result<ExistingFileSnapshot> {
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("Could not open {} for SHA-256", path.display()))?;
    fingerprint_open_file_snapshot_blocking(&mut file, path)
}

pub(in crate::download) fn fingerprint_open_file_snapshot_blocking(
    file: &mut std::fs::File,
    path: &Path,
) -> anyhow::Result<ExistingFileSnapshot> {
    use sha2::{Digest, Sha256};

    let mut size = 0_u64;
    let mut sha256 = Sha256::new();
    let mut prefix = [0_u8; 12];
    let mut prefix_len = 0_usize;
    // 64 KiB reduces read() syscalls ~8x vs 8 KiB on multi-GB videos
    // without meaningful RSS impact on the blocking pool.
    let mut buf = [0u8; 65536];
    loop {
        use std::io::Read;
        let n = file
            .read(&mut buf)
            .with_context(|| format!("Could not read {} for SHA-256", path.display()))?;
        if n == 0 {
            break;
        }
        size = size
            .checked_add(n as u64)
            .with_context(|| format!("Could not size {} for SHA-256", path.display()))?;
        if prefix_len < prefix.len() {
            let copy_len = (prefix.len() - prefix_len).min(n);
            #[allow(
                clippy::indexing_slicing,
                reason = "copy_len is bounded by both prefix and the bytes returned into buf"
            )]
            prefix[prefix_len..prefix_len + copy_len].copy_from_slice(&buf[..copy_len]);
            prefix_len += copy_len;
        }
        #[allow(
            clippy::indexing_slicing,
            reason = "`n` is bounded by buf.len() because read() returns bytes written"
        )]
        sha256.update(&buf[..n]);
    }
    Ok(ExistingFileSnapshot {
        fingerprint: ExistingFileFingerprint {
            size,
            sha256: sha256.finalize().into(),
        },
        prefix,
        prefix_len,
    })
}

/// Fingerprint a replacement entry and reject links or special files.
pub(in crate::download) async fn fingerprint_regular_file(
    path: &Path,
) -> anyhow::Result<ExistingFileFingerprint> {
    let before = fs::symlink_metadata(path)
        .await
        .with_context(|| format!("Could not inspect replacement file {}", path.display()))?;
    anyhow::ensure!(
        before.file_type().is_file(),
        "Replacement path is not a regular file: {}",
        path.display()
    );
    let fingerprint = fingerprint_file(path).await?;
    let after = fs::symlink_metadata(path)
        .await
        .with_context(|| format!("Could not recheck replacement file {}", path.display()))?;
    anyhow::ensure!(
        after.file_type().is_file(),
        "Replacement path changed away from a regular file: {}",
        path.display()
    );
    Ok(fingerprint)
}

pub(in crate::download) fn fingerprint_regular_file_snapshot_blocking(
    path: &Path,
) -> anyhow::Result<ExistingFileSnapshot> {
    let before = std::fs::symlink_metadata(path)
        .with_context(|| format!("Could not inspect replacement file {}", path.display()))?;
    anyhow::ensure!(
        before.file_type().is_file(),
        "Replacement path is not a regular file: {}",
        path.display()
    );
    let snapshot = fingerprint_file_snapshot_blocking(path)?;
    let after = std::fs::symlink_metadata(path)
        .with_context(|| format!("Could not recheck replacement file {}", path.display()))?;
    anyhow::ensure!(
        after.file_type().is_file(),
        "Replacement path changed away from a regular file: {}",
        path.display()
    );
    Ok(snapshot)
}

pub(super) fn fingerprint_regular_file_blocking(
    path: &Path,
) -> anyhow::Result<ExistingFileFingerprint> {
    Ok(fingerprint_regular_file_snapshot_blocking(path)?.fingerprint)
}

#[cfg(test)]
mod tests;
