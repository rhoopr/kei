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
pub(in crate::download) mod primary_layout;
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

#[cfg(all(test, unix, feature = "xmp"))]
pub(super) use fingerprint::WRITER_HASH_HOOK;
pub(crate) use fingerprint::compute_sha256;
pub(super) use fingerprint::fingerprint_open_file_snapshot_blocking;
pub(crate) use fingerprint::imported_path_matches_receipt;
pub(super) use fingerprint::{ExistingFileFingerprint, WriterSnapshot};
pub(super) use fingerprint::{RetainedPendingFile, retain_pending_file};
pub(super) use fingerprint::{fingerprint_downloaded_path, fingerprint_regular_file};
#[cfg(all(test, feature = "xmp", target_os = "linux"))]
pub(super) use platform::RECONCILIATION_RENAME_FAULT;
pub(super) use publication::PublicationOutcome;

/// Inspect confined regular bytes for an explicit integrity confirmation.
pub(crate) async fn inspect_integrity_fingerprint(
    root: &std::path::Path,
    path: &std::path::Path,
) -> anyhow::Result<(String, u64)> {
    let retained = retain_integrity_file(root, path).await?;
    Ok((
        data_encoding::HEXLOWER.encode(&retained.fingerprint.sha256),
        retained.fingerprint.size,
    ))
}

pub(super) async fn retain_integrity_file(
    root: &std::path::Path,
    path: &std::path::Path,
) -> anyhow::Result<std::sync::Arc<RetainedPendingFile>> {
    let path = crate::fs_util::absolute_confined_path(path)?;
    let root = crate::fs_util::absolute_confined_path(root)?;
    anyhow::ensure!(path.starts_with(&root), "Inspection outside destination");
    // An inspected path can come from a retained receipt. Its parent is
    // not an independent trust anchor: pin every ancestor of the file.
    let anchor = path
        .ancestors()
        .last()
        .ok_or_else(|| anyhow::anyhow!("Missing filesystem root"))?;
    retain_pending_file(anchor, &path)
        .await?
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound).into())
}
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
pub(super) use replacement::publish_file_if_unchanged_at_mtime_blocking;
pub(super) use replacement::publish_file_if_unchanged_blocking;
pub(super) use staging::{CleanupLeases, lock_download_destination};
pub(super) use transfer::DownloadClient;
pub(super) use transfer::DownloadLimits;
pub(super) use transfer::DownloadOpts;
pub(super) use transfer::download_file_with_mode;
pub(super) use transfer::temp_download_path;
pub(crate) use validation::LocalFileSizeExpectation;
pub(crate) use validation::is_heif_content;
pub(crate) use validation::local_file_size_matches_state;

// These paths remain available even when current sibling callers infer their types.
#[allow(
    unused_imports,
    reason = "Preserve the existing file facade API and visibility"
)]
pub(super) use self::{
    fingerprint::ExistingFileSnapshot, fingerprint::fingerprint_file,
    fingerprint::fingerprint_regular_file_snapshot_blocking, platform::PartName,
    platform::PublishResult, platform::publish_reconciliation_part_blocking,
    publication::FinalPathCollision, publication::publish_part_to_final,
    replacement::ConditionalPublishErrorDisposition, transfer::DownloadResponse,
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

#[cfg(all(test, target_os = "linux", feature = "xmp"))]
pub(super) use platform::ForcedLinkFallback;

/// Retire the temporary name that link-based no-replace publication leaves
/// behind, once both names are verified as one inode with the expected bytes.
/// Rename-based publication leaves nothing to retire. Close every handle on
/// `temp` first: unlinking an open file leaves an NFS `.nfsXXXX` link behind.
/// A substituted name is preserved and reported rather than deleted. Directory
/// durability stays with the caller that owns that contract.
pub(super) fn retire_published_temp_link(
    temp: &crate::fs_util::ConfinedPath,
    destination: &crate::fs_util::ConfinedPath,
    expected: ExistingFileFingerprint,
    identity: crate::fs_util::FileIdentity,
) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Seek;

        // A rename publication leaves no temporary name, so there is no link to
        // retire and nothing to verify.
        let name = temp
            .path()
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("Published temporary name has no file name"))?;
        let mut slot_name = std::ffi::OsString::from(".kei-retire-");
        slot_name.push(name);
        let slot = temp.sibling(&temp.path().with_file_name(slot_name))?;
        if !temp.entry_exists()? {
            // A crash after taking the name into the slot leaves it behind with
            // nothing else to reclaim it.
            return reclaim_published_temp_slot(temp, identity);
        }
        anyhow::ensure!(
            temp.retained_entry_identity()? == identity
                && destination.retained_entry_identity()? == identity,
            "Published temporary name changed; retaining both entries"
        );
        let mut file = temp.open_regular()?;
        anyhow::ensure!(
            crate::fs_util::file_link_count(&file)? == 2,
            "Published temporary entry has an unexpected link count; retaining both entries"
        );
        file.rewind()?;
        anyhow::ensure!(
            fingerprint_open_file_snapshot_blocking(&mut file, temp.path())?.fingerprint
                == expected,
            "Published temporary bytes changed; retaining both entries"
        );
        // Release the handle before unlinking: NFS keeps an open-unlinked file
        // as `.nfsXXXX`, which would leave a stray second link on the published
        // file.
        drop(file);
        // Take the temporary name into a slot this function alone names, then
        // prove the slot holds the published entry. A writer that replaced the
        // temporary name is moved into the slot and restored, never deleted.
        #[cfg(all(test, target_os = "linux"))]
        platform::retirement_point("temp-checked", temp.path());
        anyhow::ensure!(
            !slot.entry_exists()?,
            "Retirement slot already holds an entry; retaining both entries"
        );
        platform::move_to_owned_slot_confined(temp, &slot)?;
        if slot.retained_entry_identity()? != identity {
            // Restore only where that cannot replace a newer entry.
            #[cfg(all(test, target_os = "linux"))]
            platform::retirement_point("temp-quarantined", temp.path());
            let restored = platform::move_layout_confined(&slot, temp);
            anyhow::bail!(
                "Published temporary name changed while retiring; {}",
                match restored {
                    Ok(()) => "substituted entry restored".to_owned(),
                    Err(_) => format!("substituted entry retained at {}", slot.path().display()),
                }
            );
        }
        // Re-prove the destination before the slot goes: a writer that replaced
        // it must leave the bytes reachable here.
        anyhow::ensure!(
            destination.retained_entry_identity()? == identity,
            "Published destination changed while retiring the temporary name; slot retained at {}",
            slot.path().display()
        );
        slot.remove_verified(identity, crate::fs_util::RemovalKind::File)?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (temp, destination, expected, identity);
        Ok(())
    }
}

/// Release the `.kei-retire-` sibling of `temp` when it still holds `identity`.
/// A publication that dies after taking the name into that slot leaves it behind
/// with no later retirement pass to reclaim it.
#[cfg(unix)]
pub(super) fn reclaim_published_temp_slot(
    temp: &crate::fs_util::ConfinedPath,
    identity: crate::fs_util::FileIdentity,
) -> anyhow::Result<()> {
    let name = temp
        .path()
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("Published temporary name has no file name"))?;
    let mut slot_name = std::ffi::OsString::from(".kei-retire-");
    slot_name.push(name);
    let slot = temp.sibling(&temp.path().with_file_name(slot_name))?;
    if slot.retained_entry_identity().ok() == Some(identity) {
        slot.remove_verified(identity, crate::fs_util::RemovalKind::File)?;
    }
    Ok(())
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
    let root = root.to_path_buf();
    let target = target.to_path_buf();
    tokio::task::spawn_blocking(move || has_replacement_journal_blocking(&root, &target)).await?
}

/// Same confined read for commit callbacks already running on a blocking worker.
pub(super) fn has_replacement_journal_blocking(
    root: &std::path::Path,
    target: &std::path::Path,
) -> anyhow::Result<bool> {
    #[cfg(target_os = "linux")]
    {
        let path = replacement_recovery::directory_path(target)?;
        Ok(crate::fs_util::ConfinedPath::open(
            root,
            &path,
            crate::fs_util::ConfinedParents::Existing,
        )?
        .entry_exists()?)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (root, target);
        Ok(false)
    }
}
