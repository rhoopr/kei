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

/// Keep the exact confined file and namespace alive through pending adoption.
#[derive(Debug)]
pub(in crate::download) struct RetainedPendingFile {
    path: ConfinedPath,
    file: std::fs::File,
    pub(in crate::download) fingerprint: ExistingFileFingerprint,
}

impl RetainedPendingFile {
    pub(super) fn identity(&self) -> std::io::Result<crate::fs_util::FileIdentity> {
        crate::fs_util::file_identity(&self.file)
    }

    pub(in crate::download) async fn validate(self: &Arc<Self>) -> anyhow::Result<()> {
        let retained = Arc::clone(self);
        tokio::task::spawn_blocking(move || retained.validate_blocking()).await?
    }

    /// Compare only the explicitly supported provider v1 signature over the
    /// complete retained file. Unknown formats never become a positive match.
    pub(in crate::download) async fn matches_provider_v1(
        self: &Arc<Self>,
        checksum: &str,
    ) -> anyhow::Result<Option<bool>> {
        let retained = Arc::clone(self);
        let checksum = checksum.to_owned();
        tokio::task::spawn_blocking(move || retained.matches_provider_v1_blocking(&checksum))
            .await?
    }

    pub(in crate::download) fn matches_provider_v1_blocking(
        &self,
        checksum: &str,
    ) -> anyhow::Result<Option<bool>> {
        let Some(expected) = super::provider_signature::supported(checksum) else {
            return Ok(None);
        };
        let identity = crate::fs_util::file_identity(&self.file)?;
        let mut file = self.path.validate_identity(identity)?;
        let (matches, fingerprint) = super::provider_signature::compare(&mut file, &expected)?;
        anyhow::ensure!(
            fingerprint == self.fingerprint,
            "Existing file changed during provider signature comparison"
        );
        self.path.validate_identity(identity)?;
        Ok(Some(matches))
    }

    pub(in crate::download) fn link_count(&self) -> std::io::Result<u64> {
        crate::fs_util::file_link_count(&self.file)
    }

    /// Recheck held bytes and namespace inside the owning state transaction.
    pub(in crate::download) fn validate_blocking(&self) -> anyhow::Result<()> {
        let identity = crate::fs_util::file_identity(&self.file)?;
        let mut file = self.path.validate_identity(identity)?;
        anyhow::ensure!(
            fingerprint_open_file_snapshot_blocking(&mut file, self.path.path())?.fingerprint
                == self.fingerprint,
            "Reserved pending file changed before adoption finalization"
        );
        self.path.validate_identity(identity)?;
        Ok(())
    }
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
        let fingerprint = fingerprint_open_file_snapshot_blocking(&mut file, &path)?.fingerprint;
        confined.validate_identity(identity)?;
        Ok(Some(Arc::new(RetainedPendingFile {
            path: confined,
            file,
            fingerprint,
        })))
    })
    .await?
}

/// Existence/namespace inspection only. Never content proof or adoption authority.
pub(in crate::download) async fn inspect_pending_candidate(
    root: &Path,
    path: &Path,
) -> anyhow::Result<bool> {
    let root = root.to_path_buf();
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let confined = match ConfinedPath::open(&root, &path, ConfinedParents::Existing) {
            Ok(confined) => confined,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        let Some(file) = confined.open_optional_regular()? else {
            return Ok(false);
        };
        confined.validate_identity(crate::fs_util::file_identity(&file)?)?;
        Ok(true)
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

#[cfg(test)]
mod provider_signature_tests {
    use super::*;
    use sha1::{Digest, Sha1};

    fn provider_signature(bytes: &[u8]) -> String {
        let mut hash = Sha1::new();
        hash.update(b"com.apple.XattrObjectSalt\0com.apple.DataObjectSalt\0");
        hash.update(bytes);
        data_encoding::BASE64.encode(&[vec![1], hash.finalize().to_vec()].concat())
    }

    #[tokio::test]
    async fn retained_provider_signature_checks_whole_bytes_and_rechecks_before_adoption() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("synthetic.JPG");
        let bytes = vec![61_u8; 65_537];
        std::fs::write(&path, &bytes).unwrap();
        let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
        let file = retain_pending_file(dir.path(), &path)
            .await
            .unwrap()
            .unwrap();
        let expected = provider_signature(&bytes);
        assert_eq!(
            file.matches_provider_v1(&expected).await.unwrap(),
            Some(true)
        );
        assert_eq!(
            file.matches_provider_v1("opaque-unknown").await.unwrap(),
            None
        );
        let mut wrong = bytes.clone();
        *wrong.last_mut().unwrap() ^= 1;
        assert_eq!(
            file.matches_provider_v1(&provider_signature(&wrong))
                .await
                .unwrap(),
            Some(false)
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), mtime);
        std::fs::write(&path, &wrong).unwrap();
        assert!(file.matches_provider_v1(&expected).await.is_err());
        assert!(file.validate().await.is_err());
    }
}
