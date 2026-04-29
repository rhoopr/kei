//! Shared filesystem primitives.

use std::path::Path;

/// Remove `path`, treating `NotFound` as success and logging any other
/// error at `warn!`. Use this in cleanup paths (`.part` cleanup, corrupt
/// session-file deletion, EXDEV unwind) where a previous `let _ =` was
/// silently dropping errors that violated the "no silent failures"
/// invariant.
///
/// Gated on the `xmp` feature because the only sync caller is
/// `download/metadata.rs::TmpGuard::drop`, which is itself xmp-gated.
/// The async sibling `log_remove_async` is unconditionally compiled.
#[cfg(feature = "xmp")]
pub(crate) fn log_remove(path: &Path) {
    if let Err(e) = std::fs::remove_file(path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "Failed to remove file during cleanup"
            );
        }
    }
}

/// Async sibling of [`log_remove`] for callers already on a tokio task;
/// uses `tokio::fs::remove_file` so it doesn't block a runtime worker.
pub(crate) async fn log_remove_async(path: &Path) {
    if let Err(e) = tokio::fs::remove_file(path).await {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "Failed to remove file during cleanup"
            );
        }
    }
}

/// Open `path`'s parent directory and `fsync` it so a preceding `rename`'s
/// directory entry survives a power loss. Unix-only; on Windows this is a
/// no-op because the std API doesn't expose a directory handle for fsync.
///
/// Errors from the open or sync are returned to the caller. Callers that
/// want best-effort durability without bubbling the error should log and
/// drop it themselves.
pub(crate) fn fsync_parent_dir(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let parent = path.parent().unwrap_or(Path::new("."));
        let dir = std::fs::File::open(parent)?;
        dir.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

/// Install `src` at `dst` atomically.
///
/// Prefers `rename` (atomic on the same device); on EXDEV, copies to a
/// sibling of `dst` on the destination device and renames that sibling
/// into place so a mid-copy crash can't expose a half-written `dst`.
///
/// `src`'s data is fsynced before the rename and `dst`'s parent directory
/// is fsynced after, so a power loss between the rename returning and the
/// kernel committing data + directory blocks can't leave `dst` pointing
/// at an uninitialised file or vanish on the next mount.
pub(crate) fn atomic_install(src: &Path, dst: &Path) -> std::io::Result<()> {
    atomic_install_with(src, dst, |s, d| std::fs::rename(s, d))
}

/// fsync `path` if it exists. Treats NotFound as a no-op so callers don't
/// have to special-case the EXDEV path (where the original src was already
/// consumed by a copy).
///
/// Unix-only. On Windows `std::fs::File::open` returns a read-only handle
/// and `FlushFileBuffers` requires write access, so the natural
/// implementation here returns `ERROR_ACCESS_DENIED`. NTFS journals
/// metadata anyway, so the data-blocks-vs-rename ordering risk this
/// guards on Linux doesn't manifest the same way; treat the Windows path
/// as a no-op rather than carry a fragile reopen-with-write workaround.
fn fsync_file(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        match std::fs::File::open(path) {
            Ok(f) => f.sync_all(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

/// Test hook: like [`atomic_install`] but accepts an injectable `rename` so
/// tests can force the EXDEV fallback without needing a real cross-device
/// setup. Only the initial `src -> dst` rename is injected; the fallback's
/// `sibling -> dst` rename is plain `std::fs::rename` (same-device, can't
/// fail with EXDEV).
fn atomic_install_with<R>(src: &Path, dst: &Path, rename: R) -> std::io::Result<()>
where
    R: FnOnce(&Path, &Path) -> std::io::Result<()>,
{
    fsync_file(src)?;
    if let Err(rename_err) = rename(src, dst) {
        let ext = dst.extension().and_then(|e| e.to_str()).unwrap_or("tmp");
        let dst_sibling = dst.with_extension(format!("{ext}.kei-xdev-tmp-{}", std::process::id()));
        if let Err(copy_err) = std::fs::copy(src, &dst_sibling) {
            let _ = std::fs::remove_file(src);
            tracing::warn!(
                src = %src.display(),
                dst = %dst.display(),
                rename_err = %rename_err,
                copy_err = %copy_err,
                "rename failed and cross-device copy also failed"
            );
            return Err(rename_err);
        }
        // Fsync the sibling we just copied before renaming it into place.
        fsync_file(&dst_sibling)?;
        if let Err(final_err) = std::fs::rename(&dst_sibling, dst) {
            let _ = std::fs::remove_file(&dst_sibling);
            let _ = std::fs::remove_file(src);
            return Err(final_err);
        }
        let _ = std::fs::remove_file(src);
    }
    // Best-effort: a parent-dir fsync failure shouldn't fail the install,
    // but it's worth logging so an operator chasing durability incidents
    // can see it.
    if let Err(e) = fsync_parent_dir(dst) {
        tracing::warn!(
            path = %dst.display(),
            error = %e,
            "fsync of parent directory failed after atomic_install"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    #[test]
    fn same_device_rename_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.tmp");
        let dst = dir.path().join("dst.json");
        std::fs::write(&src, b"hello").unwrap();

        atomic_install(&src, &dst).expect("atomic_install");

        assert!(!src.exists(), "src must be consumed by the rename");
        assert_eq!(std::fs::read(&dst).unwrap(), b"hello");

        for entry in std::fs::read_dir(dir.path()).unwrap().flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            assert!(
                !name.contains("kei-xdev-tmp"),
                "unexpected sidecar tmp {name}"
            );
        }
    }

    #[test]
    fn missing_src_returns_err_without_touching_dst() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("nope.tmp");
        let dst = dir.path().join("dst.json");

        assert!(atomic_install(&src, &dst).is_err());
        assert!(!dst.exists(), "dst must not be created on failure");
    }

    /// Forces the rename to fail with a cross-device error, exercising the
    /// copy-to-sibling-then-rename fallback end-to-end. After the fallback,
    /// `dst` must contain the source bytes, `src` is removed, and no
    /// `.kei-xdev-tmp-*` file remains.
    #[test]
    fn exdev_fallback_installs_dst_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.tmp");
        let dst = dir.path().join("dst.json");
        std::fs::write(&src, b"xdev-payload").unwrap();

        let force_exdev = |_s: &Path, _d: &Path| -> io::Result<()> {
            Err(io::Error::new(
                io::ErrorKind::CrossesDevices,
                "simulated EXDEV",
            ))
        };

        atomic_install_with(&src, &dst, force_exdev).expect("EXDEV fallback should succeed");

        assert!(
            !src.exists(),
            "src must be removed after successful fallback"
        );
        assert_eq!(std::fs::read(&dst).unwrap(), b"xdev-payload");

        for entry in std::fs::read_dir(dir.path()).unwrap().flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            assert!(
                !name.contains("kei-xdev-tmp"),
                "EXDEV fallback must clean up its sibling tmp: {name}"
            );
        }
    }

    /// `fsync_parent_dir` returns `Ok(())` for an
    /// extant directory on every supported platform. On Unix it actually
    /// opens and fsyncs the parent; on Windows it's a documented no-op. The
    /// test pins both platforms to "doesn't error" so a future regression
    /// that drops the cfg gate or changes the open mode surfaces here.
    #[test]
    fn fsync_parent_dir_succeeds_for_extant_path() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("anchor.txt");
        std::fs::write(&file, b"x").unwrap();
        fsync_parent_dir(&file).expect("fsync_parent_dir should succeed");
    }

    /// `fsync_parent_dir` on Unix surfaces a NotFound when the parent itself
    /// is missing; on other platforms it's a no-op and returns Ok. Pinning
    /// the Unix branch makes accidental swallowing of the error visible.
    #[cfg(unix)]
    #[test]
    fn fsync_parent_dir_unix_errors_when_parent_missing() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("nope/sub/file.txt");
        let err = fsync_parent_dir(&file).expect_err("missing parent should error on unix");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    /// Happy-path coverage: a same-device install of a freshly written
    /// src succeeds end-to-end with the new fsync calls in the chain.
    /// Regression guard if a future refactor drops the fsync of src or the
    /// parent fsync and breaks the call (e.g. by accidentally borrowing a
    /// closed File past sync_all).
    #[test]
    fn atomic_install_round_trip_fsyncs_without_error() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.tmp");
        let dst = dir.path().join("dst.json");
        let payload = b"durable payload";
        std::fs::write(&src, payload).unwrap();

        atomic_install(&src, &dst).expect("atomic_install with fsync should succeed");

        assert!(!src.exists());
        assert_eq!(std::fs::read(&dst).unwrap(), payload);
    }

    /// If the initial rename fails and the cross-device copy also fails
    /// (e.g. dst parent is read-only), `src` is removed and the original
    /// rename error is returned; `dst` is never created.
    #[test]
    fn exdev_fallback_with_copy_failure_surfaces_rename_err() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.tmp");
        let nonexistent_parent = dir.path().join("no_such_dir");
        let dst = nonexistent_parent.join("dst.json");
        std::fs::write(&src, b"payload").unwrap();

        let force_exdev = |_s: &Path, _d: &Path| -> io::Result<()> {
            Err(io::Error::new(
                io::ErrorKind::CrossesDevices,
                "simulated EXDEV",
            ))
        };

        let err = atomic_install_with(&src, &dst, force_exdev).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::CrossesDevices);
        assert!(!dst.exists(), "dst must not be created when fallback fails");
        assert!(
            !src.exists(),
            "src must be cleaned up even when fallback fails"
        );
    }
}
