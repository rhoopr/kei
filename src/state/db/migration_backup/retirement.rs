//! The migration commit authorizes retirement of one specific temporary bundle.
//! No directory scan can discover or grant that authority on a later open.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};

use crate::config::ConfigSource;
use crate::fs_util::{ConfinedParents, ConfinedPath, FileIdentity, RemovalKind, file_identity};
use crate::state::{db::account, error::StateError, schema};

const KEY: &str = "schema_upgrade_temporary_bundle_v1";
pub(in crate::state::db) const FIRST_SCHEMA: i32 = 45;
pub(super) const PREFIX: &str = ".kei-schema-backup-tmp-";
// Indices are part of the v1 receipt format. The first unlink invalidates
// the completion manifest before any recovery payload is removed.
const MEMBERS: [&str; 5] = [
    "manifest.json",
    "manifest.json.part",
    "config.toml",
    "restore.txt",
    "state.db",
];

#[derive(Serialize, Deserialize, PartialEq)]
struct Member {
    identity: FileIdentity,
    length: u64,
    modified: SystemTime,
}

impl Member {
    fn inspect(file: &std::fs::File) -> std::io::Result<Self> {
        let metadata = file.metadata()?;
        Ok(Self {
            identity: file_identity(file)?,
            length: metadata.len(),
            modified: metadata.modified()?,
        })
    }
}

#[derive(Serialize, Deserialize)]
pub(in crate::state::db) struct Receipt {
    uuid: String,
    target: i32,
    live: FileIdentity,
    directory: FileIdentity,
    members: [Option<Member>; MEMBERS.len()],
}

fn confined(root: &Path, path: &Path) -> std::io::Result<ConfinedPath> {
    ConfinedPath::open(root, path, ConfinedParents::Existing)
}

fn live_identity(path: &Path) -> anyhow::Result<FileIdentity> {
    // Follow a configured symlink to its actual SQLite file. On Unix, use
    // stat: closing a second file descriptor could release SQLite's POSIX locks.
    let path = path.canonicalize()?;
    Ok(confined(path.parent().context("database has no parent")?, &path)?.entry_identity()?)
}

impl Receipt {
    pub(super) fn capture(
        path: &Path,
        bundle: &Path,
        config: &ConfigSource,
        directory: FileIdentity,
    ) -> anyhow::Result<Self> {
        let bundle = std::path::absolute(bundle)?;
        let root = bundle.parent().context("bundle has no parent")?;
        let uuid = bundle
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|s| s.strip_prefix(PREFIX))
            .context("invalid temporary bundle name")?
            .to_owned();
        ensure!(
            file_identity(&confined(root, &bundle)?.open_directory()?)? == directory,
            "published backup directory changed"
        );
        let mut members = [const { None }; MEMBERS.len()];
        for (member, name) in members.iter_mut().zip(MEMBERS) {
            if (name == "config.toml" && !matches!(config, ConfigSource::File { .. }))
                || (name == "manifest.json.part" && !cfg!(unix))
            {
                continue;
            }
            *member = Some(Member::inspect(
                &confined(root, &bundle.join(name))?.open_regular()?,
            )?);
        }
        Ok(Self {
            uuid,
            live: live_identity(path)?,
            directory,
            members,
            target: schema::SCHEMA_VERSION,
        })
    }

    pub(in crate::state::db) fn record(&self, conn: &Connection) -> Result<(), StateError> {
        let value = serde_json::to_string(self).map_err(|error| StateError::Invariant {
            operation: "recording temporary upgrade backup",
            detail: error.to_string(),
        })?;
        conn.execute(
            "INSERT INTO metadata(key,value) VALUES(?1,?2)",
            [KEY, &value],
        )?;
        if pending(conn)?.as_ref() != Some(&value) {
            return Err(StateError::Invariant {
                operation: "recording temporary upgrade backup",
                detail: "migration receipt was not stored".into(),
            });
        }
        Ok(())
    }

    fn bundle(&self, path: &Path) -> anyhow::Result<PathBuf> {
        let id = uuid::Uuid::parse_str(&self.uuid)?;
        ensure!(
            id.to_string() == self.uuid,
            "invalid temporary bundle identifier"
        );
        let root = path.parent().context("database has no parent")?;
        #[cfg(unix)]
        let root = root.canonicalize()?;
        Ok(root.join(format!("{PREFIX}{id}")))
    }

    fn verify(&self, path: &Path, owner: &account::AccountOwner, raw: &str) -> anyhow::Result<()> {
        // An independent reader must see the committed state and receipt.
        // The caller's immediate transaction excludes concurrent mutations.
        let read = account::open_read_only(path)?;
        account::validate(&read, owner)?;
        ensure!(
            pending(&read)?.as_deref() == Some(raw),
            "reopened a different database"
        );
        ensure!(
            schema::get_schema_version(&read)? == self.target,
            "wrong committed schema version"
        );
        let check: String = read.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
        ensure!(check == "ok", "SQLite integrity check failed");
        ensure!(
            read.prepare("PRAGMA foreign_key_check")?
                .query([])?
                .next()?
                .is_none(),
            "SQLite foreign key check failed"
        );
        Ok(())
    }

    fn remove(&self, path: &Path, bundle: &Path) -> anyhow::Result<()> {
        // A copied DB does not own the original's bundle, even alongside it.
        ensure!(
            live_identity(path)? == self.live,
            "live database identity changed"
        );
        let root = bundle.parent().context("bundle has no parent")?;
        let dir = confined(root, bundle)?;
        if !dir.entry_exists()? {
            dir.sync_parent()?;
            return Ok(());
        }
        let opened = dir.open_directory()?;
        ensure!(
            file_identity(&opened)? == self.directory,
            "temporary directory was replaced"
        );
        let mut entries = Vec::new();
        // Only expected names with the identities captured at publication
        // qualify. Missing members are normal after an interrupted unlink.
        for entry in bundle.read_dir()? {
            let name = entry?.file_name();
            let i = MEMBERS
                .iter()
                .position(|s| name.as_os_str() == std::ffi::OsStr::new(s))
                .context("unexpected file in temporary bundle")?;
            let member = self
                .members
                .get(i)
                .and_then(Option::as_ref)
                .context("unowned file in temporary bundle")?;
            let leaf = confined(root, &bundle.join(&name))?;
            ensure!(
                Member::inspect(&leaf.open_regular()?)? == *member,
                "temporary bundle member changed"
            );
            entries.push((i, leaf, member.identity));
        }
        entries.sort_by_key(|(i, _, _)| *i);
        for (i, leaf, identity) in entries {
            ensure!(
                dir.entry_identity()? == self.directory,
                "temporary directory changed"
            );
            leaf.remove_verified(identity, RemovalKind::File)?;
            if i == 0 {
                leaf.sync_parent()?;
                #[cfg(all(test, target_os = "linux"))]
                crate::test_helpers::process_death_point("schema-backup-retiring");
            }
        }
        drop(opened);
        dir.remove_verified(self.directory, RemovalKind::EmptyDirectory)?;
        dir.sync_parent()?;
        Ok(())
    }
}

fn pending(conn: &Connection) -> rusqlite::Result<Option<String>> {
    conn.query_row("SELECT value FROM metadata WHERE key=?1", [KEY], |r| {
        r.get(0)
    })
    .optional()
}

/// True if the database has no outstanding temporary bundle.
pub(in crate::state::db) fn finish(
    conn: &mut Connection,
    path: &Path,
    owner: &account::AccountOwner,
) -> Result<bool, StateError> {
    if pending(conn)?.is_none() {
        return Ok(true);
    }
    // Once the snapshot is gone, even interruption of the final receipt
    // write must allow a read-only ownership check. A hot DELETE journal
    // can require a write before SQLite can read the account header.
    let mode: String = conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get(0))?;
    if mode != "wal" {
        return Err(StateError::Invariant {
            operation: "preparing temporary backup retirement",
            detail: "WAL recovery is unavailable; retaining the backup".into(),
        });
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let Some(raw) = pending(&tx)? else {
        return Ok(true);
    };
    let invalid = || StateError::Invariant {
        operation: "verifying temporary upgrade backup",
        detail: "invalid backup receipt".into(),
    };
    let receipt: Receipt = serde_json::from_str(&raw).map_err(|_invalid_value| invalid())?;
    let path = std::path::absolute(path).map_err(|source| StateError::TempPath {
        path: path.to_owned(),
        source,
    })?;
    let bundle = receipt
        .bundle(&path)
        .map_err(|_invalid_location| invalid())?;
    receipt
        .verify(&path, owner, &raw)
        .map_err(|source| StateError::UpgradeVerification {
            backup: bundle.clone(),
            source,
        })?;
    // Prove the receipt can be finalized before unlinking anything. Its
    // deletion stays uncommitted until the filesystem cleanup is complete.
    if tx.execute("DELETE FROM metadata WHERE key=?1", [KEY])? != 1 || pending(&tx)?.is_some() {
        return Err(StateError::Invariant {
            operation: "completing temporary backup retirement",
            detail: "could not clear the verified upgrade receipt".into(),
        });
    }
    if let Err(error) = receipt.remove(&path, &bundle) {
        tx.rollback()?;
        tracing::warn!(%error, "Verified upgrade; temporary backup cleanup deferred");
        return Ok(false);
    }
    #[cfg(all(test, target_os = "linux"))]
    crate::test_helpers::process_death_point("schema-backup-removed");
    tx.commit()?;
    tracing::info!("Verified upgraded state and removed temporary recovery bundle");
    Ok(true)
}
