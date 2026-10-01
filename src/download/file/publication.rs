//! No-overwrite media publication and identical-byte collision handling.

use std::path::{Path, PathBuf};

use anyhow::Context;
use tokio::fs;

use super::fingerprint::fingerprint_regular_file;
use super::platform::{PublishResult, publish_part_no_replace};
use super::replacement::{FinalPublication, replace_file_if_unchanged};

#[derive(Debug, thiserror::Error)]
#[error(
    "Refusing to deduplicate {part_path} with {final_path} because their verified bytes differ"
)]
pub(in crate::download) struct FinalPathCollision {
    part_path: PathBuf,
    final_path: PathBuf,
}

/// Rename a `.part` file to its final destination, handling the case where
/// a concurrent download already placed identical bytes at the final path.
#[cfg(test)]
pub(in crate::download) async fn rename_part_to_final(
    part_path: &Path,
    final_path: &Path,
) -> anyhow::Result<()> {
    publish_part_to_final(part_path, final_path, FinalPublication::NoReplace).await
}

/// Publish a verified `.part` file according to the task's explicit policy.
pub(in crate::download) async fn publish_part_to_final(
    part_path: &Path,
    final_path: &Path,
    publication: FinalPublication,
) -> anyhow::Result<()> {
    if let FinalPublication::ReplaceTruncated(expected) = publication {
        return replace_file_if_unchanged(part_path, final_path, expected).await;
    }

    match publish_part_no_replace(part_path, final_path).await {
        Ok(PublishResult::Published) => {
            // ext4 default `data=ordered` does not guarantee directory
            // entry durability after `rename` returns Ok: a power loss
            // between rename and the kernel committing the dir block
            // can leave `final_path` absent on reboot. Best-effort
            // fsync the parent so the worst case is one redundant
            // re-download next sync, not silent loss.
            crate::fs_util::fsync_parent_dir_async_best_effort(final_path).await;
            Ok(())
        }
        Ok(PublishResult::DestinationExists) => {
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
            Ok(())
        }
        Err(e) => Err(e).with_context(|| {
            format!(
                "Could not move completed download from {} to {}",
                part_path.display(),
                final_path.display()
            )
        }),
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::super::rename_part_to_final;
    use super::FinalPathCollision;

    #[tokio::test]
    async fn rename_part_to_final_happy_path() {
        let dir = TempDir::new().unwrap();
        let part = dir.path().join("photo.part");
        let final_path = dir.path().join("photo.jpg");
        tokio::fs::write(&part, b"image data").await.unwrap();

        rename_part_to_final(&part, &final_path).await.unwrap();

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
        rename_part_to_final(&part, &final_path).await.unwrap();

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
}
