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
    DestinationExists,
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
) -> std::io::Result<PublishResult> {
    #[cfg(target_os = "linux")]
    let result =
        renameat2_confined_blocking(part, destination, libc::RENAME_NOREPLACE).or_else(|error| {
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
