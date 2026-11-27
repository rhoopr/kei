//! One-time upgrade from the previous engine's state database.
//!
//! The old file is opened read-only after its account owner is verified, and
//! is never written. Its verified local copies are carried into a new state
//! file, which is validated before it replaces nothing: it is renamed into
//! its own name. The old file stays as the rollback point until the first
//! cycle finishes a full listing; then it, its SQLite companions and its
//! schema backups are deleted.

use std::path::{Path, PathBuf};

use anyhow::Context;
use rusqlite::{Connection, OptionalExtension};

use crate::state::db::account::{AccountOwner, open_verified_read_only};

use super::state::{ImportedCopy, State};

/// `kv` key naming the old state file while its cleanup is pending.
const LEGACY_KEY: &str = "legacy_state";
const BUNDLE_PREFIX: &str = ".kei-schema-backup-";
const BUNDLE_MEMBERS: [&str; 5] = [
    "manifest.json",
    "manifest.json.part",
    "config.toml",
    "restore.txt",
    "state.db",
];

/// What the import carried over, and what it left for normal planning.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ImportReport {
    /// Verified copies the old state recorded.
    pub(crate) recorded: u64,
    pub(crate) imported: u64,
    /// Carried over although the file is gone or its size changed since it
    /// was recorded. Such a copy is never taken over.
    pub(crate) changed_on_disk: u64,
    /// The recorded path is outside the current download directory.
    pub(crate) outside: u64,
    /// Another recorded copy already claimed the path.
    pub(crate) duplicates: u64,
}

/// Build and validate new state from `legacy` when `target` does not exist
/// yet. Returns `None` when there is nothing to upgrade.
pub(crate) async fn prepare(
    legacy: &Path,
    target: &Path,
    owner: &AccountOwner,
    directory: &Path,
) -> anyhow::Result<Option<ImportReport>> {
    if exists(target).await? || !exists(legacy).await? {
        return Ok(None);
    }
    let (legacy, owner_check, directory) =
        (legacy.to_path_buf(), owner.clone(), directory.to_path_buf());
    let legacy_for_read = legacy.clone();
    let (copies, mut report, provider) = tokio::task::spawn_blocking(move || {
        let conn = open_verified_read_only(&legacy_for_read, &owner_check)?;
        let provider: String =
            conn.query_row("SELECT provider_key FROM account_owner", [], |row| {
                row.get(0)
            })?;
        let (copies, report) = read_copies(&conn, &directory)?;
        anyhow::Ok((copies, report, provider))
    })
    .await??;

    let building = building_path(target);
    remove_sqlite_files(&building).await?;
    let state = State::open(&building).await?;
    super::access::bind(&state, owner.key(), Some(&provider)).await?;
    report.imported = state.insert_imported(copies).await?;
    state.set_kv(LEGACY_KEY, &legacy.to_string_lossy()).await?;
    state.validate().await?;
    state.close().await?;
    tokio::fs::rename(&building, target)
        .await
        .with_context(|| format!("Could not install upgraded state at {}", target.display()))?;
    crate::fs_util::fsync_parent_dir_async(target).await?;
    Ok(Some(report))
}

/// After a cycle that listed every zone completely, delete the old state
/// file, its companions and its schema backups. Returns the bytes freed, or
/// `None` when no cleanup was pending.
// CONTRACT: UPGRADE_CLEANUP_REQUIRES_VALIDATED_STATE
// The caller runs this only after a cycle in which every zone listed to the
// end without error; the new state was integrity-checked before it was used.
pub(crate) async fn finish(state: &State) -> anyhow::Result<Option<u64>> {
    let Some(legacy) = state.get_kv(LEGACY_KEY).await? else {
        return Ok(None);
    };
    let legacy = PathBuf::from(legacy);
    let mut freed = remove_sqlite_files(&legacy).await?;
    freed += remove_backups(&legacy).await?;
    state.clear_imported().await?;
    state.delete_kv(LEGACY_KEY).await?;
    Ok(Some(freed))
}

fn building_path(target: &Path) -> PathBuf {
    let mut name = target.as_os_str().to_owned();
    name.push(".importing");
    PathBuf::from(name)
}

/// Remove a SQLite file and its companions; returns the bytes removed.
pub(crate) async fn remove_sqlite_files(path: &Path) -> anyhow::Result<u64> {
    let mut freed = 0;
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        let file = PathBuf::from(name);
        match tokio::fs::symlink_metadata(&file).await {
            Ok(meta) if meta.is_file() => {
                tokio::fs::remove_file(&file).await?;
                freed += meta.len();
            }
            Ok(_) => anyhow::bail!("{} is not a regular file", file.display()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(freed)
}

/// Remove schema backup bundles whose manifest names `legacy`. Bundles for
/// other databases, or with unexpected contents, are left alone.
async fn remove_backups(legacy: &Path) -> anyhow::Result<u64> {
    let Some(parent) = legacy.parent() else {
        return Ok(0);
    };
    let original = std::path::absolute(legacy)?.display().to_string();
    let mut freed = 0;
    let mut entries = match tokio::fs::read_dir(parent).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error.into()),
    };
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with(BUNDLE_PREFIX) || !entry.file_type().await?.is_dir()
        {
            continue;
        }
        let bundle = entry.path();
        let manifest = match tokio::fs::read(bundle.join("manifest.json")).await {
            Ok(bytes) => serde_json::from_slice::<serde_json::Value>(&bytes).ok(),
            Err(_) => None,
        };
        let Some(manifest) = manifest else {
            continue;
        };
        if manifest
            .get("database_path_display")
            .and_then(serde_json::Value::as_str)
            != Some(original.as_str())
        {
            continue;
        }
        let mut removed = 0;
        for member in BUNDLE_MEMBERS {
            let file = bundle.join(member);
            if let Ok(meta) = tokio::fs::symlink_metadata(&file).await
                && meta.is_file()
            {
                tokio::fs::remove_file(&file).await?;
                removed += meta.len();
            }
        }
        // Anything else in the bundle keeps it in place.
        if tokio::fs::remove_dir(&bundle).await.is_ok() {
            crate::fs_util::fsync_parent_dir_async_best_effort(&bundle).await;
        }
        freed += removed;
    }
    Ok(freed)
}

async fn exists(path: &Path) -> anyhow::Result<bool> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

/// Every verified local copy the old state recorded inside the download
/// directory, counting those whose file is gone or changed size.
fn read_copies(
    conn: &Connection,
    directory: &Path,
) -> anyhow::Result<(Vec<ImportedCopy>, ImportReport)> {
    let mut rows = Vec::new();
    if has_columns(
        conn,
        "assets",
        &[
            "id",
            "version_size",
            "local_path",
            "size_bytes",
            "checksum",
            "status",
        ],
    )? {
        let library = if has_columns(conn, "assets", &["library"])? {
            "library"
        } else {
            "'PrimarySync'"
        };
        let local = if has_columns(conn, "assets", &["local_checksum"])? {
            "local_checksum"
        } else {
            "NULL"
        };
        let sql = format!(
            "SELECT {library}, id, version_size, local_path, size_bytes, {local}, checksum FROM assets
             WHERE status = 'downloaded' AND local_path IS NOT NULL"
        );
        collect(conn, &sql, &mut rows)?;
        let joined = if library == "library" {
            "a.library"
        } else {
            library
        };
        if has_columns(
            conn,
            "asset_metadata_paths",
            &[
                "library",
                "id",
                "version_size",
                "local_path",
                "local_checksum",
                "provider_checksum",
            ],
        )? {
            collect(
                conn,
                &format!(
                    "SELECT p.library, p.id, p.version_size, p.local_path, a.size_bytes, p.local_checksum, p.provider_checksum
                     FROM asset_metadata_paths p JOIN assets a
                       ON a.id = p.id AND a.version_size = p.version_size AND {joined} = p.library
                     WHERE a.status = 'downloaded'"
                ),
                &mut rows,
            )?;
        }
    }
    if has_columns(
        conn,
        "provider_active_destinations",
        &[
            "library",
            "master",
            "version_size",
            "path",
            "size_bytes",
            "local_checksum",
            "checksum",
            "verified_media",
        ],
    )? {
        collect(
            conn,
            "SELECT library, master, version_size, path, size_bytes, local_checksum, checksum
             FROM provider_active_destinations WHERE verified_media = 1",
            &mut rows,
        )?;
    }

    let mut report = ImportReport {
        recorded: u64::try_from(rows.len())?,
        ..ImportReport::default()
    };
    let mut seen = std::collections::HashSet::new();
    let mut copies = Vec::new();
    for (zone, master, rendition, path, size, sha256, checksum) in rows {
        let absolute = if Path::new(&path).is_absolute() {
            PathBuf::from(&path)
        } else {
            directory.join(&path)
        };
        let Some(relative) = relative_to(directory, &absolute) else {
            report.outside += 1;
            continue;
        };
        if !seen.insert(relative.clone()) {
            report.duplicates += 1;
            continue;
        }
        let size = u64::try_from(size)?;
        // Kept either way: the planner takes over only a file with the
        // recorded size, and verify reports the rest.
        match std::fs::symlink_metadata(&absolute) {
            Ok(meta) if meta.is_file() && meta.len() == size => {}
            _ => report.changed_on_disk += 1,
        }
        copies.push(ImportedCopy {
            path: relative,
            zone,
            master,
            rendition,
            size,
            sha256: sha256.filter(|hash| crate::download::core_io::parse_sha256(hash).is_some()),
            checksum,
        });
    }
    Ok((copies, report))
}

type Row = (String, String, String, String, i64, Option<String>, String);

fn collect(conn: &Connection, sql: &str, rows: &mut Vec<Row>) -> anyhow::Result<()> {
    let mut stmt = conn.prepare(sql)?;
    let found = stmt.query_map([], |row| {
        Ok((
            row.get(0)?,
            row.get(1)?,
            row.get(2)?,
            row.get(3)?,
            row.get(4)?,
            row.get(5)?,
            row.get(6)?,
        ))
    })?;
    for row in found {
        rows.push(row?);
    }
    Ok(())
}

fn has_columns(conn: &Connection, table: &str, columns: &[&str]) -> anyhow::Result<bool> {
    let exists: Option<String> = conn
        .query_row(
            "SELECT name FROM sqlite_schema WHERE type = 'table' AND name = ?1",
            [table],
            |row| row.get(0),
        )
        .optional()?;
    if exists.is_none() {
        return Ok(false);
    }
    let mut stmt = conn.prepare(&format!("PRAGMA table_info(\"{table}\")"))?;
    let present: Vec<String> = stmt
        .query_map([], |row| row.get(1))?
        .collect::<Result<_, _>>()?;
    Ok(columns
        .iter()
        .all(|column| present.iter().any(|name| name == column)))
}

fn relative_to(directory: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(directory).ok()?;
    let parts = relative
        .components()
        .map(|part| match part {
            std::path::Component::Normal(part) => part.to_str().map(ToOwned::to_owned),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    (!parts.is_empty()).then(|| parts.join("/"))
}
