//! Legacy path migration from icloudpd-rs to kei.
//!
//! On first run after upgrade, detects old data directories and copies
//! config, cookies, and state databases to the new XDG-style paths.
//! Old files are left in place so the user can roll back if needed.

use std::path::Path;

use crate::config::expand_tilde;

/// New default paths.
const NEW_CONFIG_PATH: &str = "~/.config/kei/config.toml";
const NEW_COOKIE_DIR: &str = "~/.config/kei/cookies";

/// Legacy paths from icloudpd-rs.
const OLD_CONFIG_PATH: &str = "~/.config/icloudpd-rs/config.toml";
const OLD_COOKIE_DIR: &str = "~/.icloudpd-rs";

/// Summary of what was migrated.
#[derive(Debug, Default)]
pub(crate) struct MigrationReport {
    pub warnings: Vec<String>,
    pub config_migrated: bool,
    pub cookies_migrated: bool,
}

/// Check for legacy icloudpd-rs paths and copy data to the new kei locations.
///
/// Called early in `main()`, before config loading. Returns `None` if no
/// migration was needed (new paths already exist or no old paths found).
pub fn migrate_legacy_paths() -> Option<MigrationReport> {
    let new_config = expand_tilde(NEW_CONFIG_PATH);
    let new_cookie_dir = expand_tilde(NEW_COOKIE_DIR);

    // If new config already exists, no migration needed.
    if new_config.exists() {
        return None;
    }

    let old_config = expand_tilde(OLD_CONFIG_PATH);
    let old_cookie_dir = expand_tilde(OLD_COOKIE_DIR);

    let has_old_config = old_config.is_file();
    let has_old_cookies = old_cookie_dir.is_dir();

    if !has_old_config && !has_old_cookies {
        return None;
    }

    let mut report = MigrationReport::default();

    // Migrate config file
    if has_old_config {
        match migrate_file(&old_config, &new_config) {
            Ok(true) => {
                report.config_migrated = true;
                report.warnings.push(format!(
                    "Migrated config from {} to {}",
                    old_config.display(),
                    new_config.display()
                ));
            }
            Ok(false) => {} // destination already exists
            Err(e) => {
                report.warnings.push(format!(
                    "Failed to migrate config from {}: {e}. Using old path as fallback.",
                    old_config.display()
                ));
            }
        }
    }

    // Migrate cookie/session/state files
    if has_old_cookies {
        match migrate_directory_contents(&old_cookie_dir, &new_cookie_dir) {
            Ok(count) if count > 0 => {
                report.cookies_migrated = true;
                report.warnings.push(format!(
                    "Migrated {count} files from {} to {}",
                    old_cookie_dir.display(),
                    new_cookie_dir.display()
                ));
            }
            Ok(_) => {} // nothing to copy or all already existed
            Err(e) => {
                report.warnings.push(format!(
                    "Failed to migrate data from {}: {e}. Using old path as fallback.",
                    old_cookie_dir.display()
                ));
            }
        }
    }

    if !report.config_migrated && !report.cookies_migrated {
        return None;
    }

    report.warnings.push(
        "The old paths will continue to work but are deprecated. \
         Please update your scripts and config to use ~/.config/kei/."
            .to_string(),
    );

    Some(report)
}

/// Copy a single file to a new location, creating parent directories.
/// Returns `Ok(true)` if copied, `Ok(false)` if destination already exists.
fn migrate_file(src: &Path, dst: &Path) -> anyhow::Result<bool> {
    use anyhow::Context;
    if dst.exists() {
        return Ok(false);
    }
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating parent directory {}", parent.display()))?;
    }
    std::fs::copy(src, dst)
        .with_context(|| format!("copying {} to {}", src.display(), dst.display()))?;
    Ok(true)
}

/// Copy all files (not subdirectories) from one directory to another.
/// Skips files that already exist at the destination.
/// Returns the number of files successfully copied.
fn migrate_directory_contents(src_dir: &Path, dst_dir: &Path) -> anyhow::Result<usize> {
    use anyhow::Context;
    std::fs::create_dir_all(dst_dir)
        .with_context(|| format!("creating destination directory {}", dst_dir.display()))?;

    let mut count = 0;
    for entry in
        std::fs::read_dir(src_dir).with_context(|| format!("reading {}", src_dir.display()))?
    {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if !file_type.is_file() {
            continue;
        }
        let dst_path = dst_dir.join(entry.file_name());
        if dst_path.exists() {
            continue;
        }
        std::fs::copy(entry.path(), &dst_path).with_context(|| {
            format!(
                "copying {} to {}",
                entry.path().display(),
                dst_path.display()
            )
        })?;
        count += 1;
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn migrate_file_copies_to_new_location() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        let src = base.join("old/config.toml");
        let dst = base.join("new/config.toml");

        std::fs::create_dir_all(src.parent().unwrap()).unwrap();
        std::fs::write(&src, "key = \"value\"").unwrap();

        assert!(migrate_file(&src, &dst).unwrap());
        assert_eq!(std::fs::read_to_string(&dst).unwrap(), "key = \"value\"");
        // Source still exists
        assert!(src.exists());
    }

    #[test]
    fn migrate_file_skips_existing_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        let src = base.join("old/config.toml");
        let dst = base.join("new/config.toml");

        std::fs::create_dir_all(src.parent().unwrap()).unwrap();
        std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
        std::fs::write(&src, "old content").unwrap();
        std::fs::write(&dst, "new content").unwrap();

        assert!(!migrate_file(&src, &dst).unwrap());
        // Destination unchanged
        assert_eq!(std::fs::read_to_string(&dst).unwrap(), "new content");
    }

    #[test]
    fn migrate_directory_copies_files() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        let src_dir = base.join("old");
        let dst_dir = base.join("new");

        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::write(src_dir.join("user.json"), "cookies").unwrap();
        std::fs::write(src_dir.join("user.session"), "session").unwrap();
        std::fs::write(src_dir.join("user.db"), "database").unwrap();

        let count = migrate_directory_contents(&src_dir, &dst_dir).unwrap();
        assert_eq!(count, 3);
        assert_eq!(
            std::fs::read_to_string(dst_dir.join("user.json")).unwrap(),
            "cookies"
        );
        assert_eq!(
            std::fs::read_to_string(dst_dir.join("user.session")).unwrap(),
            "session"
        );
    }

    #[test]
    fn migrate_directory_skips_existing_files() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        let src_dir = base.join("old");
        let dst_dir = base.join("new");

        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::create_dir_all(&dst_dir).unwrap();
        std::fs::write(src_dir.join("user.json"), "old cookies").unwrap();
        std::fs::write(dst_dir.join("user.json"), "new cookies").unwrap();
        std::fs::write(src_dir.join("user.db"), "database").unwrap();

        let count = migrate_directory_contents(&src_dir, &dst_dir).unwrap();
        assert_eq!(count, 1); // only user.db copied
        assert_eq!(
            std::fs::read_to_string(dst_dir.join("user.json")).unwrap(),
            "new cookies"
        );
    }

    #[test]
    fn migrate_directory_skips_subdirectories() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        let src_dir = base.join("old");
        let dst_dir = base.join("new");

        std::fs::create_dir_all(src_dir.join("subdir")).unwrap();
        std::fs::write(src_dir.join("file.txt"), "content").unwrap();

        let count = migrate_directory_contents(&src_dir, &dst_dir).unwrap();
        assert_eq!(count, 1);
        assert!(!dst_dir.join("subdir").exists());
    }

    #[cfg(unix)]
    #[test]
    fn migrate_directory_skips_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        let src_dir = base.join("old");
        let dst_dir = base.join("new");

        std::fs::create_dir_all(&src_dir).unwrap();
        let real_file = base.join("real.txt");
        std::fs::write(&real_file, "target content").unwrap();
        std::os::unix::fs::symlink(&real_file, src_dir.join("link.txt")).unwrap();
        std::fs::write(src_dir.join("regular.txt"), "regular").unwrap();

        let count = migrate_directory_contents(&src_dir, &dst_dir).unwrap();
        assert_eq!(count, 1);
        assert!(dst_dir.join("regular.txt").exists());
        assert!(!dst_dir.join("link.txt").exists());
    }

    #[test]
    fn migrate_directory_empty_source_returns_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        let src_dir = base.join("empty_src");
        let dst_dir = base.join("dst");

        std::fs::create_dir_all(&src_dir).unwrap();

        let count = migrate_directory_contents(&src_dir, &dst_dir).unwrap();
        assert_eq!(count, 0);
        assert!(dst_dir.exists()); // dst_dir is still created
    }

    #[cfg(unix)]
    #[test]
    fn migrate_file_read_only_dest_parent_returns_error() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        let src = base.join("src/config.toml");
        std::fs::create_dir_all(src.parent().unwrap()).unwrap();
        std::fs::write(&src, "data").unwrap();

        let readonly_dir = base.join("readonly");
        std::fs::create_dir_all(&readonly_dir).unwrap();
        std::fs::set_permissions(&readonly_dir, std::fs::Permissions::from_mode(0o444)).unwrap();

        let dst = readonly_dir.join("subdir/config.toml");
        let result = migrate_file(&src, &dst);
        assert!(result.is_err());

        // Restore permissions so tempdir cleanup succeeds
        std::fs::set_permissions(&readonly_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}
