//! Read-only file and path guards for retained historical work.
use super::{DownloadConfig, DownloadStore};
use crate::state::LegacyFileEvidence;
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

/// Recovery scans a shared root, so its deny set must include every library.
pub(crate) async fn protected_replacement_paths(
    db: Option<&dyn DownloadStore>,
) -> anyhow::Result<Vec<PathBuf>> {
    let Some(db) = db else { return Ok(Vec::new()) };
    Ok(db.protected_legacy_paths().await?)
}

/// Shared originals remain unavailable to ordinary adoption/skip even when a
/// child has a historical catalogue receipt for the same bytes. Managed primary
/// ownership is checked separately by its exact slot owner.
pub(in crate::download) async fn current_path_is_independent(
    db: &dyn DownloadStore,
    path: &Path,
) -> anyhow::Result<bool> {
    Ok(db.source_metadata_recovery(path).await?
        != crate::state::db::SourceMetadataRecovery::ProtectedLegacy
        && db.source_metadata_recovery(&sidecar(path)).await?
            != crate::state::db::SourceMetadataRecovery::ProtectedLegacy)
}

fn sidecar(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".xmp");
    PathBuf::from(name)
}

/// Strict current-file evidence for recovery paths without a hardlink policy.
pub(in crate::download) async fn strict_current_file(
    config: &DownloadConfig,
    path: &Path,
    required: bool,
    cancel: &CancellationToken,
) -> anyhow::Result<LegacyFileEvidence> {
    fingerprint(
        &config.directory,
        path,
        required,
        &config.temp_suffix,
        cancel,
    )
    .await
}

async fn fingerprint(
    root: &Path,
    path: &Path,
    required: bool,
    temp_suffix: &str,
    cancel: &CancellationToken,
) -> anyhow::Result<LegacyFileEvidence> {
    anyhow::ensure!(!cancel.is_cancelled(), "Legacy preservation cancelled");
    anyhow::ensure!(
        !super::file::has_replacement_journal(root, path).await?,
        "Legacy replacement recovery is pending"
    );
    let root_owned = root.to_path_buf();
    let path_owned = path.to_path_buf();
    let suffix = temp_suffix.to_owned();
    let present = tokio::task::spawn_blocking(move || -> anyhow::Result<bool> {
        let confined = crate::fs_util::ConfinedPath::open(
            &root_owned,
            &path_owned,
            crate::fs_util::ConfinedParents::Existing,
        )?;
        for suffix in [suffix.as_str(), ".part"] {
            let mut temporary = path_owned.as_os_str().to_owned();
            temporary.push(suffix);
            anyhow::ensure!(
                !crate::fs_util::ConfinedPath::open(
                    &root_owned,
                    Path::new(&temporary),
                    crate::fs_util::ConfinedParents::Existing
                )?
                .entry_exists()?,
                "Legacy temporary write is pending"
            );
        }
        let file = confined.open_optional_regular()?;
        if let Some(file) = &file {
            let links = crate::fs_util::file_link_count(file)?;
            anyhow::ensure!(
                links == 1,
                "Strict recovery requires an independent file object"
            );
        }
        Ok(file.is_some())
    })
    .await??;
    anyhow::ensure!(present || !required, "Legacy original is missing");
    let value = if present {
        Some(super::file::fingerprint_downloaded_path(root, path).await?)
    } else {
        None
    };
    anyhow::ensure!(!cancel.is_cancelled(), "Legacy preservation cancelled");
    Ok(LegacyFileEvidence {
        root: root.to_path_buf(),
        path: path.to_path_buf(),
        sha256: value.map(|value| data_encoding::HEXLOWER.encode(&value.sha256)),
        size: value.map(|value| value.size),
    })
}

#[cfg(test)]
mod tests {
    use super::{fingerprint, sidecar};
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn strict_file_evidence_detects_changes_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let path = root.join("original.jpg");
        std::fs::write(&path, b"original bytes").unwrap();
        let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
        let cancel = CancellationToken::new();
        let original = fingerprint(root, &path, true, ".kei-tmp", &cancel)
            .await
            .unwrap();
        let absent = fingerprint(root, &sidecar(&path), false, ".kei-tmp", &cancel)
            .await
            .unwrap();
        assert_eq!(
            fingerprint(root, &path, true, ".kei-tmp", &cancel)
                .await
                .unwrap(),
            original
        );
        assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), mtime);
        assert!(absent.sha256.is_none());
        assert!(!sidecar(&path).exists());
        std::fs::write(sidecar(&path), b"new sidecar").unwrap();
        assert_ne!(
            fingerprint(root, &sidecar(&path), false, ".kei-tmp", &cancel)
                .await
                .unwrap(),
            absent
        );
        std::fs::write(&path, b"changed bytes").unwrap();
        assert_ne!(
            fingerprint(root, &path, true, ".kei-tmp", &cancel)
                .await
                .unwrap(),
            original
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"changed bytes");
        assert_eq!(std::fs::read(sidecar(&path)).unwrap(), b"new sidecar");
        cancel.cancel();
        assert!(
            fingerprint(root, &path, true, ".kei-tmp", &cancel)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn preservation_file_evidence_rejects_links_and_pending_writes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let path = root.join("original.jpg");
        std::fs::write(&path, b"original").unwrap();
        let cancel = CancellationToken::new();
        let linked = root.join("alias.jpg");
        std::fs::hard_link(&path, &linked).unwrap();
        assert!(
            fingerprint(root, &path, true, ".kei-tmp", &cancel)
                .await
                .is_err()
        );
        std::fs::remove_file(&linked).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&path, &linked).unwrap();
            assert!(
                fingerprint(root, &linked, true, ".kei-tmp", &cancel)
                    .await
                    .is_err()
            );
        }
        let temporary = root.join("original.jpg.kei-tmp");
        std::fs::write(&temporary, b"pending").unwrap();
        assert!(
            fingerprint(root, &path, true, ".kei-tmp", &cancel)
                .await
                .is_err()
        );
        assert_eq!(std::fs::read(&temporary).unwrap(), b"pending");
        assert_eq!(std::fs::read(&path).unwrap(), b"original");
    }

    #[tokio::test]
    async fn strict_file_evidence_rejects_missing_and_unconfined_paths() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let path = outside.path().join("foreign.jpg");
        std::fs::write(&path, b"outside unchanged").unwrap();
        let cancel = CancellationToken::new();
        assert!(
            fingerprint(root.path(), &path, true, ".kei-tmp", &cancel)
                .await
                .is_err()
        );
        assert!(
            fingerprint(
                root.path(),
                &root.path().join("missing.jpg"),
                true,
                ".kei-tmp",
                &cancel
            )
            .await
            .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"outside unchanged");
    }
}
