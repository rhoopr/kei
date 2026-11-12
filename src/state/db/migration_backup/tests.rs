use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use rusqlite::Connection;

use super::{publish_manifest, write_bundle};
use crate::config::{ConfigSource, load_toml_config_with_source};
use crate::state::{SqliteStateDb, db::account, error::StateError, schema};

static IO_FAULT: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

pub(super) fn backup_io_fault(path: &Path) -> std::io::Result<()> {
    if IO_FAULT
        .lock()
        .map_err(|_poisoned| std::io::Error::other("backup fault lock poisoned"))?
        .as_deref()
        == Some(path)
    {
        return Err(std::io::Error::other("injected backup storage failure"));
    }
    Ok(())
}

struct IoFault;
impl Drop for IoFault {
    fn drop(&mut self) {
        *IO_FAULT.lock().unwrap() = None;
    }
}

fn owner() -> account::AccountOwner {
    account::AccountOwner::authenticated(
        "backup@example.invalid",
        "com",
        &serde_json::from_value(serde_json::json!({"dsInfo":{"dsid":"synthetic-backup"}})).unwrap(),
    )
    .unwrap()
}

fn old_database(root: &Path, version: i32) -> Connection {
    let (conn, fresh) = account::open(&root.join("state.db"), &owner()).unwrap();
    assert!(fresh);
    for step in 1..=version {
        schema::migrate_to_version(&conn, 0, step).unwrap();
    }
    conn.pragma_update(None, "user_version", version).unwrap();
    conn.pragma_update(None, "journal_mode", "WAL").unwrap();
    conn.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
    conn.execute_batch("CREATE TABLE future_unknown(payload BLOB NOT NULL); INSERT INTO future_unknown VALUES (X'00FF07');
        INSERT INTO metadata VALUES ('sync_token:PrimarySync','original-cursor'),('pending_sync_token:old:PrimarySync','historical-debt');
        INSERT INTO assets(library,id,version_size,checksum,filename,created_at,size_bytes,media_type,last_seen_at,status) VALUES
        ('PrimarySync','kept','original','provider-original','kept.jpg',1000,10,'photo',1000,'downloaded'),
        ('PrimarySync','retry','original','provider-retry','retry.jpg',1000,12,'photo',1000,'pending');
        INSERT INTO asset_metadata_paths(library,id,version_size,local_path,provider_checksum,local_checksum) VALUES
        ('PrimarySync','kept','original','kept.jpg','provider-original','historical-receipt-bytes');").unwrap();
    std::fs::write(root.join("kept.jpg"), b"kept media").unwrap();
    std::fs::write(root.join("private.xmp"), b"unrelated custom metadata").unwrap();
    conn
}

fn assert_history(conn: &Connection) {
    for (key, expected) in [
        ("sync_token:PrimarySync", "original-cursor"),
        ("pending_sync_token:old:PrimarySync", "historical-debt"),
    ] {
        assert_eq!(
            conn.query_row::<String, _, _>(
                "SELECT value FROM metadata WHERE key=?1",
                [key],
                |row| row.get(0)
            )
            .unwrap(),
            expected
        );
    }
    assert_eq!(
        conn.query_row::<Vec<u8>, _, _>("SELECT payload FROM future_unknown", [], |row| row.get(0))
            .unwrap(),
        [0, 255, 7]
    );
    assert_eq!(
        conn.query_row::<String, _, _>("SELECT status FROM assets WHERE id='retry'", [], |row| row
            .get(0))
            .unwrap(),
        "pending"
    );
    assert_eq!(
        conn.query_row::<String, _, _>(
            "SELECT local_checksum FROM asset_metadata_paths",
            [],
            |row| row.get(0)
        )
        .unwrap(),
        "historical-receipt-bytes"
    );
    account::validate(conn, &owner()).unwrap();
}

fn bundles(root: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(".kei-schema-backup-")
        })
        .collect()
}

fn config(root: &Path) -> ConfigSource {
    let path = root.join("custom.toml");
    std::fs::write(
        &path,
        b"# retain exact comments and CRLF\r\n[filters]\r\nskip_created_before = '2020-01-01'\r\n",
    )
    .unwrap();
    load_toml_config_with_source(&path, true).unwrap().1
}

#[tokio::test]
async fn owned_migration_backup_captures_wal_and_exact_loaded_config_then_restores_independently() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let conn = old_database(root, 35);
    let source = config(root);
    let bytes = std::fs::read(root.join("custom.toml")).unwrap();
    // Back up the input actually used, not a later unrelated edit on disk.
    std::fs::write(root.join("custom.toml"), b"# changed after startup\n").unwrap();
    assert!(std::fs::metadata(root.join("state.db-wal")).unwrap().len() > 0);
    let opened = SqliteStateDb::open_owned_with_config(&root.join("state.db"), &owner(), &source)
        .await
        .unwrap();
    assert_eq!(
        schema::get_schema_version(&opened.acquire_lock("check migrated").unwrap()).unwrap(),
        schema::SCHEMA_VERSION
    );
    assert_history(&opened.acquire_lock("check history").unwrap());
    let saved = bundles(root);
    assert_eq!(saved.len(), 1);
    let bundle = &saved[0];
    assert_eq!(
        std::fs::metadata(bundle).unwrap().permissions().mode() & 0o777,
        0o700
    );
    for entry in std::fs::read_dir(bundle).unwrap() {
        assert_eq!(
            entry.unwrap().metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    assert_eq!(std::fs::read(bundle.join("config.toml")).unwrap(), bytes);
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(bundle.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["schema_version"], 35);
    assert_eq!(manifest["config"]["kind"], "file");
    assert_eq!(
        manifest["config"]["sha256"],
        super::digest(&bundle.join("config.toml")).unwrap()
    );
    assert_eq!(
        manifest["database_sha256"],
        super::digest(&bundle.join("state.db")).unwrap()
    );
    assert!(
        manifest["runtime_context"]
            .as_str()
            .unwrap()
            .contains("CLI overrides")
    );
    let restore = tempfile::tempdir().unwrap();
    std::fs::copy(bundle.join("state.db"), restore.path().join("restored.db")).unwrap();
    std::fs::copy(
        bundle.join("config.toml"),
        restore.path().join("restored.toml"),
    )
    .unwrap();
    let restored = Connection::open(restore.path().join("restored.db")).unwrap();
    assert_history(&restored);
    assert_eq!(schema::get_schema_version(&restored).unwrap(), 35);
    assert_eq!(
        restored
            .pragma_query_value::<String, _>(None, "journal_mode", |row| row.get(0))
            .unwrap(),
        "delete"
    );
    assert!(!restore.path().join("restored.db-wal").exists());
    assert!(
        load_toml_config_with_source(&restore.path().join("restored.toml"), true)
            .unwrap()
            .0
            .is_some()
    );
    let saved_bytes = std::fs::read(bundle.join("state.db")).unwrap();
    drop(opened);
    drop(conn);
    for _ in 0..2 {
        let opened = SqliteStateDb::open_owned_with_config(
            &root.join("state.db"),
            &owner(),
            &ConfigSource::Unavailable,
        )
        .await
        .unwrap();
        assert_history(&opened.acquire_lock("quiet reopen").unwrap());
        assert_eq!(bundles(root).len(), 1);
        assert_eq!(std::fs::read(bundle.join("state.db")).unwrap(), saved_bytes);
    }
    assert_eq!(std::fs::read(root.join("kept.jpg")).unwrap(), b"kept media");
    assert_eq!(
        std::fs::read(root.join("private.xmp")).unwrap(),
        b"unrelated custom metadata"
    );
}

#[tokio::test]
async fn owned_migration_backup_is_skipped_for_fresh_and_current_but_required_for_existing_zero() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("state.db");
    for _ in 0..3 {
        drop(
            SqliteStateDb::open_owned_with_config(&path, &owner(), &ConfigSource::Unavailable)
                .await
                .unwrap(),
        );
        assert!(bundles(temp.path()).is_empty());
    }
    let zero = temp.path().join("zero.db");
    drop(account::open(&zero, &owner()).unwrap());
    assert!(matches!(
        SqliteStateDb::open_owned_with_config(&zero, &owner(), &ConfigSource::Unavailable).await,
        Err(StateError::MigrationConfigUnavailable)
    ));
    assert_eq!(
        schema::get_schema_version(&account::open_read_only(&zero).unwrap()).unwrap(),
        0
    );
}

#[tokio::test]
async fn owned_migration_backup_failure_preserves_source_wal_version_and_journal_before_recovery() {
    for mode in ["WAL", "DELETE"] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let conn = old_database(root, 35);
        conn.pragma_update(None, "journal_mode", mode).unwrap();
        let source = config(root);
        let before = std::fs::read(root.join("state.db")).unwrap();
        let wal = std::fs::read(root.join("state.db-wal")).ok();
        // Path-scoped fault in the real backup owner after the SQLite copy, before
        // config/manifest publication. It works even when the test runs as root.
        *IO_FAULT.lock().unwrap() = Some(root.join("state.db"));
        let fault = IoFault;
        let result =
            SqliteStateDb::open_owned_with_config(&root.join("state.db"), &owner(), &source).await;
        drop(fault);
        assert!(
            matches!(result, Err(StateError::MigrationBackup { .. })),
            "{result:?}"
        );
        assert_eq!(std::fs::read(root.join("state.db")).unwrap(), before);
        assert_eq!(std::fs::read(root.join("state.db-wal")).ok(), wal);
        assert_eq!(schema::get_schema_version(&conn).unwrap(), 35);
        assert_eq!(
            conn.pragma_query_value::<String, _>(None, "journal_mode", |row| row.get(0))
                .unwrap(),
            mode.to_lowercase()
        );
        assert_history(&conn);
        let incomplete = bundles(root);
        assert_eq!(incomplete.len(), 1);
        assert!(!incomplete[0].join("manifest.json").exists());
        for _ in 0..3 {
            drop(
                SqliteStateDb::open_owned_with_config(&root.join("state.db"), &owner(), &source)
                    .await
                    .unwrap(),
            );
        }
        assert_eq!(bundles(root).len(), 2);
    }
}

#[tokio::test]
async fn owned_migration_backup_lock_refusal_preserves_history_then_recovers() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let conn = old_database(root, 35);
    let source = config(root);
    conn.execute_batch("BEGIN IMMEDIATE").unwrap();
    let result =
        SqliteStateDb::open_owned_with_config(&root.join("state.db"), &owner(), &source).await;
    assert!(
        matches!(result, Err(StateError::MigrationLock(_))),
        "{result:?}"
    );
    assert_eq!(schema::get_schema_version(&conn).unwrap(), 35);
    assert_history(&conn);
    assert!(bundles(root).is_empty());
    conn.execute_batch("ROLLBACK").unwrap();
    for _ in 0..3 {
        drop(
            SqliteStateDb::open_owned_with_config(&root.join("state.db"), &owner(), &source)
                .await
                .unwrap(),
        );
    }
    assert_eq!(bundles(root).len(), 1);
}

#[tokio::test]
async fn owned_migration_backup_retains_absence_and_rolls_back_all_steps_on_late_failure() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let conn = old_database(root, 34);
    conn.execute_batch("CREATE TABLE provider_transport_canonical (unknown BLOB); INSERT INTO provider_transport_canonical VALUES (X'F00D');").unwrap();
    let source = load_toml_config_with_source(&root.join("missing.toml"), false)
        .unwrap()
        .1;
    let result =
        SqliteStateDb::open_owned_with_config(&root.join("state.db"), &owner(), &source).await;
    assert!(
        matches!(result, Err(StateError::BackedUpMigration { .. })),
        "{result:?}"
    );
    assert_eq!(schema::get_schema_version(&conn).unwrap(), 34);
    assert_eq!(
        conn.query_row::<i64, _, _>(
            "SELECT count(*) FROM sqlite_schema WHERE name='primary_layout_operations'",
            [],
            |row| row.get(0)
        )
        .unwrap(),
        0,
        "earlier schema-35 DDL must roll back with the failed schema-36 step"
    );
    assert_eq!(
        conn.query_row::<Vec<u8>, _, _>(
            "SELECT unknown FROM provider_transport_canonical",
            [],
            |row| row.get(0)
        )
        .unwrap(),
        [0xf0, 0x0d]
    );
    assert_history(&conn);
    let saved = bundles(root);
    assert_eq!(saved.len(), 1);
    assert!(!saved[0].join("config.toml").exists());
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(saved[0].join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["config"]["kind"], "absent");
    let prior = std::fs::read(saved[0].join("state.db")).unwrap();
    conn.execute_batch("DROP TABLE provider_transport_canonical")
        .unwrap();
    for _ in 0..3 {
        drop(
            SqliteStateDb::open_owned_with_config(&root.join("state.db"), &owner(), &source)
                .await
                .unwrap(),
        );
    }
    assert_eq!(bundles(root).len(), 2);
    assert_eq!(std::fs::read(saved[0].join("state.db")).unwrap(), prior);
}

#[test]
fn owned_migration_backup_publication_never_overwrites_or_reuses_stages() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let _conn = old_database(root, 35);
    let source = config(root);
    let stage = root.join("stage");
    std::fs::write(&stage, b"new manifest").unwrap();
    let occupied = root.join("manifest.json");
    std::fs::write(&occupied, b"prior evidence").unwrap();
    assert!(publish_manifest(&stage, &occupied).is_err());
    assert_eq!(std::fs::read(&occupied).unwrap(), b"prior evidence");
    let bundle = root.join("occupied-bundle");
    std::fs::create_dir(&bundle).unwrap();
    std::fs::write(bundle.join("unknown"), b"preserve me").unwrap();
    assert!(write_bundle(&root.join("state.db"), &source, 35, &bundle).is_err());
    assert_eq!(
        std::fs::read(bundle.join("unknown")).unwrap(),
        b"preserve me"
    );
    let link = root.join("linked-bundle");
    std::os::unix::fs::symlink(&bundle, &link).unwrap();
    assert!(write_bundle(&root.join("state.db"), &source, 35, &link).is_err());
}

#[cfg(target_os = "linux")]
mod interruption;
