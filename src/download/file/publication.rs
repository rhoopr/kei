//! No-overwrite media publication and identical-byte collision handling.

use std::path::{Path, PathBuf};

use anyhow::Context;
use tokio::fs;

use super::fingerprint::fingerprint_regular_file;
use super::platform::{PartName, PublishResult, publish_part_no_replace_keeping_link};
use super::replacement::{FinalPublication, replace_file_if_unchanged};

#[derive(Debug, thiserror::Error)]
#[error(
    "Refusing to deduplicate {part_path} with {final_path} because their verified bytes differ"
)]
pub(in crate::download) struct FinalPathCollision {
    part_path: PathBuf,
    final_path: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use = "only installed output can acquire measured metadata field ownership"]
pub(in crate::download) enum PublicationOutcome {
    Installed,
    IdenticalExisting,
}

/// Rename a `.part` file to its final destination, handling the case where
/// a concurrent download already placed identical bytes at the final path.
#[cfg(test)]
pub(in crate::download) async fn rename_part_to_final(
    part_path: &Path,
    final_path: &Path,
) -> anyhow::Result<()> {
    let (_, part) =
        publish_part_to_final(part_path, final_path, FinalPublication::NoReplace).await?;
    #[cfg(unix)]
    if part == PartName::Linked {
        super::platform::retire_linked_part(part_path).await;
    }
    #[cfg(not(unix))]
    let _ = part;
    Ok(())
}

/// Publish a verified `.part` file according to the task's explicit policy.
///
/// Link publication reports [`PartName::Linked`]: the caller owns the verified
/// handle on the `.part`, so it must close it and then retire the name.
pub(in crate::download) async fn publish_part_to_final(
    part_path: &Path,
    final_path: &Path,
    publication: FinalPublication,
) -> anyhow::Result<(PublicationOutcome, PartName)> {
    if let FinalPublication::ReplaceTruncated(expected) = publication {
        replace_file_if_unchanged(part_path, final_path, expected).await?;
        return Ok((PublicationOutcome::Installed, PartName::Gone));
    }
    if let FinalPublication::ReplaceLocal {
        fingerprint,
        modified,
    } = publication
    {
        super::replacement::replace_local_file_if_unchanged(
            part_path,
            final_path,
            fingerprint,
            modified,
        )
        .await?;
        return Ok((PublicationOutcome::Installed, PartName::Gone));
    }

    match publish_part_no_replace_keeping_link(part_path, final_path).await {
        Ok((PublishResult::Published, part)) => {
            // ext4 default `data=ordered` does not guarantee directory
            // entry durability after `rename` returns Ok: a power loss
            // between rename and the kernel committing the dir block
            // can leave `final_path` absent on reboot. Best-effort
            // fsync the parent so the worst case is one redundant
            // re-download next sync, not silent loss.
            crate::fs_util::fsync_parent_dir_async_best_effort(final_path).await;
            Ok((PublicationOutcome::Installed, part))
        }
        Ok((PublishResult::DestinationExists, _)) => {
            // CONTRACT: FILE_PUBLISH_NO_OVERWRITE
            let part_fingerprint = fingerprint_regular_file(part_path)
                .await
                .with_context(|| format!("Could not verify {}", part_path.display()))?;
            let final_fingerprint = fingerprint_regular_file(final_path)
                .await
                .with_context(|| format!("Could not verify {}", final_path.display()))?;
            if part_fingerprint != final_fingerprint {
                return Err(FinalPathCollision {
                    part_path: part_path.to_path_buf(),
                    final_path: final_path.to_path_buf(),
                }
                .into());
            }

            tracing::debug!(target: "kei::download::file",
                path = %final_path.display(),
                "Destination has identical bytes, removing redundant .part file"
            );
            fs::remove_file(part_path).await.with_context(|| {
                format!(
                    "Could not remove redundant completed download {}",
                    part_path.display()
                )
            })?;
            Ok((PublicationOutcome::IdenticalExisting, PartName::Gone))
        }
        Err(e) => {
            let cause = if hard_links_unsupported(&e) {
                "; the destination filesystem rejects both rename-without-replace and hard links, \
                 and kei needs hard-link support to publish without overwriting"
            } else {
                ""
            };
            Err(e).with_context(|| {
                format!(
                    "Could not move completed download from {} to {}{cause}",
                    part_path.display(),
                    final_path.display()
                )
            })
        }
    }
}

/// `link()` refusals that mean the filesystem has no hard links (`link(2)`
/// reports this as `EPERM`), not a transient or access failure.
fn hard_links_unsupported(error: &std::io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::EPERM | libc::EOPNOTSUPP | libc::ENOSYS)
    )
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::super::rename_part_to_final;
    use super::{FinalPathCollision, FinalPublication, PublicationOutcome, publish_part_to_final};

    #[tokio::test]
    async fn rename_part_to_final_happy_path() {
        let dir = TempDir::new().unwrap();
        let part = dir.path().join("photo.part");
        let final_path = dir.path().join("photo.jpg");
        tokio::fs::write(&part, b"image data").await.unwrap();

        let (outcome, part_name) =
            publish_part_to_final(&part, &final_path, FinalPublication::NoReplace)
                .await
                .unwrap();
        assert_eq!(outcome, PublicationOutcome::Installed);
        // The publisher reports a linked `.part` for the caller to retire, after
        // the caller has released its own handle on the file.
        #[cfg(unix)]
        if part_name == super::PartName::Linked {
            super::super::platform::retire_linked_part(&part).await;
        }
        #[cfg(not(unix))]
        let _ = part_name;

        assert!(!part.exists());
        assert!(final_path.exists());
        assert_eq!(tokio::fs::read(&final_path).await.unwrap(), b"image data");
    }

    #[tokio::test]
    async fn contract_file_publish_no_overwrite_destination_already_exists() {
        let dir = TempDir::new().unwrap();
        let part = dir.path().join("photo.part");
        let final_path = dir.path().join("photo.jpg");
        tokio::fs::write(&final_path, b"existing").await.unwrap();
        tokio::fs::write(&part, b"existing").await.unwrap();

        // Should succeed without replacing the existing final file. On Linux,
        // plain rename(old, new) would overwrite `photo.jpg`; the publish path
        // must use no-overwrite semantics before cleaning the redundant .part.
        let (outcome, _) = publish_part_to_final(&part, &final_path, FinalPublication::NoReplace)
            .await
            .unwrap();
        assert_eq!(outcome, PublicationOutcome::IdenticalExisting);

        assert!(!part.exists(), ".part should not remain");
        assert!(final_path.exists(), "final file should exist");
        assert_eq!(
            tokio::fs::read(&final_path).await.unwrap(),
            b"existing",
            "existing final file must not be replaced by duplicate .part bytes"
        );
    }

    #[tokio::test]
    async fn contract_file_publish_different_destination_returns_typed_collision() {
        let dir = TempDir::new().unwrap();
        let part = dir.path().join("photo.part");
        let final_path = dir.path().join("photo.jpg");
        tokio::fs::write(&final_path, b"winner").await.unwrap();
        tokio::fs::write(&part, b"loser").await.unwrap();

        let error = rename_part_to_final(&part, &final_path)
            .await
            .expect_err("different bytes must collide");

        assert!(error.downcast_ref::<FinalPathCollision>().is_some());
        assert_eq!(tokio::fs::read(&final_path).await.unwrap(), b"winner");
        assert_eq!(tokio::fs::read(&part).await.unwrap(), b"loser");
    }

    #[tokio::test]
    async fn rename_part_to_final_nonexistent_part_returns_error() {
        let dir = TempDir::new().unwrap();
        let part = dir.path().join("missing.part");
        let final_path = dir.path().join("photo.jpg");

        let result = rename_part_to_final(&part, &final_path).await;
        assert!(result.is_err(), "should fail when .part doesn't exist");
    }

    #[test]
    fn hard_link_refusals_are_recognised_and_other_errors_are_not() {
        for code in [libc::EPERM, libc::EOPNOTSUPP, libc::ENOSYS] {
            assert!(super::hard_links_unsupported(
                &std::io::Error::from_raw_os_error(code)
            ));
        }
        for code in [libc::EIO, libc::ENOSPC, libc::EACCES, libc::EEXIST] {
            assert!(!super::hard_links_unsupported(
                &std::io::Error::from_raw_os_error(code)
            ));
        }
    }

    /// Needs `KEI_NFS_NOLINK_TEST_DIR` on a filesystem that refuses `link()`:
    /// publication must fail, keep the verified `.part` intact, and name
    /// hard-link support as the cause. A filesystem that refuses `link()` here
    /// still accepts `RENAME_NOREPLACE` through the kernel's generic path, and
    /// NFS does the reverse, so the path-scoped seam supplies the flag
    /// rejection and the mount supplies the real refusal.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn nfs_export_without_hard_links_keeps_the_part_and_names_the_cause() {
        let Some(dir) = crate::test_helpers::no_link_test_dir("nfs_no_hard_links") else {
            return;
        };
        let _fallback = super::super::platform::ForcedLinkFallback::new(dir.path());
        let part = dir.path().join("photo.part");
        let final_path = dir.path().join("photo.jpg");
        tokio::fs::write(&part, b"verified bytes").await.unwrap();

        let error = rename_part_to_final(&part, &final_path)
            .await
            .expect_err("publication needs hard links on this export");

        let message = format!("{error:#}");
        assert!(message.contains("hard link"), "{message}");
        assert_eq!(tokio::fs::read(&part).await.unwrap(), b"verified bytes");
        assert!(!final_path.exists());
    }
}
