//! Upgrade SIGKILL coverage; snapshots include committed WAL and retain old schema.
use crate::state::db::account::AccountOwner;
use crate::state::{SqliteStateDb, schema};
use rusqlite::Connection;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const WORKER: &str = "state::db::upgrade::process_death::v027_upgrade_death_worker";
fn owner() -> AccountOwner {
    AccountOwner::authenticated(
        "upgrade@example.invalid",
        "com",
        &serde_json::from_value(serde_json::json!({"dsInfo":{"dsid":"synthetic-upgrade"}}))
            .unwrap(),
    )
    .unwrap()
}
fn snapshots(root: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(root)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(".kei-pre-v027-")
        })
        .collect()
}
fn retained(conn: &Connection) {
    assert_eq!(
        conn.query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
    assert_eq!(
        conn.query_row(
            "SELECT value FROM metadata WHERE key='sync_token:PrimarySync'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "old-cursor"
    );
    assert_eq!(
        conn.query_row(
            "SELECT status FROM assets WHERE id='pending-upgrade'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "pending"
    );
    assert_eq!(
        conn.query_row("SELECT payload FROM future_unknown", [], |r| r
            .get::<_, Vec<u8>>(0))
            .unwrap(),
        [0, 255, 7]
    );
}
struct Worker(Child);
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn v027_upgrade_sigkill_preserves_snapshot_and_reopens_without_losing_wal_work() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("state.sqlite");
    std::fs::write(
        root.path().join("fixture-owner"),
        b"kei-synthetic-process-death",
    )
    .unwrap();
    let media = root.path().join("kept.JPG");
    std::fs::write(&media, b"unchanged media").unwrap();
    let mtime = std::fs::metadata(&media).unwrap().modified().unwrap();
    let db = SqliteStateDb::open_owned(&path, &owner()).await.unwrap();
    db.upsert_seen(&crate::test_helpers::TestAssetRecord::new("pending-upgrade").build())
        .await
        .unwrap();
    drop(db);
    let wal = Connection::open(&path).unwrap();
    wal.execute_batch(
        "DROP INDEX idx_receipt_preserved_path;
        ALTER TABLE owned_temp_files DROP COLUMN publication;
        ALTER TABLE asset_metadata_paths DROP COLUMN preserved_path_key;
        DROP TABLE download_receipts;
        ALTER TABLE provider_active_decisions DROP COLUMN compacted_hash;
        ALTER TABLE provider_active_destinations DROP COLUMN frozen_metadata;
        ALTER TABLE provider_active_destinations DROP COLUMN frozen_metadata_hash;",
    )
    .unwrap();
    wal.pragma_update(None, "user_version", 36).unwrap();
    wal.pragma_update(None, "journal_mode", "WAL").unwrap();
    wal.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
    wal.execute_batch(
        "INSERT INTO metadata(key,value) VALUES('sync_token:PrimarySync','old-cursor');
        CREATE TABLE future_unknown(payload BLOB NOT NULL);
        INSERT INTO future_unknown VALUES(X'00FF07');",
    )
    .unwrap();
    let log = std::fs::File::create(root.path().join("worker.log")).unwrap();
    let mut child = Worker(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", WORKER, "--ignored", "--nocapture"])
            .env("KEI_TEST_PROCESS_DEATH_ROOT", root.path())
            .env("KEI_TEST_PROCESS_DEATH_POINT", "v027-upgrade-snapshot")
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log))
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    while !root.path().join("ready").exists() {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "upgrade worker exited: {}",
            std::fs::read_to_string(root.path().join("worker.log")).unwrap()
        );
        assert!(Instant::now() < deadline, "upgrade worker timeout");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        std::fs::read_to_string(root.path().join("ready")).unwrap(),
        "v027-upgrade-snapshot"
    );
    child.0.kill().unwrap();
    assert_eq!(child.0.wait().unwrap().signal(), Some(9));
    assert_eq!(schema::get_schema_version(&wal).unwrap(), 36);
    retained(&wal);
    let saved = snapshots(root.path());
    assert_eq!(saved.len(), 1);
    assert_eq!(
        std::fs::metadata(&saved[0]).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let snapshot_bytes = std::fs::read(&saved[0]).unwrap();
    let snapshot = Connection::open(&saved[0]).unwrap();
    assert_eq!(schema::get_schema_version(&snapshot).unwrap(), 36);
    retained(&snapshot);
    drop(snapshot);
    drop(wal);
    for _ in 0..2 {
        let db = SqliteStateDb::open_owned(&path, &owner()).await.unwrap();
        let conn = db.acquire_lock("verify upgraded work").unwrap();
        assert_eq!(
            schema::get_schema_version(&conn).unwrap(),
            schema::SCHEMA_VERSION
        );
        retained(&conn);
        assert_eq!(std::fs::read(&saved[0]).unwrap(), snapshot_bytes);
        assert_eq!(std::fs::read(&media).unwrap(), b"unchanged media");
        assert_eq!(
            std::fs::metadata(&media).unwrap().modified().unwrap(),
            mtime
        );
    }
    // A killed attempt may leave a complete snapshot; retry may preserve another.
    // Neither can overwrite the first rollback database.
    let count = snapshots(root.path()).len();
    assert_eq!(count, 2);
}

#[tokio::test]
#[ignore = "isolated synthetic upgrade process killed by parent"]
async fn v027_upgrade_death_worker() {
    let root = PathBuf::from(std::env::var_os("KEI_TEST_PROCESS_DEATH_ROOT").unwrap());
    SqliteStateDb::open_owned(&root.join("state.sqlite"), &owner())
        .await
        .unwrap();
    panic!("upgrade completed without reaching snapshot hook");
}
