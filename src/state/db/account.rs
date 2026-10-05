//! Independent account ownership, checked before schema migration or state use.
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags, OptionalExtension};

use crate::state::{error::StateError, schema};

const ACCOUNT_OWNER_FORMAT_VERSION: i64 = 1;

/// Configured scope plus an optional independently authenticated provider pin.
#[derive(Clone)]
pub(crate) struct AccountOwner {
    key: String,
    provider: Option<String>,
}

impl AccountOwner {
    #[must_use]
    pub(crate) fn configured(username: &str, realm: &str) -> Self {
        Self {
            key: crate::account::namespace(username, realm),
            provider: None,
        }
    }

    pub(crate) fn authenticated(
        username: &str,
        realm: &str,
        data: &crate::auth::AccountLoginResponse,
    ) -> Result<Self, StateError> {
        let dsid = data
            .ds_info
            .as_ref()
            .and_then(|info| info.dsid.as_deref())
            .filter(|value| !value.trim().is_empty())
            .ok_or(StateError::AccountIdentityUnavailable)?;
        Ok(Self {
            key: crate::account::namespace(username, realm),
            provider: Some(crate::account::provider_fingerprint(realm, dsid)),
        })
    }
}

fn owner_row(conn: &Connection) -> Result<Option<(i64, String, String)>, StateError> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='account_owner')",
        [],
        |row| row.get(0),
    )?;
    if !exists {
        return Ok(None);
    }
    let count: i64 = conn.query_row("SELECT count(*) FROM account_owner", [], |row| row.get(0))?;
    if count != 1 {
        return Err(StateError::AccountOwnerMismatch);
    }
    conn.query_row(
        "SELECT version, account_key, provider_key FROM account_owner WHERE singleton=1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )
    .optional()
    .map_err(StateError::Migration)
}

pub(super) fn validate(conn: &Connection, owner: &AccountOwner) -> Result<(), StateError> {
    let (version, key, provider) = owner_row(conn)?.ok_or(StateError::AccountOwnerMissing)?;
    if version != ACCOUNT_OWNER_FORMAT_VERSION
        || key != owner.key
        || (provider.len() != 64 || !provider.bytes().all(|byte| byte.is_ascii_hexdigit()))
        || owner
            .provider
            .as_ref()
            .is_some_and(|expected| expected != &provider)
    {
        return Err(StateError::AccountOwnerMismatch);
    }
    Ok(())
}

pub(super) fn validate_authenticated(
    conn: &Connection,
    owner: &AccountOwner,
) -> Result<(), StateError> {
    if owner.provider.is_none() {
        return Err(StateError::AccountIdentityUnavailable);
    }
    validate(conn, owner)
}

fn bind(conn: &Connection, owner: &AccountOwner) -> Result<(), StateError> {
    let provider = owner
        .provider
        .as_ref()
        .ok_or(StateError::AccountIdentityUnavailable)?;
    conn.execute_batch("BEGIN IMMEDIATE; CREATE TABLE account_owner (singleton INTEGER PRIMARY KEY CHECK(singleton=1), version INTEGER NOT NULL, account_key TEXT NOT NULL, provider_key TEXT NOT NULL);")?;
    conn.execute(
        "INSERT INTO account_owner VALUES (1, ?1, ?2, ?3)",
        (ACCOUNT_OWNER_FORMAT_VERSION, &owner.key, provider),
    )?;
    conn.execute_batch("COMMIT")?;
    Ok(())
}

pub(super) fn open(path: &Path, owner: &AccountOwner) -> Result<Connection, StateError> {
    // Reserve the new file exclusively. A pre-existing empty/unowned file is
    // never silently claimed, including one left by an interrupted creation.
    if owner.provider.is_none()
        && !path.try_exists().map_err(|source| StateError::TempPath {
            path: path.to_path_buf(),
            source,
        })?
    {
        return Err(StateError::AccountIdentityUnavailable);
    }
    let created = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(file) => {
            drop(file);
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(source) => {
            return Err(StateError::TempPath {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    if !created {
        let inspection = open_read_only(path)?;
        validate(&inspection, owner)?;
    }
    let conn =
        Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE).map_err(|source| {
            StateError::Open {
                path: path.to_path_buf(),
                source,
            }
        })?;
    // A refused connection must not checkpoint a crash-left WAL on drop.
    // Recheck after the writable open, including a replaced-file race.
    conn.set_db_config(
        rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,
        true,
    )?;
    if created {
        bind(&conn, owner)?;
    } else {
        validate(&conn, owner)?;
    }
    conn.set_db_config(
        rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,
        false,
    )?;
    Ok(conn)
}

pub(super) fn open_read_only(path: &Path) -> Result<Connection, StateError> {
    let conn =
        Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(|source| {
            StateError::Open {
                path: path.to_path_buf(),
                source,
            }
        })?;
    conn.set_db_config(
        rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,
        true,
    )?;
    Ok(conn)
}

async fn entry_exists(path: &Path) -> Result<bool, StateError> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(StateError::TempPath {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Discover only the new namespace. An unowned legacy file is an explicit
/// migration requirement, never automatic fallback or permission for fresh state.
pub(crate) async fn state_path(
    directory: &Path,
    username: &str,
    realm: &str,
) -> Result<PathBuf, StateError> {
    let path = directory.join(format!("{}.db", crate::account::namespace(username, realm)));
    let legacy = directory.join(format!(
        "{}.db",
        crate::auth::session::sanitize_username(username)
    ));
    if !entry_exists(&path).await? && entry_exists(&legacy).await? {
        return Err(StateError::LegacyAccountMigrationRequired);
    }
    Ok(path)
}

/// Copy a confirmed legacy snapshot, bind the staged copy and publish without
/// replacement. Source rows, checkpoints, debt and SQLite companions remain.
pub(crate) async fn adopt_legacy(
    source: &Path,
    destination: &Path,
    owner: &AccountOwner,
    username: &str,
    realm: &str,
) -> Result<(), StateError> {
    if owner.provider.is_none() {
        return Err(StateError::AccountIdentityUnavailable);
    }
    let source = source.to_path_buf();
    let destination = destination.to_path_buf();
    let owner = owner.clone();
    let username = username.to_string();
    let realm = realm.to_string();
    tokio::task::spawn_blocking(move || {
        let parent = destination
            .parent()
            .ok_or(StateError::AccountOwnerMismatch)?;
        std::fs::create_dir_all(parent).map_err(|source| StateError::ParentDir {
            path: parent.to_path_buf(),
            source,
        })?;
        let stage = parent.join(format!(".account-adoption-{}.db", uuid::Uuid::new_v4()));
        let result = (|| {
            copy_and_bind_legacy(&source, &stage, &owner, &username, &realm)?;
            // Same-directory hard-link publication is atomic and refuses an
            // existing destination. No rename-overwrite or cross-device copy.
            std::fs::hard_link(&stage, &destination).map_err(|source| StateError::TempPath {
                path: destination.clone(),
                source,
            })?;
            #[cfg(all(test, target_os = "linux"))]
            crate::test_helpers::process_death_point("account-adoption-published");
            crate::fs_util::fsync_parent_dir(&destination).map_err(|source| {
                StateError::TempPath {
                    path: destination.clone(),
                    source,
                }
            })?;
            #[cfg(all(test, target_os = "linux"))]
            crate::test_helpers::process_death_point("account-adoption-directory-synced");
            Ok(())
        })();
        // Only our private stage is removed. Never remove a published target
        // after an uncertain fsync result, or any legacy source/companion.
        let _ = std::fs::remove_file(&stage);
        result
    })
    .await?
}

fn copy_and_bind_legacy(
    source: &Path,
    stage: &Path,
    owner: &AccountOwner,
    username: &str,
    realm: &str,
) -> Result<(), StateError> {
    let input = open_read_only(source)
        .map_err(|_unreadable| StateError::AccountMigrationSourceUnreadable)?;
    // A bound DB is not a legacy adoption source, even for this owner.
    if owner_row(&input)?.is_some() {
        return Err(StateError::AccountOwnerMismatch);
    }
    let scoped: bool = input.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='scoped_db_sync_tokens')",
        [],
        |row| row.get(0),
    )?;
    if scoped {
        let conflict: bool = input.query_row("SELECT EXISTS(SELECT 1 FROM scoped_db_sync_tokens WHERE account != ?1 OR provider != 'icloud')", (&username,), |row| row.get(0))?;
        if conflict {
            return Err(StateError::AccountOwnerMismatch);
        }
        let mut statement = input.prepare("SELECT scope_json FROM scoped_db_sync_tokens")?;
        let scopes = statement.query_map([], |row| row.get::<_, String>(0))?;
        for scope in scopes {
            let value: serde_json::Value = serde_json::from_str(&scope?)
                .map_err(|_invalid| StateError::AccountOwnerMismatch)?;
            if value.get("domain").and_then(serde_json::Value::as_str) != Some(realm) {
                return Err(StateError::AccountOwnerMismatch);
            }
        }
    }
    let version = schema::get_schema_version(&input)?;
    if version > schema::SCHEMA_VERSION {
        return Err(StateError::UnsupportedSchemaVersion {
            found: version,
            expected: schema::SCHEMA_VERSION,
        });
    }
    let before: i64 = input.pragma_query_value(None, "data_version", |row| row.get(0))?;
    let mut output = Connection::open(stage).map_err(|error| StateError::Open {
        path: stage.to_path_buf(),
        source: error,
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(stage, std::fs::Permissions::from_mode(0o600)).map_err(
            |source| StateError::TempPath {
                path: stage.to_path_buf(),
                source,
            },
        )?;
    }
    #[cfg(all(test, target_os = "linux"))]
    crate::test_helpers::process_death_point("account-adoption-stage-open");
    // One bounded backup step returns Busy/Locked instead of waiting
    // indefinitely. SQLite copies a consistent snapshot, including WAL.
    match rusqlite::backup::Backup::new(&input, &mut output)?.step(-1)? {
        rusqlite::backup::StepResult::Done => {}
        _ => return Err(StateError::AccountMigrationBusy),
    }
    #[cfg(all(test, target_os = "linux"))]
    crate::test_helpers::process_death_point("account-adoption-copied");
    let after: i64 = input.pragma_query_value(None, "data_version", |row| row.get(0))?;
    if before != after {
        return Err(StateError::AccountMigrationBusy);
    }
    output.pragma_update(None, "journal_mode", "DELETE")?;
    output.pragma_update(None, "synchronous", "FULL")?;
    bind(&output, owner)?;
    schema::migrate(&output)?;
    validate(&output, owner)?;
    let integrity: String = output.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        return Err(StateError::AccountMigrationInvalid);
    }
    drop(output);
    // Windows FlushFileBuffers requires write access. This private stage is
    // opened without creation or truncation; the legacy source stays read-only.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(stage)
        .map_err(|source| StateError::TempPath {
            path: stage.to_path_buf(),
            source,
        })?;
    file.sync_all().map_err(|source| StateError::TempPath {
        path: stage.to_path_buf(),
        source,
    })?;
    #[cfg(all(test, target_os = "linux"))]
    crate::test_helpers::process_death_point("account-adoption-stage-synced");
    Ok(())
}

#[cfg(test)]
mod tests;

#[cfg(all(test, target_os = "linux"))]
mod interruption_tests;
