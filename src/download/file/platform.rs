//! Platform rename, exchange, hard-link, and directory durability primitives.

use std::path::Path;

#[cfg(unix)]
use tokio::fs;

use crate::fs_util::ConfinedPath;

pub(super) fn fsync_parent_dir_best_effort_blocking(path: &Path) {
    if let Err(error) = crate::fs_util::fsync_parent_dir(path) {
        tracing::warn!(target: "kei::download::file",
            path = %path.display(),
            %error,
            "parent-dir fsync failed; durability of rename not guaranteed"
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::download) enum PublishResult {
    Published,
    /// Published, but the no-overwrite hard-link fallback's temporary entry
    /// could not be safely removed (a foreign writer may have replaced it, or
    /// the unlink itself failed and a retry did not clear it). The caller's
    /// bytes and ownership evidence are intact; only cleanup of the extra
    /// link is outstanding.
    PublishedWithRetainedTemp,
    DestinationExists,
}

/// Outcome of a no-overwrite publication's own temporary-entry cleanup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinkCleanupOutcome {
    /// The temporary entry was removed; the destination keeps exactly one link.
    Cleaned,
    /// The destination's bytes are correct, but the temporary entry's own
    /// link could not be safely or successfully removed.
    Retained,
}

/// A retirement move must consume the source atomically. Link publication is
/// deliberately unavailable because later unlink would race a foreign writer.
pub(in crate::download) fn move_layout_confined(
    source: &ConfinedPath,
    destination: &ConfinedPath,
) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        renameat2_confined_blocking(source, destination, libc::RENAME_NOREPLACE)
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
    #[allow(unused_variables)] part_identity: crate::fs_util::FileIdentity,
) -> std::io::Result<PublishResult> {
    #[cfg(target_os = "linux")]
    let result = match renameat2_confined_blocking(part, destination, libc::RENAME_NOREPLACE) {
        Ok(()) => Ok(LinkCleanupOutcome::Cleaned),
        Err(error) if is_renameat2_unsupported(&error) => {
            hard_link_then_unlink_confined(part, destination, part_identity)
        }
        Err(error) => Err(error),
    };
    #[cfg(target_os = "macos")]
    let result = rename_confined_macos(part, destination, libc::RENAME_EXCL)
        .map(|()| LinkCleanupOutcome::Cleaned);
    #[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
    let result = hard_link_then_unlink_confined(part, destination, part_identity);
    #[cfg(windows)]
    let result = move_file_no_replace_blocking(part.path(), destination.path())
        .map(|()| LinkCleanupOutcome::Cleaned);
    match result {
        Ok(LinkCleanupOutcome::Cleaned) => Ok(PublishResult::Published),
        Ok(LinkCleanupOutcome::Retained) => Ok(PublishResult::PublishedWithRetainedTemp),
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

/// Publish `part` to `destination` by hard link, then unlink `part` so the
/// destination keeps exactly one link. Mirrors `publish_part_by_hard_link`.
///
/// Without the unlink the published entry keeps `nlink == 2` (shared with the
/// retained reconciliation part). The primary layout hardlink guard
/// (`file::primary_layout::snapshot`) refuses any entry with `nlink > 1`, so a
/// filesystem without `renameat2(RENAME_NOREPLACE)` support (e.g. CephFS, NFS)
/// would otherwise deadlock.
///
/// `expected_identity` is `part`'s identity as observed by the caller before
/// this call, mirroring `replacement_recovery::finish_owned_prepared_link`'s
/// own identity contract. The link itself is created from an already-open,
/// already-verified file descriptor (see [`hard_link_confined_by_fd`]), so a
/// foreign writer replacing `part`'s name before linking cannot cause this
/// function to publish bytes it never verified. A foreign writer can still
/// replace `part`'s name in the much smaller window between that link
/// succeeding and this function's unlink; removing that entry on the
/// caller's behalf would delete bytes this function never published, so the
/// identity is re-checked immediately before unlinking, and the entry is
/// left untouched (not unlinked) on any mismatch.
///
/// A transient unlink failure (e.g. `EBUSY`/`ESTALE`, observed on NFS/CephFS)
/// is retried once, as is each parent-directory fsync. If the identity check
/// fails or the unlink retry also fails, the destination's bytes are already
/// correct and durable; only removal of the now-redundant temporary link is
/// outstanding, reported via [`LinkCleanupOutcome::Retained`] rather than
/// failing the publication. A durability (fsync) failure is not downgraded
/// the same way: the caller (and journaled recovery) depend on durability
/// having actually happened before an operation is treated as complete
/// (CephFS/NFS can report transient fsync errors, hence the one retry, but a
/// persistent failure still fails the publication rather than being hidden).
#[cfg(all(unix, not(target_os = "macos")))]
fn hard_link_then_unlink_confined(
    part: &ConfinedPath,
    destination: &ConfinedPath,
    expected_identity: crate::fs_util::FileIdentity,
) -> std::io::Result<LinkCleanupOutcome> {
    hard_link_confined_by_fd(part, destination, expected_identity)?;
    if let Err(error) = part.validate_identity(expected_identity) {
        tracing::warn!(target: "kei::download::file",
            path = %part.path().display(),
            %error,
            "Temporary entry changed after no-overwrite hard link; retaining it rather than risking a foreign file"
        );
        return Ok(LinkCleanupOutcome::Retained);
    }
    // One immediate retry absorbs the common transient case without masking
    // a persistent failure (e.g. a read-only remount, a permissions change).
    let unlink_result = unlink_confined(part).or_else(|first_error| {
        if part.validate_identity(expected_identity).is_err() {
            return Err(first_error);
        }
        unlink_confined(part).map_err(|_retry_error| first_error)
    });
    if let Err(error) = unlink_result {
        tracing::warn!(target: "kei::download::file",
            path = %part.path().display(),
            %error,
            "Failed to remove published temporary after no-overwrite hard link; entry retains an extra link"
        );
        return Ok(LinkCleanupOutcome::Retained);
    }
    sync_parent_with_retry(part)?;
    sync_parent_with_retry(destination)?;
    Ok(LinkCleanupOutcome::Cleaned)
}

/// Hard-link the file identified by `expected_identity` to `destination`,
/// verifying identity and performing the link against the same open file
/// descriptor so the two can never diverge (unlike verifying `part`'s
/// identity by name and then linking by name again, which leaves a window
/// for a foreign writer to replace the name in between).
#[cfg(all(unix, not(target_os = "macos")))]
fn hard_link_confined_by_fd(
    part: &ConfinedPath,
    destination: &ConfinedPath,
    expected_identity: crate::fs_util::FileIdentity,
) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;

    let file = part.open_regular()?;
    if crate::fs_util::file_identity(&file)? != expected_identity {
        return Err(std::io::Error::other(
            "Temporary entry changed before it could be linked",
        ));
    }
    // /proc/self/fd/<fd> is a magic symlink to this exact open file, not to
    // whatever name `part` currently resolves to, so this links the inode
    // this function just verified, however `part`'s name is used afterward.
    // A formatted fd number never contains a NUL byte, so this never fails
    // in practice; mapped rather than `expect`ed since this is itself
    // inside a function that returns `Result`.
    let fd_path = std::ffi::CString::new(format!("/proc/self/fd/{}", file.as_raw_fd()))
        .map_err(std::io::Error::other)?;
    // SAFETY: `file` is kept alive until after the call, keeping the fd (and
    // thus the magic symlink) valid; the destination's retained parent
    // directory descriptor and NUL-terminated name remain live.
    // AT_SYMLINK_FOLLOW dereferences the magic symlink to the underlying
    // inode instead of erroring on it.
    let result = unsafe {
        libc::linkat(
            libc::AT_FDCWD,
            fd_path.as_ptr(),
            destination.parent_fd(),
            destination.name_cstr().as_ptr(),
            libc::AT_SYMLINK_FOLLOW,
        )
    };
    drop(file);
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Fsync `path`'s parent directory, retrying once on a transient failure.
/// Unlike [`fsync_parent_dir_best_effort_blocking`], a persistent failure is
/// still returned: callers here depend on durability having happened before
/// an operation is treated as complete, not merely attempted.
#[cfg(all(unix, not(target_os = "macos")))]
fn sync_parent_with_retry(path: &ConfinedPath) -> std::io::Result<()> {
    path.sync_parent().or_else(|_| path.sync_parent())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn unlink_confined(part: &ConfinedPath) -> std::io::Result<()> {
    #[cfg(test)]
    if let Some(result) = tests::unlink_fault_injection(part.path()) {
        return result;
    }
    // SAFETY: the retained parent directory descriptor and the NUL-terminated
    // name remain live for the syscall, and unlinkat does not follow the leaf.
    if unsafe { libc::unlinkat(part.parent_fd(), part.name_cstr().as_ptr(), 0) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::{LinkCleanupOutcome, hard_link_confined_by_fd, hard_link_then_unlink_confined};
    use crate::fs_util::{ConfinedParents, ConfinedPath, file_identity};
    use std::collections::HashMap;
    use std::os::unix::fs::MetadataExt;
    use std::sync::Mutex;

    // Scopes a fixed number of injected unlinkat failures to a single path,
    // so tests can model a transient fault (one failure, then success) or a
    // persistent one (every attempt fails) without touching real I/O.
    static UNLINK_FAULTS: Mutex<Option<HashMap<std::path::PathBuf, u32>>> = Mutex::new(None);
    // Content to swap a path's entry to, as a side effect of consuming one
    // injected fault, modeling a foreign writer racing the retry itself.
    static SWAP_ON_FAULT: Mutex<Option<HashMap<std::path::PathBuf, Vec<u8>>>> = Mutex::new(None);

    fn inject_unlink_failures(path: &std::path::Path, count: u32) {
        UNLINK_FAULTS
            .lock()
            .unwrap()
            .get_or_insert_with(HashMap::new)
            .insert(path.to_path_buf(), count);
    }

    fn inject_unlink_failure_with_foreign_swap(path: &std::path::Path, foreign_bytes: &[u8]) {
        inject_unlink_failures(path, 1);
        SWAP_ON_FAULT
            .lock()
            .unwrap()
            .get_or_insert_with(HashMap::new)
            .insert(path.to_path_buf(), foreign_bytes.to_vec());
    }

    pub(super) fn unlink_fault_injection(path: &std::path::Path) -> Option<std::io::Result<()>> {
        let mut guard = UNLINK_FAULTS.lock().unwrap();
        let remaining = guard.as_mut()?.get_mut(path)?;
        if *remaining == 0 {
            return None;
        }
        *remaining -= 1;
        // A foreign writer can replace this exact name at any point before
        // its removal; model it landing right as this attempt fails, so the
        // retry's own identity re-check is what has to catch it.
        if let Some(bytes) = SWAP_ON_FAULT
            .lock()
            .unwrap()
            .as_mut()
            .and_then(|map| map.remove(path))
        {
            std::fs::remove_file(path).unwrap();
            std::fs::write(path, bytes).unwrap();
        }
        Some(Err(std::io::Error::from_raw_os_error(libc::EBUSY)))
    }

    fn setup(root: &std::path::Path) -> (ConfinedPath, ConfinedPath, crate::fs_util::FileIdentity) {
        let source = root.join("source.jpg");
        let destination = root.join("destination.jpg");
        std::fs::write(&source, b"reconcile me").unwrap();
        let part = ConfinedPath::open(root, &source, ConfinedParents::Existing).unwrap();
        let identity = file_identity(&part.open_regular().unwrap()).unwrap();
        let destination = ConfinedPath::open(root, &destination, ConfinedParents::Create).unwrap();
        (part, destination, identity)
    }

    #[test]
    fn retries_a_transient_unlink_failure_and_reaches_steady_state() {
        let root = tempfile::tempdir().unwrap();
        let (part, destination, identity) = setup(root.path());
        inject_unlink_failures(part.path(), 1);

        let outcome = hard_link_then_unlink_confined(&part, &destination, identity).unwrap();

        assert_eq!(outcome, LinkCleanupOutcome::Cleaned);
        assert!(
            !part.path().exists(),
            "temporary must be removed once the retry succeeds"
        );
        assert_eq!(std::fs::metadata(destination.path()).unwrap().nlink(), 1);
    }

    #[test]
    fn retains_entry_on_persistent_unlink_failure() {
        let root = tempfile::tempdir().unwrap();
        let (part, destination, identity) = setup(root.path());
        inject_unlink_failures(part.path(), 2);

        let outcome = hard_link_then_unlink_confined(&part, &destination, identity).unwrap();

        assert_eq!(outcome, LinkCleanupOutcome::Retained);
        assert!(
            part.path().exists(),
            "bytes must be retained, not lost, when cleanup cannot complete"
        );
        assert_eq!(
            std::fs::metadata(destination.path()).unwrap().nlink(),
            2,
            "destination still shares its link with the retained temporary"
        );
        assert_eq!(
            std::fs::read(destination.path()).unwrap(),
            b"reconcile me",
            "destination bytes are correct regardless of cleanup outcome"
        );
    }

    #[test]
    fn hard_link_confined_by_fd_refuses_to_link_an_already_foreign_entry() {
        let root = tempfile::tempdir().unwrap();
        let (part, destination, _identity) = setup(root.path());
        // A foreign identity stands in for a writer having already replaced
        // `part`'s name before this call even starts: nothing may be
        // published from an entry that was never verified.
        let foreign = root.path().join("unrelated.jpg");
        std::fs::write(&foreign, b"not part of this publication").unwrap();
        let foreign_identity = file_identity(
            &ConfinedPath::open(root.path(), &foreign, ConfinedParents::Existing)
                .unwrap()
                .open_regular()
                .unwrap(),
        )
        .unwrap();

        let error = hard_link_confined_by_fd(&part, &destination, foreign_identity).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::Other);
        assert!(
            !destination.path().exists(),
            "an identity mismatch must publish nothing, not an unverified entry"
        );
        assert_eq!(std::fs::read(part.path()).unwrap(), b"reconcile me");
    }

    #[test]
    fn retains_entry_when_a_foreign_writer_replaces_it_before_the_retry() {
        let root = tempfile::tempdir().unwrap();
        let (part, destination, identity) = setup(root.path());
        // The link itself is fd-pinned (hard_link_confined_by_fd) and so
        // cannot be fooled by a rename of `part`'s name; this models the
        // narrower, remaining window between that link succeeding and the
        // unlink of the now-redundant name, where a foreign writer's rename
        // can still land.
        inject_unlink_failure_with_foreign_swap(part.path(), b"foreign bytes, not ours");

        let outcome = hard_link_then_unlink_confined(&part, &destination, identity).unwrap();

        assert_eq!(outcome, LinkCleanupOutcome::Retained);
        assert_eq!(
            std::fs::read(part.path()).unwrap(),
            b"foreign bytes, not ours",
            "the foreign entry must survive untouched, not be deleted on our behalf"
        );
        assert_eq!(
            std::fs::read(destination.path()).unwrap(),
            b"reconcile me",
            "destination keeps the bytes verified at fd-link time, unaffected by the later swap"
        );
    }
}

#[cfg(target_os = "linux")]
fn renameat2_confined_blocking(
    part_path: &ConfinedPath,
    final_path: &ConfinedPath,
    flags: libc::c_uint,
) -> std::io::Result<()> {
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

#[cfg(target_os = "linux")]
pub(super) async fn publish_part_no_replace(
    part_path: &Path,
    final_path: &Path,
) -> std::io::Result<PublishResult> {
    let part = part_path.to_path_buf();
    let final_path_buf = final_path.to_path_buf();
    let rename_result =
        tokio::task::spawn_blocking(move || renameat2_no_replace_blocking(&part, &final_path_buf))
            .await
            .map_err(std::io::Error::other)?;

    match rename_result {
        Ok(()) => Ok(PublishResult::Published),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            Ok(PublishResult::DestinationExists)
        }
        Err(e) if is_renameat2_unsupported(&e) => publish_part_by_hard_link(part_path, final_path)
            .await
            .or_else(|link_err| destination_exists_or(link_err, final_path)),
        Err(e) => destination_exists_or(e, final_path),
    }
}

#[cfg(target_os = "linux")]
fn renameat2_no_replace_blocking(part_path: &Path, final_path: &Path) -> std::io::Result<()> {
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
pub(super) async fn publish_part_no_replace(
    part_path: &Path,
    final_path: &Path,
) -> std::io::Result<PublishResult> {
    publish_part_by_hard_link(part_path, final_path)
        .await
        .or_else(|link_err| destination_exists_or(link_err, final_path))
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
) -> std::io::Result<PublishResult> {
    match fs::hard_link(part_path, final_path).await {
        Ok(()) => {
            if let Err(rm_err) = fs::remove_file(part_path).await {
                tracing::warn!(target: "kei::download::file",
                    path = %part_path.display(),
                    error = %rm_err,
                    "Failed to remove published .part file after no-overwrite hard link"
                );
            }
            Ok(PublishResult::Published)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            Ok(PublishResult::DestinationExists)
        }
        Err(e) => Err(e),
    }
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

/// Match the standard library's absolute-path handling before passing paths to
/// Win32 APIs directly. Extended paths avoid MAX_PATH without registry or
/// manifest requirements; native UTF-16 units and existing namespaces survive.
#[cfg(windows)]
fn windows_api_path(path: &Path) -> std::io::Result<Vec<u16>> {
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Component, Prefix};

    if path.as_os_str().encode_wide().any(|unit| unit == 0) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "path contains NUL",
        ));
    }
    // GetFullPathNameW resolves relative paths and Win32 separators/components
    // without filesystem traversal. Already verbatim paths stay unchanged.
    let absolute = std::path::absolute(path)?;
    let wide: Vec<u16> = absolute.as_os_str().encode_wide().collect();
    let prefix = match absolute.components().next() {
        Some(Component::Prefix(prefix)) => prefix.kind(),
        _ => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Windows publication requires an absolute path",
            ));
        }
    };
    let mut result: Vec<u16> = match prefix {
        Prefix::Disk(_) => r"\\?\".encode_utf16().chain(wide).collect(),
        Prefix::UNC(_, _) => r"\\?\UNC\"
            .encode_utf16()
            .chain(wide.into_iter().skip(2))
            .collect(),
        Prefix::Verbatim(_)
        | Prefix::VerbatimUNC(_, _)
        | Prefix::VerbatimDisk(_)
        | Prefix::DeviceNS(_) => wide,
    };
    result.push(0);
    Ok(result)
}

#[cfg(windows)]
pub(super) fn move_file_no_replace_blocking(
    part_path: &Path,
    final_path: &Path,
) -> std::io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};

    let part = windows_api_path(part_path)?;
    let final_path = windows_api_path(final_path)?;
    // SAFETY: both path arguments are live NUL-terminated absolute Windows
    // strings, with extended namespaces for ordinary drive and UNC paths.
    // MOVEFILE_WRITE_THROUGH keeps the existing durable-publish intent, and
    // omitting MOVEFILE_REPLACE_EXISTING gives this promotion no-overwrite
    // semantics.
    let rc = unsafe { MoveFileExW(part.as_ptr(), final_path.as_ptr(), MOVEFILE_WRITE_THROUGH) };
    if rc != 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

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
            let source_identity =
                crate::fs_util::file_identity(&source.open_regular().unwrap()).unwrap();
            assert_eq!(
                publish_reconciliation_part_blocking(&source, &destination, source_identity)
                    .unwrap(),
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
            let source_identity =
                crate::fs_util::file_identity(&source.open_regular().unwrap()).unwrap();
            assert_eq!(
                publish_reconciliation_part_blocking(&source, &destination, source_identity)
                    .unwrap(),
                PublishResult::Published
            );
            assert_eq!(std::fs::read(destination.path()).unwrap(), b"second source");
            assert!(!source.path().exists());
        })
        .await
        .unwrap();
    }
}
