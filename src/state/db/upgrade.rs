//! Preserve a consistent pre-v27 database snapshot before each owned upgrade attempt.
use crate::state::{error::StateError, schema};
use rusqlite::Connection;
use std::path::Path;

pub(super) fn preserve_before_v027(conn: &Connection, path: &Path) -> Result<(), StateError> {
    let version = schema::get_schema_version(conn)?;
    if version == 0 || version >= 37 {
        return Ok(());
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let identifier = uuid::Uuid::new_v4();
    let stage = parent.join(format!(".kei-upgrade-stage-{identifier}.sqlite"));
    let saved = parent.join(format!(".kei-pre-v027-{identifier}.sqlite"));
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(&stage)
        .map_err(|source| StateError::TempPath {
            path: stage.clone(),
            source,
        })?;
    let result = (|| {
        let mut output = Connection::open(&stage)?;
        match rusqlite::backup::Backup::new(conn, &mut output)?.step(-1)? {
            rusqlite::backup::StepResult::Done => {}
            _ => return Err(StateError::AccountMigrationBusy),
        }
        // SQLite includes committed WAL data without moving the original.
        output.pragma_update(None, "journal_mode", "DELETE")?;
        let integrity: String = output.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
        if integrity != "ok" || schema::get_schema_version(&output)? != version {
            return Err(StateError::AccountMigrationInvalid);
        }
        drop(output);
        file.sync_all().map_err(|source| StateError::TempPath {
            path: stage.clone(),
            source,
        })?;
        // Publish without replacement only after the private snapshot is valid
        // and durable. A partial stage is never advertised as a rollback file.
        std::fs::hard_link(&stage, &saved).map_err(|source| StateError::TempPath {
            path: saved.clone(),
            source,
        })?;
        crate::fs_util::fsync_parent_dir(&saved).map_err(|source| StateError::TempPath {
            path: saved.clone(),
            source,
        })?;
        #[cfg(all(test, target_os = "linux"))]
        crate::test_helpers::process_death_point("v027-upgrade-snapshot");
        tracing::info!(path = %saved.display(), "Preserved database rollback snapshot before v0.27 upgrade");
        Ok(())
    })();
    drop(file);
    // Only remove the private stage created by this invocation. Published
    // snapshots survive both successful upgrades and uncertain interruptions.
    let _ = std::fs::remove_file(&stage);
    result
}

#[cfg(test)]
mod tests {
    use super::preserve_before_v027;
    use crate::state::schema;
    use rusqlite::Connection;

    #[test]
    fn v027_upgrade_snapshot_preserves_wal_work_and_original_schema() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("state.sqlite");
        let conn = Connection::open(&path).unwrap();
        schema::migrate(&conn).unwrap();
        conn.execute_batch(
            "DROP INDEX idx_receipt_preserved_path;
            DROP TABLE download_receipts;
        ALTER TABLE provider_active_decisions DROP COLUMN compacted_hash;
        ALTER TABLE provider_active_destinations DROP COLUMN frozen_metadata;
            ALTER TABLE provider_active_destinations DROP COLUMN frozen_metadata_hash;
            ALTER TABLE owned_temp_files DROP COLUMN publication;
            ALTER TABLE asset_metadata_paths DROP COLUMN preserved_path_key;",
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 36).unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        conn.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
        conn.execute(
            "INSERT INTO metadata(key,value) VALUES('sync_token:PrimarySync','old-token')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO metadata(key,value) VALUES('unfinished-test-work','retained')",
            [],
        )
        .unwrap();
        preserve_before_v027(&conn, &path).unwrap();
        let snapshots: Vec<_> = std::fs::read_dir(root.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".kei-pre-v027-")
            })
            .collect();
        assert_eq!(snapshots.len(), 1);
        let saved = Connection::open(&snapshots[0]).unwrap();
        assert_eq!(schema::get_schema_version(&saved).unwrap(), 36);
        assert_eq!(
            saved
                .query_row(
                    "SELECT value FROM metadata WHERE key='unfinished-test-work'",
                    [],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
            "retained"
        );
        schema::migrate(&conn).unwrap();
        conn.execute(
            "UPDATE metadata SET value='new-token' WHERE key='sync_token:PrimarySync'",
            [],
        )
        .unwrap();
        assert_eq!(
            saved
                .query_row(
                    "SELECT value FROM metadata WHERE key='sync_token:PrimarySync'",
                    [],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
            "old-token"
        );
        assert_eq!(schema::get_schema_version(&saved).unwrap(), 36);
        preserve_before_v027(&conn, &path).unwrap();
        assert_eq!(
            std::fs::read_dir(root.path())
                .unwrap()
                .filter(|entry| entry
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".kei-pre-v027-"))
                .count(),
            1
        );
    }
}

#[cfg(all(test, target_os = "linux"))]
mod process_death;
