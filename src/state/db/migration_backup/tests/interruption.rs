//! Actual process termination at the backup and migration transaction boundary.

use std::os::unix::process::ExitStatusExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use super::{assert_history, bundles, config, old_database, owner};
use crate::state::{SqliteStateDb, db::account, error::StateError, schema};

const WORKER: &str =
    "state::db::migration_backup::tests::interruption::owned_migration_backup_interruption_worker";
const POINTS: [&str; 6] = [
    "schema-backup-created",
    "schema-backup-copied",
    "schema-backup-staged",
    "schema-backup-published",
    "schema-migration-before-commit",
    "schema-migration-committed",
];

struct Worker(Child);
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn owned_migration_backup_interruption_worker() {
    let Some(root) = std::env::var_os("KEI_TEST_SCHEMA_BACKUP_WORKER") else {
        return;
    };
    let root = std::path::PathBuf::from(root);
    let source = crate::config::load_toml_config_with_source(&root.join("custom.toml"), true)
        .unwrap()
        .1;
    drop(
        SqliteStateDb::open_owned_with_config(&root.join("state.db"), &owner(), &source)
            .await
            .unwrap(),
    );
    panic!("production interruption point was not reached");
}

#[tokio::test]
async fn owned_migration_backup_process_death_recovers_and_excludes_competing_writers() {
    for point in POINTS {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let conn = old_database(root, 35);
        let source = config(root);
        let config_bytes = std::fs::read(root.join("custom.toml")).unwrap();
        std::fs::write(root.join("fixture-owner"), b"kei-synthetic-process-death").unwrap();
        let mut child = Worker(
            Command::new(std::env::current_exe().unwrap())
                .args([WORKER, "--exact", "--nocapture"])
                .env("KEI_TEST_SCHEMA_BACKUP_WORKER", root)
                .env("KEI_TEST_PROCESS_DEATH_POINT", point)
                .env("KEI_TEST_PROCESS_DEATH_ROOT", root)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(30);
        while !root.join("ready").exists() {
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "{point}: worker exited before reaching boundary"
            );
            assert!(
                Instant::now() < deadline,
                "{point}: worker did not reach boundary"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(std::fs::read_to_string(root.join("ready")).unwrap(), point);
        let committed = point == "schema-migration-committed";
        assert_history(&conn);
        assert_eq!(
            schema::get_schema_version(&conn).unwrap(),
            if committed {
                schema::SCHEMA_VERSION
            } else {
                35
            }
        );
        conn.busy_timeout(Duration::ZERO).unwrap();
        if committed {
            conn.execute_batch("BEGIN IMMEDIATE; ROLLBACK;").unwrap();
        } else {
            let error = conn.execute_batch("BEGIN IMMEDIATE;").unwrap_err();
            assert_eq!(
                error.sqlite_error_code(),
                Some(rusqlite::ErrorCode::DatabaseBusy),
                "{point}: competing writers must remain excluded until commit"
            );
        }
        child.0.kill().unwrap();
        assert_eq!(child.0.wait().unwrap().signal(), Some(9));
        drop(conn);
        let inspected = account::open_read_only(&root.join("state.db")).unwrap();
        assert_history(&inspected);
        assert_eq!(
            schema::get_schema_version(&inspected).unwrap(),
            if committed {
                schema::SCHEMA_VERSION
            } else {
                35
            }
        );
        drop(inspected);
        let prior = bundles(root);
        assert_eq!(prior.len(), 1);
        let complete = matches!(
            point,
            "schema-backup-published"
                | "schema-migration-before-commit"
                | "schema-migration-committed"
        );
        assert_eq!(
            prior[0].join("manifest.json").exists(),
            complete,
            "{point}: partial stages are not complete"
        );
        let prior_database = std::fs::read(prior[0].join("state.db")).ok();
        for _ in 0..3 {
            let reopened =
                SqliteStateDb::open_owned_with_config(&root.join("state.db"), &owner(), &source)
                    .await
                    .unwrap();
            assert_history(&reopened.acquire_lock("recovered migration").unwrap());
        }
        assert_eq!(bundles(root).len(), if committed { 1 } else { 2 });
        assert_eq!(
            std::fs::read(prior[0].join("state.db")).ok(),
            prior_database,
            "{point}: interrupted evidence must not be reused or overwritten"
        );
        for bundle in bundles(root)
            .iter()
            .filter(|bundle| bundle.join("manifest.json").exists())
        {
            let backup = account::open_read_only(&bundle.join("state.db")).unwrap();
            assert_history(&backup);
            assert_eq!(schema::get_schema_version(&backup).unwrap(), 35);
            assert_eq!(
                std::fs::read(bundle.join("config.toml")).unwrap(),
                config_bytes
            );
        }
        assert_eq!(
            std::fs::read(root.join("custom.toml")).unwrap(),
            config_bytes
        );
        assert_eq!(std::fs::read(root.join("kept.jpg")).unwrap(), b"kept media");
        assert_eq!(
            std::fs::read(root.join("private.xmp")).unwrap(),
            b"unrelated custom metadata"
        );
    }
}

#[tokio::test]
async fn owned_migration_backup_delete_spill_process_death_refuses_then_restores() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let path = root.join("state.db");
    let conn = old_database(root, 35);
    conn.pragma_update(None, "journal_mode", "DELETE").unwrap();
    // Persist a one-page cache for the production opener. Real migration DDL
    // must spill to the database, not merely leave dirty pages in memory.
    conn.pragma_update(None, "default_cache_size", 1).unwrap();
    drop(conn);
    let before = std::fs::read(&path).unwrap();
    let source = config(root);
    std::fs::write(root.join("fixture-owner"), b"kei-synthetic-process-death").unwrap();
    let point = "schema-migration-before-commit";
    let log = std::fs::File::create(root.join("worker.log")).unwrap();
    let mut child = Worker(
        Command::new(std::env::current_exe().unwrap())
            .args([WORKER, "--exact", "--nocapture"])
            .env("KEI_TEST_SCHEMA_BACKUP_WORKER", root)
            .env("KEI_TEST_PROCESS_DEATH_POINT", point)
            .env("KEI_TEST_PROCESS_DEATH_ROOT", root)
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    while !root.join("ready").exists() {
        assert!(
            child.0.try_wait().unwrap().is_none() && Instant::now() < deadline,
            "worker failed to reach migration boundary: {}",
            std::fs::read_to_string(root.join("worker.log")).unwrap()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(std::fs::read_to_string(root.join("ready")).unwrap(), point);
    assert_ne!(std::fs::read(&path).unwrap(), before, "no pages spilled");
    let journal = root.join("state.db-journal");
    assert!(
        std::fs::read(&journal)
            .unwrap()
            .starts_with(&[0xd9, 0xd5, 0x05, 0xf9, 0x20, 0xa1, 0x63, 0xd7])
    );
    child.0.kill().unwrap();
    assert_eq!(child.0.wait().unwrap().signal(), Some(9));

    let saved = bundles(root);
    assert_eq!(saved.len(), 1);
    let bundle = &saved[0];
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(bundle.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["schema_version"], 35);
    for (file, expected) in [
        ("state.db", &manifest["database_sha256"]),
        ("config.toml", &manifest["config"]["sha256"]),
    ] {
        assert_eq!(super::super::digest(&bundle.join(file)).unwrap(), *expected);
    }
    let evidence: Vec<_> = [
        path.clone(),
        journal.clone(),
        root.join("custom.toml"),
        root.join("kept.jpg"),
        root.join("private.xmp"),
    ]
    .into_iter()
    .chain(
        std::fs::read_dir(bundle)
            .unwrap()
            .map(|entry| entry.unwrap().path()),
    )
    .map(|path| {
        let bytes = std::fs::read(&path).unwrap();
        (path, bytes)
    })
    .collect();
    let inspection = account::open_read_only(&path).unwrap();
    let raw = inspection
        .query_row::<i64, _, _>("SELECT count(*) FROM account_owner", [], |row| row.get(0))
        .unwrap_err();
    assert_eq!(
        raw.sqlite_error().unwrap().extended_code,
        rusqlite::ffi::SQLITE_READONLY_ROLLBACK
    );
    drop(inspection);
    let wrong = account::AccountOwner::authenticated(
        "foreign@example.invalid",
        "com",
        &serde_json::from_value(serde_json::json!({"dsInfo":{"dsid":"foreign"}})).unwrap(),
    )
    .unwrap();
    for _ in 0..2 {
        for expected in [&owner(), &wrong] {
            for result in [
                SqliteStateDb::open_owned_with_config(&path, expected, &source).await,
                SqliteStateDb::open_owned_read_only(&path, expected).await,
            ] {
                let error = result.unwrap_err();
                assert!(matches!(
                    error,
                    StateError::AccountRollbackRecoveryRequired(_)
                ));
                assert!(error.to_string().contains("rollback recovery"), "{error}");
                assert!(error.to_string().contains("restore.txt"), "{error}");
            }
            for (path, bytes) in &evidence {
                assert_eq!(
                    std::fs::read(path).unwrap(),
                    *bytes,
                    "{} changed on refusal",
                    path.display()
                );
            }
            assert_eq!(bundles(root), saved);
            assert!(!root.join("state.db-wal").exists());
            assert!(!root.join("state.db-shm").exists());
        }
    }

    // Follow restore.txt: validate the standalone snapshot first, preserve the
    // interrupted DB with its journal, then install only the verified snapshot.
    let backup = account::open_read_only(&bundle.join("state.db")).unwrap();
    assert_history(&backup);
    assert_eq!(schema::get_schema_version(&backup).unwrap(), 35);
    assert!(matches!(
        account::validate(&backup, &wrong),
        Err(StateError::AccountOwnerMismatch)
    ));
    assert_eq!(
        backup
            .query_row::<String, _, _>("PRAGMA integrity_check", [], |row| row.get(0))
            .unwrap(),
        "ok"
    );
    drop(backup);
    let restore = std::fs::read_to_string(bundle.join("restore.txt")).unwrap();
    let preserved = root.join("interrupted-evidence");
    std::fs::create_dir(&preserved).unwrap();
    for file in ["state.db", "state.db-journal", "custom.toml"] {
        std::fs::rename(root.join(file), preserved.join(file)).unwrap();
    }
    std::fs::copy(bundle.join("state.db"), &path).unwrap();
    std::fs::copy(bundle.join("config.toml"), root.join("custom.toml")).unwrap();
    let source = crate::config::load_toml_config_with_source(&root.join("custom.toml"), true)
        .unwrap()
        .1;
    let restored_bytes = std::fs::read(&path).unwrap();
    assert!(matches!(
        SqliteStateDb::open_owned_with_config(&path, &wrong, &source).await,
        Err(StateError::AccountOwnerMismatch)
    ));
    assert_eq!(std::fs::read(&path).unwrap(), restored_bytes);
    for _ in 0..3 {
        let reopened = SqliteStateDb::open_owned_with_config(&path, &owner(), &source)
            .await
            .unwrap();
        let conn = reopened
            .acquire_lock("restored and quiet migration")
            .unwrap();
        assert_history(&conn);
        assert_eq!(
            schema::get_schema_version(&conn).unwrap(),
            schema::SCHEMA_VERSION
        );
    }
    assert_eq!(bundles(root).len(), 2);
    for (path, bytes) in evidence {
        let path =
            if path == root.join("state.db") || path == journal || path == root.join("custom.toml")
            {
                preserved.join(path.file_name().unwrap())
            } else {
                path
            };
        assert_eq!(
            std::fs::read(&path).unwrap(),
            bytes,
            "{} changed",
            path.display()
        );
    }
    assert!(restore.contains("-journal"), "{restore}");
}
