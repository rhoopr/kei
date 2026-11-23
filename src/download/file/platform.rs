//! Platform rename, exchange, hard-link, and directory durability primitives.

use std::path::Path;

#[cfg(unix)]
use tokio::fs;

use crate::fs_util::ConfinedPath;
#[cfg(target_os = "linux")]
use crate::fs_util::RemovalKind;

#[cfg(all(test, feature = "xmp", target_os = "linux"))]
thread_local! {
    pub(in crate::download) static RECONCILIATION_RENAME_FAULT: std::cell::Cell<Option<fn(&Path)>> =
        const { std::cell::Cell::new(None) };
}

pub(super) fn fsync_parent_dir_best_effort_blocking(path: &Path) {
    if let Err(error) = crate::fs_util::fsync_parent_dir(path) {
        tracing::warn!(target: "kei::download::file",
            path = %path.display(),
            %error,
            "parent-dir fsync failed; durability of rename not guaranteed"
        );
    }
}

#[cfg(all(test, target_os = "linux"))]
static LINK_FALLBACK_ROOTS: std::sync::Mutex<Vec<std::path::PathBuf>> =
    std::sync::Mutex::new(Vec::new());

#[cfg(all(test, target_os = "linux"))]
fn link_fallback_forced(path: &Path) -> bool {
    LINK_FALLBACK_ROOTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .any(|root| path.starts_with(root))
}

/// Makes no-replace publication under `root` behave as on a filesystem that
/// rejects `RENAME_NOREPLACE` (NFS) until dropped. Scoped to a path so parallel
/// tests and blocking-pool threads are unaffected.
#[cfg(all(test, target_os = "linux"))]
pub(in crate::download) struct ForcedLinkFallback(std::path::PathBuf);

#[cfg(all(test, target_os = "linux"))]
impl ForcedLinkFallback {
    pub(in crate::download) fn new(root: &Path) -> Self {
        LINK_FALLBACK_ROOTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(root.to_path_buf());
        Self(root.to_path_buf())
    }
}

#[cfg(all(test, target_os = "linux"))]
impl Drop for ForcedLinkFallback {
    fn drop(&mut self) {
        let mut roots = LINK_FALLBACK_ROOTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(position) = roots.iter().position(|root| *root == self.0) {
            roots.remove(position);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::download) enum PublishResult {
    Published,
    DestinationExists,
}

/// Whether the `.part` name survives a no-replace publication. Link publication
/// leaves it for the caller, which must close its own handle on the file before
/// retiring the name: unlinking an open file makes an NFS client keep it as
/// `.nfsXXXX`, leaving a second link on the published file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use = "a link publication leaves the .part name for the caller to retire"]
pub(in crate::download) enum PartName {
    Gone,
    #[cfg(unix)]
    Linked,
}

/// A retirement move must consume the source atomically. Link publication is
/// deliberately unavailable because later unlink would race a foreign writer.
pub(in crate::download) fn move_layout_confined(
    source: &ConfinedPath,
    destination: &ConfinedPath,
) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        rename_noreplace_confined(source, destination)
    }
    #[cfg(target_os = "macos")]
    {
        rename_confined_macos(source, destination, libc::RENAME_EXCL)
    }
    #[cfg(windows)]
    {
        move_file_no_replace_blocking(source.path(), destination.path())
    }
    #[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
    {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "Conditional alias retirement is unsupported on this platform",
        ))
    }
}

/// Result of a move whose directory claim the caller journals before the
/// plain rename that consumes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::download) enum JournaledMove {
    Moved,
    /// The filesystem rejects `RENAME_NOREPLACE`. An exclusive empty directory
    /// now claims `destination`: journal this identity, then call again with it.
    // Only the Linux claim path constructs this variant.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Claimed(crate::fs_util::FileIdentity),
}

/// Move between public names. A filesystem with `RENAME_NOREPLACE` keeps the
/// atomic no-replace rename, so no foreign entry can be replaced unnoticed.
///
/// NFS rejects that flag, so the fallback publishes the source entry under the
/// destination name and proves that name before removing the source. Removing
/// the source first would destroy the only bytes when a foreign writer replaces
/// the destination in between. Directories need a journaled claim, so they move
/// through [`move_noreplace_journaled_confined`].
pub(in crate::download) fn move_noreplace_confined(
    source: &ConfinedPath,
    destination: &ConfinedPath,
) -> std::io::Result<()> {
    let moved = move_layout_confined(source, destination);
    #[cfg(target_os = "linux")]
    if let Err(error) = &moved
        && is_renameat2_unsupported(error)
    {
        return move_noreplace_without_renameat2(source, destination);
    }
    moved
}

/// Move between public names where a caller journals the destination claim of
/// a directory move before the plain rename consumes it. `claim` is the
/// identity the journal recorded, so replay can recognise its own empty claim
/// and a substituted entry is never replaced.
pub(in crate::download) fn move_noreplace_journaled_confined(
    source: &ConfinedPath,
    destination: &ConfinedPath,
    claim: Option<crate::fs_util::FileIdentity>,
) -> std::io::Result<JournaledMove> {
    #[cfg(target_os = "linux")]
    if claim.is_some() {
        // The recorded claim occupies the destination, so only the plain
        // rename after verifying that claim can proceed.
        return move_noreplace_journaled_without_renameat2(source, destination, claim);
    }
    let moved = move_layout_confined(source, destination);
    #[cfg(target_os = "linux")]
    if let Err(error) = &moved
        && is_renameat2_unsupported(error)
    {
        return move_noreplace_journaled_without_renameat2(source, destination, claim);
    }
    #[cfg(not(target_os = "linux"))]
    let _ = claim;
    moved.map(|()| JournaledMove::Moved)
}

/// A regular file is linked first, and its source name is removed only once the
/// destination is verified to hold that same entry.
#[cfg(target_os = "linux")]
fn move_noreplace_without_renameat2(
    source: &ConfinedPath,
    destination: &ConfinedPath,
) -> std::io::Result<()> {
    match source.open_directory() {
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "Moving a directory without RENAME_NOREPLACE needs a journaled destination claim",
        )),
        Err(error) if matches!(error.raw_os_error(), Some(libc::ENOTDIR | libc::ELOOP)) => {
            move_linked_file_confined(source, destination)
        }
        Err(error) => Err(error),
    }
}

#[cfg(target_os = "linux")]
fn move_noreplace_journaled_without_renameat2(
    source: &ConfinedPath,
    destination: &ConfinedPath,
    claim: Option<crate::fs_util::FileIdentity>,
) -> std::io::Result<JournaledMove> {
    match source.open_directory() {
        Ok(_) => claim_directory_destination(source, destination, claim),
        Err(error) if matches!(error.raw_os_error(), Some(libc::ENOTDIR | libc::ELOOP)) => {
            move_linked_file_confined(source, destination).map(|()| JournaledMove::Moved)
        }
        Err(error) => Err(error),
    }
}

/// Publish a regular file under `destination` and prove both names before the
/// source link is removed. A recovery link keeps the bytes reachable when a
/// foreign writer replaces the destination during that proof.
#[cfg(target_os = "linux")]
fn move_linked_file_confined(
    source: &ConfinedPath,
    destination: &ConfinedPath,
) -> std::io::Result<()> {
    // The retained parents are the authority: an ancestor may have moved since
    // these paths were opened.
    let identity = source.retained_entry_identity()?;
    match hard_link_confined(source, destination) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            // A resumed attempt may already have linked this exact entry.
            if destination.retained_entry_identity()? != identity {
                return Err(error);
            }
        }
        Err(error) => return Err(error),
    }
    #[cfg(all(test, target_os = "linux"))]
    crate::test_helpers::process_death_point("move-file-linked");
    // A name this call alone owns: linkat refuses an existing name, so no
    // foreign entry can be mistaken for the recovery copy.
    let recovery = destination.sibling(
        &destination
            .path()
            .with_file_name(format!(".kei-move-{}", uuid::Uuid::new_v4())),
    )?;
    hard_link_confined(source, &recovery)?;
    if destination.retained_entry_identity()? != identity {
        return Err(std::io::Error::other(format!(
            "Move destination no longer holds the moved entry; recovery copy retained at {}",
            recovery.path().display()
        )));
    }
    source.remove_verified(identity, RemovalKind::File)?;
    if destination.retained_entry_identity()? != identity {
        return Err(std::io::Error::other(format!(
            "Move destination changed after the source name was removed; recovery copy retained at {}",
            recovery.path().display()
        )));
    }
    recovery.remove_verified(identity, RemovalKind::File)
}

/// Claim a directory destination with an exclusive empty directory whose
/// identity the caller journals before the plain rename replaces it. A recorded
/// claim is the only entry this fallback may rename over.
#[cfg(target_os = "linux")]
fn claim_directory_destination(
    source: &ConfinedPath,
    destination: &ConfinedPath,
    claim: Option<crate::fs_util::FileIdentity>,
) -> std::io::Result<JournaledMove> {
    if let Some(claim) = claim {
        let installed = destination.open_directory()?;
        if crate::fs_util::file_identity(&installed)? != claim {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "Album directory destination no longer holds the recorded claim",
            ));
        }
        // A non-empty or substituted entry is refused by the kernel here.
        renameat2_confined_blocking(source, destination, 0)?;
        return Ok(JournaledMove::Moved);
    }
    let directory = match destination.create_new_directory() {
        Ok(directory) => directory,
        Err(error)
            if error.kind() == std::io::ErrorKind::AlreadyExists
                || error.kind() == std::io::ErrorKind::NotADirectory =>
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "Album directory destination already holds a foreign entry",
            ));
        }
        Err(error) => return Err(error),
    };
    let identity = crate::fs_util::file_identity(&directory)?;
    if destination.retained_entry_identity()? != identity {
        return Err(std::io::Error::other("New album directory claim changed"));
    }
    // The identity is journaled next, so the claim must survive a crash first.
    destination.sync_parent()?;
    Ok(JournaledMove::Claimed(identity))
}

/// Move an obsolete alias into a private retirement slot. A filesystem without
/// `RENAME_NOREPLACE` (NFS) claims the slot with an exclusive hard link and
/// removes the source only after verifying that link, so no entry created in
/// between can be overwritten. Never use this to restore a public name; that
/// keeps `move_noreplace_confined`.
pub(in crate::download) fn retire_layout_confined(
    source: &ConfinedPath,
    slot: &ConfinedPath,
) -> std::io::Result<()> {
    let moved = move_layout_confined(source, slot);
    #[cfg(target_os = "linux")]
    if let Err(error) = &moved
        && is_renameat2_unsupported(error)
    {
        let identity = source.retained_entry_identity()?;
        // linkat refuses an existing slot, so a foreign entry is never replaced.
        // An interrupted retirement leaves the slot holding this very inode: the
        // move is then already linked and only the source name remains to retire.
        match hard_link_confined(source, slot) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if slot.retained_entry_identity()? != identity {
                    return Err(error);
                }
            }
            Err(error) => return Err(error),
        }
        if slot.retained_entry_identity()? != identity {
            return Err(std::io::Error::other(
                "Alias retirement slot no longer holds the retired entry",
            ));
        }
        if let Err(error) = source.remove_verified(identity, RemovalKind::File) {
            // The source still owns the bytes, so a failed retirement must not
            // leave the extra slot name behind.
            let _ = slot.remove_verified(identity, RemovalKind::File);
            return Err(error);
        }
        if slot.retained_entry_identity()? != identity {
            return Err(std::io::Error::other(
                "Alias retirement slot changed after the source name was removed",
            ));
        }
        return Ok(());
    }
    moved
}

/// `RENAME_NOREPLACE` rename. Tests can make it report the `EINVAL` an NFS
/// client returns, to exercise the fallbacks without an NFS mount.
#[cfg(target_os = "linux")]
fn rename_noreplace_confined(
    source: &ConfinedPath,
    destination: &ConfinedPath,
) -> std::io::Result<()> {
    #[cfg(test)]
    if link_fallback_forced(source.path()) {
        return Err(std::io::Error::from_raw_os_error(libc::EINVAL));
    }
    renameat2_confined_blocking(source, destination, libc::RENAME_NOREPLACE)
}

/// Exchange only entries in the retained, no-follow parent capabilities.
/// Linux unsupported exchange is reported to the conditional replacement
/// owner, which retains these capabilities through its journal fallback.
#[cfg(unix)]
pub(in crate::download) fn exchange_layout_confined(
    source: &ConfinedPath,
    destination: &ConfinedPath,
) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        renameat2_confined_blocking(source, destination, libc::RENAME_EXCHANGE)
    }
    #[cfg(target_os = "macos")]
    {
        rename_confined_macos(source, destination, libc::RENAME_SWAP)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "Confined primary layout exchange is unsupported on this platform",
        ))
    }
}

pub(in crate::download) fn publish_reconciliation_part_blocking(
    part: &ConfinedPath,
    destination: &ConfinedPath,
) -> std::io::Result<PublishResult> {
    #[cfg(target_os = "linux")]
    let result = rename_noreplace_confined(part, destination).or_else(|error| {
        if is_renameat2_unsupported(&error) {
            hard_link_confined(part, destination)
        } else {
            Err(error)
        }
    });
    #[cfg(target_os = "macos")]
    let result = rename_confined_macos(part, destination, libc::RENAME_EXCL);
    #[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
    let result = hard_link_confined(part, destination);
    #[cfg(windows)]
    let result = move_file_no_replace_blocking(part.path(), destination.path());
    match result {
        Ok(()) => Ok(PublishResult::Published),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            Ok(PublishResult::DestinationExists)
        }
        Err(error) => Err(error),
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
pub(super) fn hard_link_confined(
    part: &ConfinedPath,
    destination: &ConfinedPath,
) -> std::io::Result<()> {
    // SAFETY: both retained directory descriptors and NUL-terminated names
    // remain live. linkat refuses an existing destination; no path cleanup follows.
    if unsafe {
        libc::linkat(
            part.parent_fd(),
            part.name_cstr().as_ptr(),
            destination.parent_fd(),
            destination.name_cstr().as_ptr(),
            0,
        )
    } == 0
    {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "linux")]
fn renameat2_confined_blocking(
    part_path: &ConfinedPath,
    final_path: &ConfinedPath,
    flags: libc::c_uint,
) -> std::io::Result<()> {
    #[cfg(all(test, feature = "xmp"))]
    if flags == libc::RENAME_NOREPLACE
        && let Some(hook) = RECONCILIATION_RENAME_FAULT.take()
    {
        hook(final_path.path());
        return Err(std::io::Error::from_raw_os_error(libc::EOPNOTSUPP));
    }
    // SAFETY: both directory descriptors and NUL-terminated names remain live,
    // and callers pass one documented renameat2 flag.
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            part_path.parent_fd(),
            part_path.name_cstr().as_ptr(),
            final_path.parent_fd(),
            final_path.name_cstr().as_ptr(),
            flags,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "macos")]
fn rename_confined_macos(
    source: &ConfinedPath,
    destination: &ConfinedPath,
    flags: libc::c_uint,
) -> std::io::Result<()> {
    // SAFETY: both retained directory descriptors and NUL-terminated names
    // remain live and callers pass a documented renameatx_np flag.
    if unsafe {
        libc::renameatx_np(
            source.parent_fd(),
            source.name_cstr().as_ptr(),
            destination.parent_fd(),
            destination.name_cstr().as_ptr(),
            flags,
        )
    } == 0
    {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Publish and retire the `.part` name, for callers that hold no handle on it.
#[cfg(unix)]
pub(super) async fn publish_part_no_replace(
    part_path: &Path,
    final_path: &Path,
) -> std::io::Result<PublishResult> {
    let (result, part) = publish_part_no_replace_keeping_link(part_path, final_path).await?;
    if part == PartName::Linked {
        retire_linked_part(part_path).await;
    }
    Ok(result)
}

#[cfg(target_os = "linux")]
pub(super) async fn publish_part_no_replace_keeping_link(
    part_path: &Path,
    final_path: &Path,
) -> std::io::Result<(PublishResult, PartName)> {
    let part = part_path.to_path_buf();
    let final_path_buf = final_path.to_path_buf();
    let rename_result =
        tokio::task::spawn_blocking(move || renameat2_no_replace_blocking(&part, &final_path_buf))
            .await
            .map_err(std::io::Error::other)?;

    match rename_result {
        Ok(()) => Ok((PublishResult::Published, PartName::Gone)),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            Ok((PublishResult::DestinationExists, PartName::Gone))
        }
        Err(e) if is_renameat2_unsupported(&e) => publish_part_by_hard_link(part_path, final_path)
            .await
            .or_else(|link_err| {
                destination_exists_or(link_err, final_path).map(|result| (result, PartName::Gone))
            }),
        Err(e) => destination_exists_or(e, final_path).map(|result| (result, PartName::Gone)),
    }
}

#[cfg(target_os = "linux")]
fn renameat2_no_replace_blocking(part_path: &Path, final_path: &Path) -> std::io::Result<()> {
    #[cfg(test)]
    if link_fallback_forced(part_path) {
        return Err(std::io::Error::from_raw_os_error(libc::EINVAL));
    }
    renameat2_blocking(part_path, final_path, libc::RENAME_NOREPLACE)
}

#[cfg(target_os = "linux")]
pub(super) fn renameat2_exchange_blocking(
    part_path: &Path,
    final_path: &Path,
) -> std::io::Result<()> {
    renameat2_blocking(part_path, final_path, libc::RENAME_EXCHANGE)
}

#[cfg(target_os = "linux")]
fn renameat2_blocking(
    part_path: &Path,
    final_path: &Path,
    flags: libc::c_uint,
) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let part_c = CString::new(part_path.as_os_str().as_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let final_c = CString::new(final_path.as_os_str().as_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    // SAFETY: both path arguments are valid NUL-terminated C strings, AT_FDCWD
    // asks the kernel to resolve them from the current working directory when
    // they are relative, and callers pass one documented renameat2 flag. No
    // Rust references are shared with the kernel after the call.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            part_c.as_ptr(),
            libc::AT_FDCWD,
            final_c.as_ptr(),
            flags,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "linux")]
pub(super) fn is_renameat2_unsupported(e: &std::io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(libc::ENOSYS | libc::EINVAL | libc::EOPNOTSUPP)
    )
}

#[cfg(all(unix, not(target_os = "linux")))]
pub(super) async fn publish_part_no_replace_keeping_link(
    part_path: &Path,
    final_path: &Path,
) -> std::io::Result<(PublishResult, PartName)> {
    publish_part_by_hard_link(part_path, final_path)
        .await
        .or_else(|link_err| {
            destination_exists_or(link_err, final_path).map(|result| (result, PartName::Gone))
        })
}

#[cfg(target_os = "macos")]
pub(super) fn rename_exchange_blocking(part_path: &Path, final_path: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let part = CString::new(part_path.as_os_str().as_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let final_path = CString::new(final_path.as_os_str().as_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    // SAFETY: both paths are valid NUL-terminated C strings and RENAME_SWAP
    // atomically exchanges two existing directory entries on macOS.
    let rc = unsafe { libc::renamex_np(part.as_ptr(), final_path.as_ptr(), libc::RENAME_SWAP) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(unix)]
async fn publish_part_by_hard_link(
    part_path: &Path,
    final_path: &Path,
) -> std::io::Result<(PublishResult, PartName)> {
    match fs::hard_link(part_path, final_path).await {
        Ok(()) => Ok((PublishResult::Published, PartName::Linked)),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            Ok((PublishResult::DestinationExists, PartName::Gone))
        }
        Err(e) => Err(e),
    }
}

/// Remove the `.part` name that link publication left behind. The caller must
/// have closed its own handles on the file first.
#[cfg(unix)]
pub(in crate::download) async fn retire_linked_part(part_path: &Path) {
    if let Err(rm_err) = fs::remove_file(part_path).await {
        tracing::warn!(target: "kei::download::file",
            path = %part_path.display(),
            error = %rm_err,
            "Failed to remove published .part file after no-overwrite hard link"
        );
    }
}

#[cfg(windows)]
pub(super) async fn publish_part_no_replace_keeping_link(
    part_path: &Path,
    final_path: &Path,
) -> std::io::Result<(PublishResult, PartName)> {
    publish_part_no_replace(part_path, final_path)
        .await
        .map(|result| (result, PartName::Gone))
}

#[cfg(windows)]
pub(super) async fn publish_part_no_replace(
    part_path: &Path,
    final_path: &Path,
) -> std::io::Result<PublishResult> {
    let part = part_path.to_path_buf();
    let final_path_buf = final_path.to_path_buf();
    let rename_result =
        tokio::task::spawn_blocking(move || move_file_no_replace_blocking(&part, &final_path_buf))
            .await
            .map_err(std::io::Error::other)?;

    match rename_result {
        Ok(()) => Ok(PublishResult::Published),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            Ok(PublishResult::DestinationExists)
        }
        Err(e) => destination_exists_or(e, final_path),
    }
}

#[cfg(windows)]
pub(super) use crate::fs_util::move_file_no_replace_blocking;
#[cfg(windows)]
use crate::fs_util::windows_api_path;

#[cfg(windows)]
pub(super) fn replace_file_with_backup_blocking(
    final_path: &Path,
    replacement_path: &Path,
    backup_path: &Path,
) -> std::io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::{REPLACEFILE_WRITE_THROUGH, ReplaceFileW};

    let final_path = windows_api_path(final_path)?;
    let replacement = windows_api_path(replacement_path)?;
    let backup = windows_api_path(backup_path)?;
    // SAFETY: every path is a live NUL-terminated absolute Windows string
    // with extended-length support. The final
    // path exists, the replacement is the verified `.part` file, and Windows
    // atomically moves the displaced bytes to the distinct backup path.
    let rc = unsafe {
        ReplaceFileW(
            final_path.as_ptr(),
            replacement.as_ptr(),
            backup.as_ptr(),
            REPLACEFILE_WRITE_THROUGH,
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if rc != 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn destination_exists_or(err: std::io::Error, final_path: &Path) -> std::io::Result<PublishResult> {
    if final_path.try_exists().unwrap_or(false) {
        Ok(PublishResult::DestinationExists)
    } else {
        Err(err)
    }
}

#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
    use super::{ForcedLinkFallback, move_noreplace_confined};
    use crate::fs_util::{ConfinedParents, ConfinedPath};
    use std::path::{Path, PathBuf};

    fn open(root: &Path, path: &Path) -> ConfinedPath {
        ConfinedPath::open(root, path, ConfinedParents::Existing).unwrap()
    }

    #[test]
    #[ignore = "parent-owned SIGKILL fixture"]
    fn move_link_process_death_child() {
        let root = PathBuf::from(std::env::var_os("KEI_TEST_PROCESS_DEATH_ROOT").unwrap());
        assert_eq!(
            std::fs::read(root.join("fixture-owner")).unwrap(),
            b"kei-synthetic-process-death"
        );
        let _fallback = ForcedLinkFallback::new(&root);
        let source = open(&root, &root.join("album/note.txt"));
        let target = open(&root, &root.join("moved/note.txt"));
        move_noreplace_confined(&source, &target).unwrap();
        panic!("move child did not reach its process-death point");
    }

    /// The death happens after the destination link and before the source name
    /// is removed, so the resumed move must treat the existing link as its own.
    #[test]
    fn link_publication_process_death_resumes_the_interrupted_move() {
        use std::os::unix::fs::MetadataExt;
        use std::os::unix::process::ExitStatusExt;

        let root = tempfile::tempdir().unwrap();
        let album = root.path().join("album");
        let moved = root.path().join("moved");
        std::fs::create_dir(&album).unwrap();
        std::fs::create_dir(&moved).unwrap();
        std::fs::write(album.join("note.txt"), b"user bytes").unwrap();
        std::fs::write(
            root.path().join("fixture-owner"),
            b"kei-synthetic-process-death",
        )
        .unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "download::file::platform::linux_tests::move_link_process_death_child",
                "--ignored",
                "--test-threads=1",
            ])
            .env("KEI_TEST_PROCESS_DEATH_ROOT", root.path())
            .env("KEI_TEST_PROCESS_DEATH_POINT", "move-file-linked")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let ready = root.path().join("ready");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !ready.exists() {
            if let Some(status) = child.try_wait().unwrap() {
                panic!("move child exited before its process-death point: {status}");
            }
            if std::time::Instant::now() >= deadline {
                child.kill().unwrap();
                let _status = child.wait().unwrap();
                panic!("move child did not reach its process-death point");
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(std::fs::read_to_string(&ready).unwrap(), "move-file-linked");
        child.kill().unwrap();
        assert_eq!(child.wait().unwrap().signal(), Some(9));

        let source_name = album.join("note.txt");
        let moved_name = moved.join("note.txt");
        let interrupted = std::fs::metadata(&moved_name).unwrap();
        assert_eq!(
            std::fs::metadata(&source_name).unwrap().ino(),
            interrupted.ino()
        );
        assert_eq!(interrupted.nlink(), 2, "both names survived the death");
        assert_eq!(std::fs::read(&moved_name).unwrap(), b"user bytes");

        let _fallback = ForcedLinkFallback::new(root.path());
        move_noreplace_confined(
            &open(root.path(), &source_name),
            &open(root.path(), &moved_name),
        )
        .unwrap();
        assert!(!source_name.exists());
        assert_eq!(std::fs::metadata(&moved_name).unwrap().nlink(), 1);
        assert_eq!(std::fs::read(&moved_name).unwrap(), b"user bytes");
        assert_eq!(
            std::fs::read_dir(&moved).unwrap().count(),
            1,
            "the resumed move leaves exactly one name"
        );
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::{
        PublishResult, move_layout_confined, publish_part_no_replace,
        publish_reconciliation_part_blocking, windows_api_path,
    };
    use crate::fs_util::{ConfinedParents, ConfinedPath};
    use std::ffi::OsString;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::path::Path;

    #[test]
    fn windows_api_paths_preserve_native_units_and_resolve_win32_syntax() {
        for (input, expected) in [
            (
                r"C:/photos/./nested/../media.jpg",
                r"\\?\C:\photos\media.jpg",
            ),
            (
                r"\\server\share\photos\media.jpg",
                r"\\?\UNC\server\share\photos\media.jpg",
            ),
            (r"\\?\C:\photos\media.jpg", r"\\?\C:\photos\media.jpg"),
            (
                r"\\?\UNC\server\share\media.jpg",
                r"\\?\UNC\server\share\media.jpg",
            ),
            (r"\\.\C:\photos\media.jpg", r"\\.\C:\photos\media.jpg"),
        ] {
            let mut units: Vec<u16> = expected.encode_utf16().collect();
            units.push(0);
            assert_eq!(
                windows_api_path(Path::new(input)).unwrap(),
                units,
                "{input}"
            );
        }
        let relative = Path::new("native-relative.jpg");
        assert_eq!(
            windows_api_path(relative).unwrap(),
            windows_api_path(&std::path::absolute(relative).unwrap()).unwrap()
        );
        let mut native: Vec<u16> = r"C:\photos\".encode_utf16().collect();
        native.push(0xd800);
        native.extend(".jpg".encode_utf16());
        let mut expected: Vec<u16> = r"\\?\".encode_utf16().collect();
        expected.extend(&native);
        expected.push(0);
        assert_eq!(
            windows_api_path(Path::new(&OsString::from_wide(&native))).unwrap(),
            expected
        );
        native.push(0);
        assert_eq!(
            windows_api_path(Path::new(&OsString::from_wide(&native)))
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::InvalidInput
        );
    }

    #[tokio::test]
    async fn windows_long_paths_publish_without_overwrite_and_retire_confined_aliases() {
        let root = tempfile::tempdir().unwrap();
        let parent = root
            .path()
            .join("nested".repeat(20))
            .join("nested".repeat(20));
        tokio::fs::create_dir_all(&parent).await.unwrap();
        let stage = parent.join("source.part");
        let target = parent.join("primary.jpg");
        let retired = parent.join("retired.jpg");
        assert!(stage.as_os_str().encode_wide().count() > 260);
        assert!(target.as_os_str().encode_wide().count() > 260);
        tokio::fs::write(&stage, b"verified source").await.unwrap();
        assert_eq!(
            publish_part_no_replace(&stage, &target).await.unwrap(),
            PublishResult::Published
        );
        assert!(!stage.exists());
        assert_eq!(tokio::fs::read(&target).await.unwrap(), b"verified source");
        tokio::fs::write(&stage, b"second source").await.unwrap();
        assert_eq!(
            publish_part_no_replace(&stage, &target).await.unwrap(),
            PublishResult::DestinationExists
        );
        assert_eq!(tokio::fs::read(&stage).await.unwrap(), b"second source");
        assert_eq!(tokio::fs::read(&target).await.unwrap(), b"verified source");
        let root = root.path().to_owned();
        tokio::task::spawn_blocking(move || {
            let source = ConfinedPath::open(&root, &stage, ConfinedParents::Existing).unwrap();
            let destination =
                ConfinedPath::open(&root, &target, ConfinedParents::Existing).unwrap();
            let retired = ConfinedPath::open(&root, &retired, ConfinedParents::Existing).unwrap();
            assert_eq!(
                publish_reconciliation_part_blocking(&source, &destination).unwrap(),
                PublishResult::DestinationExists
            );
            assert_eq!(std::fs::read(source.path()).unwrap(), b"second source");
            assert_eq!(
                std::fs::read(destination.path()).unwrap(),
                b"verified source"
            );
            move_layout_confined(&destination, &retired).unwrap();
            assert_eq!(std::fs::read(retired.path()).unwrap(), b"verified source");
            assert!(!destination.path().exists());
            assert_eq!(
                publish_reconciliation_part_blocking(&source, &destination).unwrap(),
                PublishResult::Published
            );
            assert_eq!(std::fs::read(destination.path()).unwrap(), b"second source");
            assert!(!source.path().exists());
        })
        .await
        .unwrap();
    }
}
