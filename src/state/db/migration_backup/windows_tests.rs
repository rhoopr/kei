//! Native Windows execution is required; these tests are not Linux emulation.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use rusqlite::Connection;

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
        return Err(std::io::Error::other(
            "injected Windows backup storage failure",
        ));
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
        "windows-backup@example.invalid",
        "com",
        &serde_json::from_value(serde_json::json!({"dsInfo":{"dsid":"synthetic-windows-backup"}}))
            .unwrap(),
    )
    .unwrap()
}

fn seed(root: &Path) -> Connection {
    let (conn, fresh) = account::open(&root.join("state.db"), &owner()).unwrap();
    assert!(fresh);
    for version in 1..=35 {
        schema::migrate_to_version(&conn, 0, version).unwrap();
    }
    conn.pragma_update(None, "journal_mode", "WAL").unwrap();
    conn.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
    conn.execute_batch("CREATE TABLE future_unknown(payload BLOB); INSERT INTO future_unknown VALUES (X'00FF07');
        INSERT INTO metadata VALUES ('sync_token:PrimarySync','original-cursor'),('historical-receipt','exact historical bytes');
        INSERT INTO assets(library,id,version_size,checksum,filename,created_at,size_bytes,media_type,last_seen_at,status)
        VALUES ('PrimarySync','retry','original','provider-retry','retry.jpg',1000,12,'photo',1000,'pending');").unwrap();
    std::fs::write(root.join("kept.jpg"), b"existing media").unwrap();
    std::fs::write(
        root.join("custom.toml"),
        b"# preserve exact CRLF\r\n[filters]\r\nskip_created_before = '2020-01-01'\r\n",
    )
    .unwrap();
    conn
}

fn assert_history(conn: &Connection) {
    assert_eq!(
        conn.query_row::<Vec<u8>, _, _>("SELECT payload FROM future_unknown", [], |row| row.get(0))
            .unwrap(),
        [0, 255, 7]
    );
    for (key, expected) in [
        ("sync_token:PrimarySync", "original-cursor"),
        ("historical-receipt", "exact historical bytes"),
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
        conn.query_row::<String, _, _>("SELECT status FROM assets WHERE id='retry'", [], |row| row
            .get(0))
            .unwrap(),
        "pending"
    );
    account::validate(conn, &owner()).unwrap();
}

enum BundleState {
    Complete,
    Partial,
}
use BundleState::{Complete, Partial};

fn bundles(root: &Path, state: BundleState) -> Vec<PathBuf> {
    std::fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(".kei-schema-backup-")
                && path
                    .extension()
                    .is_some_and(|extension| extension == "partial")
                    == matches!(state, Partial)
        })
        .collect()
}

#[tokio::test]
async fn owned_migration_backup_windows_wal_config_restore_and_quiet_reopens() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let source = seed(root);
    let context = load_toml_config_with_source(&root.join("custom.toml"), true)
        .unwrap()
        .1;
    let config = std::fs::read(root.join("custom.toml")).unwrap();
    assert!(std::fs::metadata(root.join("state.db-wal")).unwrap().len() > 0);
    drop(
        SqliteStateDb::open_owned_with_config(&root.join("state.db"), &owner(), &context)
            .await
            .unwrap(),
    );
    let complete = bundles(root, Complete);
    assert_eq!(complete.len(), 1);
    assert!(bundles(root, Partial).is_empty());
    let bundle = &complete[0];
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(bundle.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["native_path_encoding"], "windows_utf16le_hex");
    assert_eq!(manifest["schema_version"], 35);
    assert_eq!(
        manifest["database_sha256"],
        super::digest(&bundle.join("state.db")).unwrap()
    );
    assert_eq!(std::fs::read(bundle.join("config.toml")).unwrap(), config);
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
    load_toml_config_with_source(&restore.path().join("restored.toml"), true).unwrap();
    drop(source);
    for _ in 0..2 {
        let reopened = SqliteStateDb::open_owned_with_config(
            &root.join("state.db"),
            &owner(),
            &ConfigSource::Unavailable,
        )
        .await
        .unwrap();
        assert_history(&reopened.acquire_lock("quiet Windows reopen").unwrap());
        assert_eq!(bundles(root, Complete).len(), 1);
    }
    assert_eq!(
        std::fs::read(root.join("kept.jpg")).unwrap(),
        b"existing media"
    );
}

#[tokio::test]
async fn owned_migration_backup_windows_storage_failure_retains_partial_then_recovers() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let conn = seed(root);
    let context = load_toml_config_with_source(&root.join("missing.toml"), false)
        .unwrap()
        .1;
    *IO_FAULT.lock().unwrap() = Some(root.join("state.db"));
    let fault = IoFault;
    let result =
        SqliteStateDb::open_owned_with_config(&root.join("state.db"), &owner(), &context).await;
    drop(fault);
    assert!(
        matches!(result, Err(StateError::MigrationBackup { .. })),
        "{result:?}"
    );
    assert_eq!(schema::get_schema_version(&conn).unwrap(), 35);
    assert_history(&conn);
    assert!(bundles(root, Complete).is_empty());
    let partial = bundles(root, Partial);
    assert_eq!(partial.len(), 1);
    assert!(!partial[0].join("manifest.json").exists());
    let bytes = std::fs::read(partial[0].join("state.db")).unwrap();
    for _ in 0..3 {
        drop(
            SqliteStateDb::open_owned_with_config(&root.join("state.db"), &owner(), &context)
                .await
                .unwrap(),
        );
    }
    assert_eq!(bundles(root, Complete).len(), 1);
    assert_eq!(std::fs::read(partial[0].join("state.db")).unwrap(), bytes);
    let manifest: serde_json::Value = serde_json::from_slice(
        &std::fs::read(bundles(root, Complete)[0].join("manifest.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(manifest["config"]["kind"], "absent");
}

#[test]
fn owned_migration_backup_windows_manifest_paths_round_trip_native_units() {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    let mut units: Vec<u16> = r"C:\backup\".encode_utf16().collect();
    units.push(0xd800);
    let path = PathBuf::from(std::ffi::OsString::from_wide(&units));
    let bytes = data_encoding::HEXLOWER
        .decode(super::native_path(&path).as_bytes())
        .unwrap();
    let decoded: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    assert_eq!(
        std::ffi::OsString::from_wide(&decoded)
            .encode_wide()
            .collect::<Vec<_>>(),
        units
    );
}

#[tokio::test]
async fn owned_migration_backup_windows_stage_substitution_refuses_migration_then_recovers() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let conn = seed(root);
    let context = load_toml_config_with_source(&root.join("custom.toml"), true)
        .unwrap()
        .1;
    let original_db = std::fs::read(root.join("state.db")).unwrap();
    let original_wal = std::fs::read(root.join("state.db-wal")).unwrap();
    let config = std::fs::read(root.join("custom.toml")).unwrap();
    let substitution = super::windows::tests::substitute_after_handoff(root);
    let result =
        SqliteStateDb::open_owned_with_config(&root.join("state.db"), &owner(), &context).await;
    assert!(
        substitution.fired(),
        "require the real stage directory replacement"
    );
    let Err(StateError::MigrationBackup { source, .. }) = result else {
        panic!("stage substitution must fail at the shared migration backup boundary");
    };
    assert!(
        source.to_string().contains("original populated stage"),
        "{source}"
    );
    assert_eq!(schema::get_schema_version(&conn).unwrap(), 35);
    assert_history(&conn);
    assert_eq!(std::fs::read(root.join("state.db")).unwrap(), original_db);
    assert_eq!(
        std::fs::read(root.join("state.db-wal")).unwrap(),
        original_wal
    );
    let preserved = root.join("original-stage.partial");
    let backup = account::open_read_only(&preserved.join("state.db")).unwrap();
    assert_history(&backup);
    assert_eq!(schema::get_schema_version(&backup).unwrap(), 35);
    assert_eq!(
        std::fs::read(preserved.join("config.toml")).unwrap(),
        config
    );
    drop(backup);
    let preserved_bytes = std::fs::read(preserved.join("state.db")).unwrap();
    let replaced = bundles(root, Complete);
    assert_eq!(replaced.len(), 1);
    assert_eq!(
        std::fs::read(replaced[0].join("foreign-evidence")).unwrap(),
        b"substituted directory bytes"
    );
    drop(substitution);
    drop(conn);
    for _ in 0..3 {
        let reopened =
            SqliteStateDb::open_owned_with_config(&root.join("state.db"), &owner(), &context)
                .await
                .unwrap();
        let conn = reopened
            .acquire_lock("recovered Windows stage substitution")
            .unwrap();
        assert_history(&conn);
        assert_eq!(
            schema::get_schema_version(&conn).unwrap(),
            schema::SCHEMA_VERSION
        );
    }
    assert_eq!(
        bundles(root, Complete).len(),
        2,
        "one fresh real backup, no repeated quiet backups"
    );
    assert_eq!(
        std::fs::read(preserved.join("state.db")).unwrap(),
        preserved_bytes
    );
    assert_eq!(
        std::fs::read(replaced[0].join("foreign-evidence")).unwrap(),
        b"substituted directory bytes"
    );
    assert_eq!(std::fs::read(root.join("custom.toml")).unwrap(), config);
    assert_eq!(
        std::fs::read(root.join("kept.jpg")).unwrap(),
        b"existing media"
    );
}

pub(super) fn interruption_point(point: &str) {
    if std::env::var("KEI_TEST_WINDOWS_BACKUP_POINT").as_deref() != Ok(point) {
        return;
    }
    let root = PathBuf::from(std::env::var_os("KEI_TEST_WINDOWS_BACKUP_ROOT").unwrap());
    assert_eq!(
        std::fs::read(root.join("fixture-owner")).unwrap(),
        b"synthetic-windows-backup"
    );
    std::fs::write(root.join("ready.part"), point).unwrap();
    std::fs::rename(root.join("ready.part"), root.join("ready")).unwrap();
    loop {
        std::thread::park();
    }
}

#[tokio::test]
async fn owned_migration_backup_windows_interruption_worker() {
    let Some(root) = std::env::var_os("KEI_TEST_WINDOWS_BACKUP_ROOT") else {
        return;
    };
    let root = PathBuf::from(root);
    let context = load_toml_config_with_source(&root.join("custom.toml"), true)
        .unwrap()
        .1;
    drop(
        SqliteStateDb::open_owned_with_config(&root.join("state.db"), &owner(), &context)
            .await
            .unwrap(),
    );
    panic!("Windows interruption boundary was not reached");
}

struct Worker(Child);
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn owned_migration_backup_windows_process_termination_preserves_partial_and_complete_bundles()
{
    const WORKER: &str = "state::db::migration_backup::windows_tests::owned_migration_backup_windows_interruption_worker";
    for point in [
        "schema-backup-created",
        "schema-backup-copied",
        "schema-backup-staged",
        "schema-backup-published",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let conn = seed(root);
        let context = load_toml_config_with_source(&root.join("custom.toml"), true)
            .unwrap()
            .1;
        std::fs::write(root.join("fixture-owner"), b"synthetic-windows-backup").unwrap();
        let mut child = Worker(
            Command::new(std::env::current_exe().unwrap())
                .args([WORKER, "--exact", "--nocapture"])
                .env("KEI_TEST_WINDOWS_BACKUP_ROOT", root)
                .env("KEI_TEST_WINDOWS_BACKUP_POINT", point)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(30);
        while !root.join("ready").exists() {
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "{point}: worker exited before boundary"
            );
            assert!(Instant::now() < deadline, "{point}: boundary timed out");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(std::fs::read_to_string(root.join("ready")).unwrap(), point);
        conn.busy_timeout(Duration::ZERO).unwrap();
        assert_eq!(
            conn.execute_batch("BEGIN IMMEDIATE")
                .unwrap_err()
                .sqlite_error_code(),
            Some(rusqlite::ErrorCode::DatabaseBusy)
        );
        child.0.kill().unwrap(); // std uses TerminateProcess on Windows.
        assert!(!child.0.wait().unwrap().success());
        assert_history(&conn);
        assert_eq!(schema::get_schema_version(&conn).unwrap(), 35);
        drop(conn);
        let published = point == "schema-backup-published";
        assert_eq!(bundles(root, Complete).len(), usize::from(published));
        assert_eq!(bundles(root, Partial).len(), usize::from(!published));
        if point == "schema-backup-staged" {
            assert!(
                bundles(root, Partial)[0].join("manifest.json").exists(),
                "prepared manifest alone cannot complete a .partial directory"
            );
        }
        for _ in 0..3 {
            let reopened =
                SqliteStateDb::open_owned_with_config(&root.join("state.db"), &owner(), &context)
                    .await
                    .unwrap();
            assert_history(&reopened.acquire_lock("Windows restart recovery").unwrap());
        }
        assert_eq!(bundles(root, Complete).len(), 1 + usize::from(published));
        assert_eq!(
            std::fs::read(root.join("kept.jpg")).unwrap(),
            b"existing media"
        );
    }
}
