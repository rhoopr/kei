//! Confined local copies and retained verification through state finalization.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;

use crate::fs_util::{ConfinedParents, ConfinedPath, FileIdentity, file_identity};

use super::fingerprint::{ExistingFileFingerprint, fingerprint_open_file_snapshot_blocking};
use super::platform::{PublishResult, publish_reconciliation_part_blocking};

/// Validate source and destination ancestors before path-aware planning.
#[must_use = "unsafe reconciliation paths must keep the old state path"]
pub(in crate::download) async fn validate_reconciliation_paths(
    root: &Path,
    source: Option<&Path>,
    destination: &Path,
) -> anyhow::Result<()> {
    let root = root.to_path_buf();
    let source = source.map(Path::to_path_buf);
    let destination = destination.to_path_buf();
    tokio::task::spawn_blocking(move || {
        if let Some(source) = source {
            let source_root = reconciliation_source_root(&root, &source)?;
            match ConfinedPath::open(&source_root, &source, ConfinedParents::Existing)
                .and_then(|path| path.open_optional_regular())
            {
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        ConfinedPath::open(&root, &destination, ConfinedParents::Create)?
            .open_optional_regular()?;
        Ok(())
    })
    .await?
}

/// Retain verified files and directory capabilities through state finalization.
#[derive(Debug)]
pub(crate) struct ReconciledFile {
    pub(in crate::download) source: ConfinedPath,
    pub(in crate::download) destination: ConfinedPath,
    source_file: std::fs::File,
    destination_file: std::fs::File,
    checksum: String,
}

impl ReconciledFile {
    pub(in crate::download) fn checksum(&self) -> &str {
        &self.checksum
    }

    /// Both paths have passed confined traversal and absolute normalization.
    #[must_use]
    pub(in crate::download) fn is_same_path(&self) -> bool {
        self.source.path() == self.destination.path()
    }

    pub(in crate::download) async fn validate(self: &Arc<Self>) -> anyhow::Result<()> {
        let copy = Arc::clone(self);
        tokio::task::spawn_blocking(move || copy.validate_blocking()).await?
    }

    pub(in crate::download) async fn set_capture_time(
        self: &Arc<Self>,
        timestamp: i64,
    ) -> anyhow::Result<()> {
        let copy = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            copy.validate_blocking()?;
            let file = copy
                .destination
                .validate_for_metadata(file_identity(&copy.destination_file)?)?;
            anyhow::ensure!(
                file_identity(&copy.source_file)? != file_identity(&file)?,
                "Reconciliation destination aliases the source file"
            );
            use std::time::{Duration, UNIX_EPOCH};
            let duration = Duration::from_secs(timestamp.unsigned_abs());
            let time = if timestamp >= 0 {
                UNIX_EPOCH
                    .checked_add(duration)
                    .context("Capture mtime is out of range")?
            } else {
                UNIX_EPOCH.checked_sub(duration).unwrap_or(UNIX_EPOCH)
            };
            file.set_times(
                std::fs::FileTimes::new()
                    .set_modified(time)
                    .set_accessed(time),
            )?;
            #[cfg(not(windows))]
            file.sync_all()?;
            copy.destination
                .validate_identity(file_identity(&copy.destination_file)?)?;
            Ok(())
        })
        .await?
    }

    #[cfg(feature = "xmp")]
    pub(in crate::download) fn open_source_for_metadata(&self) -> anyhow::Result<std::fs::File> {
        let mut file = self
            .source
            .validate_identity(file_identity(&self.source_file)?)?;
        let fingerprint =
            fingerprint_open_file_snapshot_blocking(&mut file, self.source.path())?.fingerprint;
        anyhow::ensure!(
            data_encoding::HEXLOWER.encode(&fingerprint.sha256) == self.checksum,
            "Reconciliation source changed before metadata planning"
        );
        Ok(file)
    }

    pub(in crate::download) fn validate_blocking(&self) -> anyhow::Result<()> {
        for (path, retained) in [
            (&self.source, &self.source_file),
            (&self.destination, &self.destination_file),
        ] {
            let identity = file_identity(retained)?;
            let mut file = path.validate_identity(identity)?;
            let fingerprint =
                fingerprint_open_file_snapshot_blocking(&mut file, path.path())?.fingerprint;
            anyhow::ensure!(
                data_encoding::HEXLOWER.encode(&fingerprint.sha256) == self.checksum,
                "Reconciliation media changed before state finalization"
            );
            path.validate_identity(identity)?;
        }
        Ok(())
    }
}

/// Copy regular media without following leaf symlinks or replacing destinations.
///
/// Returns `None` for different destination bytes. Unsafe entries and failed
/// validation return an error; callers must preserve the previous state path.
#[must_use = "only verified reconciliation destinations may be recorded in state"]
pub(crate) async fn copy_local_file_no_replace(
    root: &Path,
    source: &Path,
    destination: &Path,
    temp_suffix: &str,
) -> anyhow::Result<Option<Arc<ReconciledFile>>> {
    let root = root.to_path_buf();
    let source = source.to_path_buf();
    let destination = destination.to_path_buf();
    let temp_suffix = temp_suffix.to_owned();
    tokio::task::spawn_blocking(move || {
        copy_local_file_no_replace_blocking(
            &root,
            &source,
            &destination,
            &temp_suffix,
            |_| {},
            || {},
        )
    })
    .await?
}

fn copy_local_file_no_replace_blocking(
    root: &Path,
    source: &Path,
    destination: &Path,
    temp_suffix: &str,
    before_publication: impl FnOnce(&Path),
    after_publication: impl FnOnce(),
) -> anyhow::Result<Option<Arc<ReconciledFile>>> {
    let source_root = reconciliation_source_root(root, source)?;
    let source = ConfinedPath::open(&source_root, source, ConfinedParents::Existing)?;
    let destination = ConfinedPath::open(root, destination, ConfinedParents::Create)?;
    let mut source_file = source.open_regular()?;
    let source_identity = file_identity(&source_file)?;
    let source_fingerprint =
        fingerprint_open_file_snapshot_blocking(&mut source_file, source.path())?.fingerprint;
    if destination.open_optional_regular()?.is_some() {
        return finish_reconciled_copy(source, source_file, destination, source_fingerprint, None);
    }
    // Create a new, unpredictable name. Never truncate or remove a pre-existing
    // entry. Retain ambiguous temporary entries rather than risking user bytes.
    let part_path = destination.path().with_file_name(format!(
        ".kei-reconcile-{}{temp_suffix}",
        uuid::Uuid::new_v4()
    ));
    let part_path = destination.sibling(&part_path)?;
    let mut part = part_path.create_new_regular()?;
    let part_identity = file_identity(&part)?;
    use std::io::Seek;
    source_file.rewind()?;
    std::io::copy(&mut source_file, &mut part)?;
    part.set_permissions(source_file.metadata()?.permissions())?;
    part.sync_all()?;
    part.rewind()?;
    anyhow::ensure!(
        fingerprint_open_file_snapshot_blocking(&mut part, part_path.path())?.fingerprint
            == source_fingerprint,
        "Local path reconciliation checksum mismatch"
    );
    source_file.rewind()?;
    anyhow::ensure!(
        file_identity(&source.validate_identity(source_identity)?)? == source_identity
            && fingerprint_open_file_snapshot_blocking(&mut source_file, source.path())?
                .fingerprint
                == source_fingerprint,
        "Reconciliation source changed during copy"
    );
    before_publication(part_path.path());
    anyhow::ensure!(
        file_identity(&part_path.validate_identity(part_identity)?)? == part_identity,
        "Reconciliation temporary entry changed before publication"
    );
    let publication = publish_reconciliation_part_blocking(&part_path, &destination)?;
    after_publication();
    let expected_identity = match publication {
        PublishResult::Published => Some(part_identity),
        PublishResult::DestinationExists => None,
    };
    if part_path.entry_exists()? {
        tracing::warn!(target: "kei::download::file",path = %part_path.path().display(), "Retaining reconciliation temporary file for manual inspection");
    }
    finish_reconciled_copy(
        source,
        source_file,
        destination,
        source_fingerprint,
        expected_identity,
    )
}

fn finish_reconciled_copy(
    source: ConfinedPath,
    source_file: std::fs::File,
    destination: ConfinedPath,
    expected: ExistingFileFingerprint,
    expected_identity: Option<FileIdentity>,
) -> anyhow::Result<Option<Arc<ReconciledFile>>> {
    let mut file = destination.open_regular()?;
    let identity = file_identity(&file)?;
    anyhow::ensure!(
        expected_identity.is_none_or(|expected| expected == identity),
        "Reconciled destination identity changed"
    );
    let actual =
        fingerprint_open_file_snapshot_blocking(&mut file, destination.path())?.fingerprint;
    anyhow::ensure!(
        file_identity(&destination.validate_identity(identity)?)? == identity,
        "Reconciled destination changed while hashing"
    );
    if actual != expected {
        return Ok(None);
    }
    destination.sync_parent()?;
    Ok(Some(Arc::new(ReconciledFile {
        source,
        destination,
        source_file,
        destination_file: file,
        checksum: data_encoding::HEXLOWER.encode(&actual.sha256),
    })))
}

fn reconciliation_source_root(download_root: &Path, source: &Path) -> anyhow::Result<PathBuf> {
    let download_root = crate::fs_util::absolute_confined_path(download_root)?;
    let source = crate::fs_util::absolute_confined_path(source)?;
    if source.starts_with(&download_root) {
        return Ok(download_root);
    }
    let source_parent = source.parent().ok_or_else(|| {
        anyhow::anyhow!(
            "Cannot confine reconciliation source without a parent: {}",
            source.display()
        )
    })?;
    Ok(source_parent
        .ancestors()
        .find(|ancestor| download_root.starts_with(ancestor))
        .unwrap_or(source_parent)
        .to_path_buf())
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::path::PathBuf;

    use super::super::fingerprint::compute_sha256;
    use super::copy_local_file_no_replace;
    #[cfg(unix)]
    use super::copy_local_file_no_replace_blocking;

    #[tokio::test]
    async fn local_reconciliation_copy_preserves_source_and_refuses_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.jpg");
        let destination = dir.path().join("nested/destination.jpg");
        std::fs::write(&source, b"catalog bytes").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o640)).unwrap();
        }

        let copied = copy_local_file_no_replace(dir.path(), &source, &destination, ".part")
            .await
            .unwrap();
        assert!(copied.is_some());
        assert_eq!(std::fs::read(&source).unwrap(), b"catalog bytes");
        assert_eq!(std::fs::read(&destination).unwrap(), b"catalog bytes");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&destination)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o640
            );
        }

        std::fs::write(&destination, b"user bytes").unwrap();
        let conflict = copy_local_file_no_replace(dir.path(), &source, &destination, ".part")
            .await
            .unwrap();
        assert!(conflict.is_none());
        assert_eq!(std::fs::read(&destination).unwrap(), b"user bytes");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reconciliation_leaf_failed_copy_retains_private_partial_file() {
        use std::os::unix::fs::PermissionsExt;

        const CHILD_DIRECTORY: &str = "KEI_TEST_PRIVATE_RECONCILIATION_COPY";
        const SOURCE_BYTES: &[u8] = &[0x5a; 8192];
        if let Some(directory) = std::env::var_os(CHILD_DIRECTORY) {
            let directory = PathBuf::from(directory);
            let error = copy_local_file_no_replace(
                &directory,
                &directory.join("source.jpg"),
                &directory.join("destination.jpg"),
                ".part",
            )
            .await
            .unwrap_err();
            assert_eq!(
                error
                    .downcast_ref::<std::io::Error>()
                    .unwrap()
                    .raw_os_error(),
                Some(libc::EFBIG)
            );
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.jpg");
        let destination = dir.path().join("destination.jpg");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(&source, SOURCE_BYTES).unwrap();
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o600)).unwrap();

        // Isolate the umask, file-size limit, and signal disposition from other
        // tests. Ignore SIGXFSZ so the real copy returns an error after writing.
        let output = std::process::Command::new("sh")
            .args([
                "-c",
                "umask 022; trap '' XFSZ; ulimit -f 2; exec \"$@\"",
                "reconciliation-copy-test",
            ])
            .arg(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "download::file::reconciliation::tests::reconciliation_leaf_failed_copy_retains_private_partial_file",
                "--nocapture",
            ])
            .env(CHILD_DIRECTORY, dir.path())
            // The injected limit must not leave a truncated LLVM profile for
            // the parent coverage run to merge. Only discard the child's data.
            .env("LLVM_PROFILE_FILE", "/dev/null")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "child failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        assert!(!destination.exists());
        let partials: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "part"))
            .collect();
        assert_eq!(partials.len(), 1);
        let partial = &partials[0];
        let partial_bytes = std::fs::read(partial).unwrap();
        assert!(!partial_bytes.is_empty());
        assert!(partial_bytes.len() < SOURCE_BYTES.len());
        assert_eq!(partial_bytes, SOURCE_BYTES[..partial_bytes.len()]);
        assert_eq!(
            std::fs::metadata(partial).unwrap().permissions().mode() & 0o777,
            0o600
        );

        // Retry without the limit, then reuse the published file unchanged.
        let mut file_count_after_retry = None;
        for _ in 0..2 {
            assert!(
                copy_local_file_no_replace(dir.path(), &source, &destination, ".part")
                    .await
                    .unwrap()
                    .is_some()
            );
            for path in [&source, &destination] {
                assert_eq!(std::fs::read(path).unwrap(), SOURCE_BYTES);
                assert_eq!(
                    std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }
            assert_eq!(std::fs::read(partial).unwrap(), partial_bytes);
            // Hard-link publication can retain the successful temporary file.
            let file_count = std::fs::read_dir(dir.path()).unwrap().count();
            if let Some(previous_count) = file_count_after_retry {
                assert_eq!(file_count, previous_count);
            }
            file_count_after_retry = Some(file_count);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reconciliation_leaf_symlinks_and_special_entries_are_rejected() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.jpg");
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("target.jpg");
        let destination = dir.path().join("destination.jpg");
        std::fs::write(&source, b"catalog bytes").unwrap();
        std::fs::write(&target, b"catalog bytes").unwrap();
        for target_path in [&target, &outside.path().join("missing.jpg")] {
            symlink(target_path, &destination).unwrap();
            assert!(
                copy_local_file_no_replace(dir.path(), &source, &destination, ".part")
                    .await
                    .is_err()
            );
            assert_eq!(std::fs::read_link(&destination).unwrap(), *target_path);
            assert_eq!(std::fs::read(&target).unwrap(), b"catalog bytes");
            std::fs::remove_file(&destination).unwrap();
        }
        std::fs::create_dir(&destination).unwrap();
        assert!(
            copy_local_file_no_replace(dir.path(), &source, &destination, ".part")
                .await
                .is_err()
        );
        assert!(destination.is_dir());
        std::fs::remove_dir(&destination).unwrap();
        let linked_source = dir.path().join("source-link.jpg");
        symlink(&source, &linked_source).unwrap();
        assert!(
            copy_local_file_no_replace(dir.path(), &linked_source, &destination, ".part")
                .await
                .is_err()
        );
        assert!(!destination.exists());
        assert_eq!(std::fs::read(&source).unwrap(), b"catalog bytes");
    }

    #[cfg(unix)]
    #[test]
    fn reconciliation_leaf_publication_races_preserve_external_bytes() {
        use std::os::unix::fs::symlink;
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("target.jpg");
        std::fs::write(&target, b"catalog bytes").unwrap();
        for after_publish in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let source = dir.path().join("source.jpg");
            let destination = dir.path().join("destination.jpg");
            std::fs::write(&source, b"catalog bytes").unwrap();
            let result = copy_local_file_no_replace_blocking(
                dir.path(),
                &source,
                &destination,
                ".part",
                |_| {
                    if !after_publish {
                        symlink(&target, &destination).unwrap();
                    }
                },
                || {
                    if after_publish {
                        std::fs::remove_file(&destination).unwrap();
                        symlink(&target, &destination).unwrap();
                    }
                },
            );
            assert!(result.is_err());
            assert_eq!(std::fs::read_link(&destination).unwrap(), target);
            assert_eq!(std::fs::read(&target).unwrap(), b"catalog bytes");
            assert_eq!(std::fs::read(&source).unwrap(), b"catalog bytes");
        }
    }

    #[tokio::test]
    async fn reconciliation_leaf_identical_regular_destination_is_reusable() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.jpg");
        let destination = dir.path().join("destination.jpg");
        std::fs::write(&source, b"catalog bytes").unwrap();
        std::fs::write(&destination, b"catalog bytes").unwrap();
        let expected = compute_sha256(&source).await.unwrap();
        for _ in 0..2 {
            assert_eq!(
                copy_local_file_no_replace(dir.path(), &source, &destination, ".part")
                    .await
                    .unwrap()
                    .map(|copy| copy.checksum().to_owned()),
                Some(expected.clone())
            );
        }
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn reconciliation_leaf_replaced_temporary_file_is_not_published() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.jpg");
        let destination = dir.path().join("destination.jpg");
        std::fs::write(&source, b"catalog bytes").unwrap();
        let result = copy_local_file_no_replace_blocking(
            dir.path(),
            &source,
            &destination,
            ".part",
            |part| {
                std::fs::remove_file(part).unwrap();
                symlink(&source, part).unwrap();
            },
            || {},
        );
        assert!(result.is_err());
        assert!(!destination.exists());
        assert_eq!(std::fs::read(&source).unwrap(), b"catalog bytes");
    }

    #[cfg(unix)]
    #[test]
    fn reconciliation_confined_parent_publication_races_reject_external_redirects() {
        use std::os::unix::fs::symlink;
        for after_publish in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            let source = root.path().join("source.jpg");
            let parent = root.path().join("parent");
            let retained = root.path().join("retained");
            let destination = parent.join("destination.jpg");
            std::fs::create_dir(&parent).unwrap();
            std::fs::write(&source, b"media").unwrap();
            let replace_parent = || {
                std::fs::rename(&parent, &retained).unwrap();
                symlink(outside.path(), &parent).unwrap();
            };
            let result = copy_local_file_no_replace_blocking(
                root.path(),
                &source,
                &destination,
                ".part",
                |_| {
                    if !after_publish {
                        replace_parent();
                    }
                },
                || {
                    if after_publish {
                        replace_parent();
                    }
                },
            );
            assert!(result.is_err());
            assert_eq!(std::fs::read(&source).unwrap(), b"media");
            assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reconciliation_confined_receipt_detects_parent_change_before_state() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let source = root.path().join("source.jpg");
        let parent = root.path().join("parent");
        let destination = parent.join("destination.jpg");
        std::fs::write(&source, b"media").unwrap();
        let copied = copy_local_file_no_replace(root.path(), &source, &destination, ".part")
            .await
            .unwrap()
            .unwrap();
        copied.validate().await.unwrap();
        std::fs::rename(&parent, root.path().join("retained")).unwrap();
        std::os::unix::fs::symlink(outside.path(), &parent).unwrap();
        assert!(copied.validate().await.is_err());
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
        assert_eq!(std::fs::read(&source).unwrap(), b"media");
    }

    #[tokio::test]
    async fn reconciliation_confined_relative_root_reaches_steady_state() {
        let cwd = std::env::current_dir().unwrap();
        let root = tempfile::tempdir_in(&cwd).unwrap();
        let relative_root = root.path().strip_prefix(&cwd).unwrap();
        let source = root.path().join("source.jpg");
        let destination = relative_root.join("nested/destination.jpg");
        std::fs::write(&source, b"media").unwrap();
        for _ in 0..2 {
            let copy = copy_local_file_no_replace(relative_root, &source, &destination, ".part")
                .await
                .unwrap()
                .unwrap();
            copy.validate().await.unwrap();
            assert_eq!(copy.checksum(), compute_sha256(&source).await.unwrap());
        }
        assert_eq!(
            std::fs::read_dir(destination.parent().unwrap())
                .unwrap()
                .count(),
            1
        );
        assert_eq!(std::fs::read(&source).unwrap(), b"media");
    }
}
