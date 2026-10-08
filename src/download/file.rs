//! File transfer, validation, and safe publication.
//!
//! Transfer owns HTTP retries and temporary-file writes. Validation owns media
//! and response checks; fingerprinting supplies same-read byte evidence.
//! Publication handles no-overwrite collisions. Replacement owns stable-input
//! checks and restoration. Reconciliation retains confined file capabilities.
//! Platform primitives stay below these policies. Child dependencies are one-way.
//!
//! Existing tests keep their names under `<owner>::tests`; HTTP wiremock tests
//! move to `transfer::tests::wiremock_tests`. Shared media bytes live in
//! `test_support`. The external facade paths and visibility stay unchanged.

mod fingerprint;
mod platform;
mod publication;
mod reconciliation;
mod replacement;
#[cfg(target_os = "linux")]
mod replacement_recovery;
#[cfg(target_os = "linux")]
use anyhow::Context;
mod staging;
mod transfer;
mod validation;

#[cfg(test)]
mod test_support;

pub(super) use fingerprint::ExistingFileFingerprint;
pub(crate) use fingerprint::compute_sha256;
pub(crate) use fingerprint::imported_path_matches_receipt;
pub(super) use fingerprint::{RetainedPendingFile, retain_pending_file};
pub(super) use fingerprint::{fingerprint_downloaded_path, fingerprint_regular_file};
#[cfg(test)]
pub(super) use publication::rename_part_to_final;
#[cfg_attr(
    not(feature = "xmp"),
    allow(
        unused_imports,
        reason = "Preserve the file facade type without XMP callers"
    )
)]
pub(crate) use reconciliation::ReconciledFile;
pub(crate) use reconciliation::copy_local_file_no_replace;
pub(super) use reconciliation::validate_reconciliation_paths;
pub(super) use replacement::ConditionalPublishTargetChanged;
pub(super) use replacement::FinalPublication;
pub(super) use replacement::classify_conditional_publish_error;
#[cfg(feature = "xmp")]
pub(super) use replacement::publish_file_if_unchanged;
pub(super) use replacement::publish_file_if_unchanged_blocking;
pub(super) use staging::{CleanupLeases, lock_download_destination};
pub(super) use transfer::DownloadClient;
pub(super) use transfer::DownloadLimits;
pub(super) use transfer::DownloadOpts;
pub(super) use transfer::download_file_with_mode;
pub(super) use transfer::temp_download_path;
pub(crate) use validation::LocalFileSizeExpectation;
pub(crate) use validation::local_file_size_matches_state;

// These paths remain available even when current sibling callers infer their types.
#[allow(
    unused_imports,
    reason = "Preserve the existing file facade API and visibility"
)]
pub(super) use self::{
    fingerprint::ExistingFileSnapshot, fingerprint::fingerprint_file,
    fingerprint::fingerprint_regular_file_snapshot_blocking, platform::PublishResult,
    platform::publish_reconciliation_part_blocking, publication::FinalPathCollision,
    publication::publish_part_to_final, replacement::ConditionalPublishErrorDisposition,
    transfer::DownloadResponse,
};

/// Recover interrupted fallback publications before normal download work.
#[cfg(target_os = "linux")]
pub(super) async fn recover_conditional_replacements(
    root: &std::path::Path,
    protected_paths: &[std::path::PathBuf],
) -> anyhow::Result<()> {
    replacement_recovery::recover_tree(root, protected_paths).await
}

#[cfg(not(target_os = "linux"))]
pub(super) async fn recover_conditional_replacements(
    _root: &std::path::Path,
    _protected_paths: &[std::path::PathBuf],
) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(all(target_os = "linux", feature = "xmp"))]
pub(super) fn cleanup_prepared_sidecar(
    path: &std::path::Path,
    expected: ExistingFileFingerprint,
    identity: crate::fs_util::FileIdentity,
) -> anyhow::Result<()> {
    replacement_recovery::cleanup_prepared(path, expected, identity)
}

#[cfg(target_os = "linux")]
pub(super) fn recover_file_replacement(path: &std::path::Path) -> anyhow::Result<()> {
    replacement_recovery::recover_target(path)
}

/// Recover the media and sidecar journals before inspecting either input.
pub(super) async fn recover_metadata_replacements(path: &std::path::Path) -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    {
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || {
            replacement_recovery::recover_target(&path)?;
            let mut name = path
                .file_name()
                .context("Metadata path has no filename")?
                .to_os_string();
            name.push(".xmp");
            replacement_recovery::recover_target(&path.with_file_name(name))
        })
        .await??;
    }
    #[cfg(not(target_os = "linux"))]
    let _ = path;
    Ok(())
}

/// Read-only prerequisite for preserving a target that recovery could otherwise change.
pub(super) async fn has_replacement_journal(
    root: &std::path::Path,
    target: &std::path::Path,
) -> anyhow::Result<bool> {
    #[cfg(target_os = "linux")]
    {
        let root = root.to_path_buf();
        let target = target.to_path_buf();
        tokio::task::spawn_blocking(move || -> anyhow::Result<bool> {
            let path = replacement_recovery::directory_path(&target)?;
            Ok(crate::fs_util::ConfinedPath::open(
                &root,
                &path,
                crate::fs_util::ConfinedParents::Existing,
            )?
            .entry_exists()?)
        })
        .await?
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (root, target);
        Ok(false)
    }
}
