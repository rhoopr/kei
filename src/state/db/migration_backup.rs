//! A complete recovery bundle must be durable before any owned-schema mutation.

use std::path::{Path, PathBuf};

#[cfg(any(unix, windows))]
use rusqlite::Connection;

#[cfg(windows)]
mod windows;

#[cfg(windows)]
type PreparedBundle = windows::Stage;
#[cfg(unix)]
type PreparedBundle = ();

use crate::config::ConfigSource;
use crate::state::error::StateError;

pub(super) fn create(
    path: &Path,
    source: &ConfigSource,
    version: i32,
) -> Result<PathBuf, StateError> {
    if matches!(source, ConfigSource::Unavailable) {
        return Err(StateError::MigrationConfigUnavailable);
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (path, version);
        Err(StateError::MigrationBackupUnsupported)
    }
    #[cfg(any(unix, windows))]
    {
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let bundle = parent.join(format!(".kei-schema-backup-{}", uuid::Uuid::new_v4()));
        #[cfg(unix)]
        let stage = bundle.clone();
        #[cfg(windows)]
        let stage = bundle.with_extension("partial");
        let prepare = || {
            write_bundle(path, source, version, &stage).map_err(|source| {
                StateError::MigrationBackup {
                    path: stage.clone(),
                    source,
                }
            })
        };
        #[cfg(unix)]
        prepare()?;
        #[cfg(windows)]
        let mut prepared = prepare()?;
        #[cfg(windows)]
        prepared
            .publish(&bundle)
            .map_err(|source| StateError::MigrationBackup {
                path: stage,
                source,
            })?;
        #[cfg(all(test, windows))]
        windows_tests::interruption_point("schema-backup-published");
        Ok(bundle)
    }
}

#[cfg(any(unix, windows))]
fn write_bundle(
    path: &Path,
    source: &ConfigSource,
    version: i32,
    bundle: &Path,
) -> std::io::Result<PreparedBundle> {
    // Exclusive directory creation, not create_dir_all: never reuse a partial
    // bundle, follow a pre-existing link, or overwrite prior recovery evidence.
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(bundle)?;
    }
    #[cfg(windows)]
    let directory_guard = windows::create_private_dir(bundle)?;
    #[cfg(all(test, target_os = "linux"))]
    crate::test_helpers::process_death_point("schema-backup-created");
    #[cfg(all(test, windows))]
    windows_tests::interruption_point("schema-backup-created");
    let database = bundle.join("state.db");
    drop(private_file(&database)?);
    {
        // The caller holds BEGIN IMMEDIATE on the source throughout the copy,
        // durable publication and migration. A separate reader is necessary:
        // SQLite refuses backup_step on a source with a write transaction.
        let input = super::account::open_read_only(path).map_err(std::io::Error::other)?;
        let mut output =
            Connection::open_with_flags(&database, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
                .map_err(std::io::Error::other)?;
        match rusqlite::backup::Backup::new(&input, &mut output)
            .map_err(std::io::Error::other)?
            .step(-1)
            .map_err(std::io::Error::other)?
        {
            rusqlite::backup::StepResult::Done => {}
            _ => {
                return Err(std::io::Error::other(
                    "SQLite backup was busy; stop competing writers and retry",
                ));
            }
        }
        output
            .pragma_update(None, "journal_mode", "DELETE")
            .map_err(std::io::Error::other)?;
        let integrity: String = output
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .map_err(std::io::Error::other)?;
        if integrity != "ok" {
            return Err(std::io::Error::other(
                "SQLite backup failed integrity validation",
            ));
        }
    }
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&database)?
        .sync_all()?;
    #[cfg(all(test, target_os = "linux"))]
    crate::test_helpers::process_death_point("schema-backup-copied");
    #[cfg(all(test, windows))]
    windows_tests::interruption_point("schema-backup-copied");
    #[cfg(all(test, unix))]
    tests::backup_io_fault(path)?;
    #[cfg(all(test, windows))]
    windows_tests::backup_io_fault(path)?;
    let config = match source {
        ConfigSource::File { path, bytes } => {
            write_private(&bundle.join("config.toml"), bytes)?;
            serde_json::json!({"kind": "file", "path_display": path.display().to_string(),
                "path_native_hex": native_path(path), "sha256": digest(&bundle.join("config.toml"))?})
        }
        ConfigSource::Absent { path } => serde_json::json!({"kind": "absent",
            "path_display": path.display().to_string(), "path_native_hex": native_path(path)}),
        ConfigSource::Unavailable => {
            return Err(std::io::Error::other("startup config context unavailable"));
        }
    };
    write_private(&bundle.join("restore.txt"), RESTORE.as_bytes())?;
    let original_path = std::path::absolute(path)?;
    let manifest = serde_json::to_vec_pretty(&serde_json::json!({
        "format": 1, "schema_version": version, "kei_version": env!("CARGO_PKG_VERSION"),
        "database_path_display": original_path.display().to_string(),
        "database_path_native_hex": native_path(&original_path),
        "native_path_encoding": NATIVE_PATH_ENCODING,
        "database_sha256": digest(&database)?, "config": config,
        "runtime_context": "CLI overrides, environment values and credentials are not captured. Reapply the original invocation/environment separately; config.toml alone does not restore them.",
    }))?;
    let stage = bundle.join("manifest.json.part");
    write_private(&stage, &manifest)?;
    // Sync contents and the new bundle entry before marking it complete.
    #[cfg(unix)]
    {
        crate::fs_util::fsync_parent_dir(&stage)?;
        crate::fs_util::fsync_parent_dir(bundle)?;
    }
    #[cfg(all(test, target_os = "linux"))]
    crate::test_helpers::process_death_point("schema-backup-staged");
    #[cfg(unix)]
    publish_manifest(&stage, &bundle.join("manifest.json"))?;
    // The Windows directory remains .partial even with a prepared manifest.
    // Only the later write-through, no-overwrite directory move completes it.
    #[cfg(windows)]
    crate::fs_util::move_file_no_replace_blocking(&stage, &bundle.join("manifest.json"))?;
    #[cfg(all(test, windows))]
    windows_tests::interruption_point("schema-backup-staged");
    #[cfg(all(test, target_os = "linux"))]
    crate::test_helpers::process_death_point("schema-backup-published");
    #[cfg(unix)]
    {
        Ok(())
    }
    #[cfg(windows)]
    {
        Ok(directory_guard)
    }
}

#[cfg(windows)]
fn private_file(path: &Path) -> std::io::Result<std::fs::File> {
    windows::private_file(path)
}

#[cfg(unix)]
fn private_file(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(any(unix, windows))]
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = private_file(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(unix)]
fn publish_manifest(stage: &Path, published: &Path) -> std::io::Result<()> {
    std::fs::hard_link(stage, published)?;
    crate::fs_util::fsync_parent_dir(published)
}

#[cfg(unix)]
fn native_path(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    data_encoding::HEXLOWER.encode(path.as_os_str().as_bytes())
}

#[cfg(unix)]
const NATIVE_PATH_ENCODING: &str = "unix_bytes_hex";
#[cfg(windows)]
const NATIVE_PATH_ENCODING: &str = "windows_utf16le_hex";

#[cfg(windows)]
fn native_path(path: &Path) -> String {
    use std::os::windows::ffi::OsStrExt;
    let bytes: Vec<u8> = path
        .as_os_str()
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect();
    data_encoding::HEXLOWER.encode(&bytes)
}

#[cfg(any(unix, windows))]
fn digest(path: &Path) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 8192];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hash.update(buffer.get(..read).ok_or_else(|| {
            std::io::Error::other("file read exceeded the backup checksum buffer")
        })?);
    }
    Ok(data_encoding::HEXLOWER.encode(&hash.finalize()))
}

#[cfg(any(unix, windows))]
const RESTORE: &str = "Pre-migration state/config recovery\n\n\
Only a final bundle containing manifest.json is complete. A directory ending in .partial is incomplete even when it contains a manifest. Ignore incomplete bundles.\n\
Stop every kei process and state writer before restoration. Keep this bundle unchanged.\n\
Verify the original database path and the SHA-256 values for state.db and (when present) config.toml in manifest.json. Keep account ownership validation enabled; a backup for another account or database path must not be used.\n\
Use the previous compatible kei binary. Preserve the current database AND every existing -journal/-wal/-shm companion together under separate names, then copy state.db to the original database path recorded in manifest.json. Never delete a hot rollback journal or open the interrupted original in a writable SQLite tool to bypass ownership validation. Never combine the backup database with newer journal/WAL/SHM files. Keep restored files private: Unix mode 600, or a protected owner-only Windows ACL.\n\
For config.kind=file, preserve the current config separately and copy config.toml to its recorded source path with private permissions. These are the exact bytes loaded at startup, not a regenerated config. For config.kind=absent, no file was loaded: preserve any later file separately and restore that absence. native_path_encoding identifies hex-encoded Unix bytes or Windows UTF-16LE units; display fields may be lossy. Windows publication uses native write-through moves, not POSIX directory fsync or a physical power-loss guarantee.\n\
CLI overrides, environment variables, credentials and service arguments are NOT included. Reapply the original invocation and environment separately. Defaults depend on the compatible binary. A config-only restore does not reproduce overrides.\n\
This bundle contains no media. Restoring it is supported only before subsequent media/metadata changes, or together with a separately matched media backup. Never downgrade only the binary against migrated state.\n";

#[cfg(all(test, unix))]
mod tests;
#[cfg(all(test, windows))]
mod windows_tests;
