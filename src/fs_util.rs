//! Shared filesystem primitives.

use std::path::Path;

/// Install `src` at `dst` atomically. Prefers `rename` (atomic on the same
/// device); on EXDEV, copies to a sibling of `dst` on the destination device
/// and renames that sibling into place so a mid-copy crash can't expose a
/// half-written `dst`.
pub(crate) fn atomic_install(src: &Path, dst: &Path) -> std::io::Result<()> {
    atomic_install_with(src, dst, |s, d| std::fs::rename(s, d))
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
        if let Err(final_err) = std::fs::rename(&dst_sibling, dst) {
            let _ = std::fs::remove_file(&dst_sibling);
            let _ = std::fs::remove_file(src);
            return Err(final_err);
        }
        let _ = std::fs::remove_file(src);
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
