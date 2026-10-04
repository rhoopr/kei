//! Deterministic process death in the actual adoption owner, not power loss.

use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use super::{AccountOwner, adopt_legacy, open_read_only, state_path};
use crate::state::{SqliteStateDb, error::StateError};

const USERNAME: &str = "migration@example.invalid";
const WORKER: &str = "state::db::account::interruption_tests::account_adoption_interruption_worker";
const POINTS: [&str; 5] = [
    "account-adoption-stage-open",
    "account-adoption-copied",
    "account-adoption-stage-synced",
    "account-adoption-published",
    "account-adoption-directory-synced",
];

fn owner() -> AccountOwner {
    AccountOwner::authenticated(
        USERNAME,
        "com",
        &serde_json::from_value(serde_json::json!({"dsInfo":{"dsid":"migration-provider"}}))
            .unwrap(),
    )
    .unwrap()
}

fn target(root: &Path) -> PathBuf {
    root.join(format!("{}.db", crate::account::namespace(USERNAME, "com")))
}

fn source(root: &Path) -> PathBuf {
    root.join("migrationexampleinvalid.db")
}

struct Worker(Child);
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn assert_state(conn: &rusqlite::Connection) {
    assert_eq!(
        conn.query_row::<String, _, _>(
            "SELECT value FROM metadata WHERE key='sync_token:PrimarySync'",
            [],
            |r| r.get(0)
        )
        .unwrap(),
        "legacy-cursor"
    );
    assert_eq!(
        conn.query_row::<String, _, _>(
            "SELECT value FROM metadata WHERE key='pending_sync_token:old:PrimarySync'",
            [],
            |r| r.get(0)
        )
        .unwrap(),
        "historical-debt"
    );
    assert_eq!(
        conn.query_row::<i64, _, _>("SELECT count(*) FROM assets", [], |r| r.get(0))
            .unwrap(),
        2
    );
    assert_eq!(
        conn.query_row::<String, _, _>("SELECT status FROM assets WHERE id='retry'", [], |r| r
            .get(0))
            .unwrap(),
        "pending"
    );
    assert_eq!(
        conn.query_row::<Vec<u8>, _, _>("SELECT payload FROM future_unknown", [], |r| r.get(0))
            .unwrap(),
        [0, 255, 7]
    );
    assert_eq!(
        conn.query_row::<String, _, _>(
            "SELECT local_checksum FROM asset_metadata_paths",
            [],
            |r| r.get(0)
        )
        .unwrap(),
        "legacy-receipt"
    );
}

#[tokio::test]
async fn account_adoption_interruption_worker() {
    let Some(root) = std::env::var_os("KEI_TEST_ACCOUNT_ADOPTION_WORKER") else {
        return;
    };
    let root = PathBuf::from(root);
    adopt_legacy(&source(&root), &target(&root), &owner(), USERNAME, "com")
        .await
        .unwrap();
    panic!("configured production adoption point was not reached");
}

#[tokio::test]
async fn account_adoption_process_death_preserves_source_and_recovers_at_copy_and_publication() {
    for point in POINTS {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        std::fs::write(root.join("fixture-owner"), b"kei-synthetic-process-death").unwrap();
        let media = root.join("kept.jpg");
        std::fs::write(&media, b"original media stays exact").unwrap();
        let db = SqliteStateDb::open(&source(root)).await.unwrap();
        db.set_metadata("sync_token:PrimarySync", "legacy-cursor")
            .await
            .unwrap();
        db.set_metadata("pending_sync_token:old:PrimarySync", "historical-debt")
            .await
            .unwrap();
        {
            let conn = db.acquire_lock("migration interruption seed").unwrap();
            conn.execute_batch("CREATE TABLE future_unknown(payload BLOB NOT NULL); INSERT INTO future_unknown VALUES (X'00FF07'); INSERT INTO assets(library,id,version_size,checksum,filename,created_at,size_bytes,media_type,last_seen_at,status) VALUES ('PrimarySync','kept','original','legacy-provider','kept.jpg',1000,25,'photo',1000,'downloaded'),('PrimarySync','retry','original','pending-provider','pending.jpg',1000,4,'photo',1000,'pending');").unwrap();
            conn.execute("INSERT INTO asset_metadata_paths(library,id,version_size,local_path,provider_checksum,local_checksum) VALUES ('PrimarySync','kept','original',?1,'legacy-provider','legacy-receipt')",[media.to_str().unwrap()]).unwrap();
        }
        // Leave committed source rows in WAL. The reader and killed adoption
        // must not checkpoint or rewrite this still-live legacy connection.
        let source_bytes = std::fs::read(source(root)).unwrap();
        let wal = source(root).with_extension("db-wal");
        let wal_bytes = std::fs::read(&wal).unwrap();
        assert!(source(root).with_extension("db-shm").exists());
        let mut child = Worker(
            Command::new(std::env::current_exe().unwrap())
                .args([WORKER, "--exact", "--nocapture"])
                .env("KEI_TEST_ACCOUNT_ADOPTION_WORKER", root)
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
                "{point}: worker exited before production point"
            );
            assert!(
                Instant::now() < deadline,
                "{point}: production point timed out"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(std::fs::read_to_string(root.join("ready")).unwrap(), point);
        child.0.kill().unwrap();
        assert_eq!(
            child.0.wait().unwrap().signal(),
            Some(9),
            "{point}: require real SIGKILL"
        );
        let stages: Vec<_> = std::fs::read_dir(root)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".account-adoption-")
                    && p.extension().is_some_and(|ext| ext == "db")
            })
            .collect();
        assert_eq!(
            stages.len(),
            1,
            "{point}: prove we killed an actual private copy"
        );
        let stage = &stages[0];
        let stage_bytes = std::fs::read(stage).unwrap();
        if point == "account-adoption-stage-open" {
            assert!(stage_bytes.is_empty(), "backup must not have started");
        } else {
            let staged = open_read_only(stage).unwrap();
            assert_state(&staged);
            if point == "account-adoption-copied" {
                assert!(matches!(
                    super::validate(&staged, &owner()),
                    Err(StateError::AccountOwnerMissing)
                ));
            } else {
                super::validate(&staged, &owner()).unwrap();
            }
        }
        let published = matches!(
            point,
            "account-adoption-published" | "account-adoption-directory-synced"
        );
        assert_eq!(
            target(root).exists(),
            published,
            "{point}: publication boundary"
        );
        assert!(
            source_bytes == std::fs::read(source(root)).unwrap(),
            "{point}: original DB bytes"
        );
        assert!(
            wal_bytes == std::fs::read(&wal).unwrap(),
            "{point}: original WAL bytes"
        );
        assert!(source(root).with_extension("db-shm").exists());
        if !published {
            assert!(matches!(
                state_path(root, USERNAME, "com").await,
                Err(StateError::LegacyAccountMigrationRequired)
            ));
            adopt_legacy(&source(root), &target(root), &owner(), USERNAME, "com")
                .await
                .unwrap();
        } else {
            // A complete published target is authoritative even when cleanup
            // was interrupted. Retrying adoption must never replace it.
            let published_bytes = std::fs::read(target(root)).unwrap();
            assert!(
                adopt_legacy(&source(root), &target(root), &owner(), USERNAME, "com")
                    .await
                    .is_err()
            );
            assert!(
                published_bytes == std::fs::read(target(root)).unwrap(),
                "published target was replaced"
            );
        }
        for _ in 0..2 {
            assert_eq!(
                state_path(root, USERNAME, "com").await.unwrap(),
                target(root)
            );
            let reopened = SqliteStateDb::open_owned(&target(root), &owner())
                .await
                .unwrap();
            assert_state(&reopened.acquire_lock("reopen adopted state").unwrap());
            drop(reopened);
        }
        assert!(source_bytes == std::fs::read(source(root)).unwrap());
        assert!(wal_bytes == std::fs::read(&wal).unwrap());
        assert_eq!(
            std::fs::read(&media).unwrap(),
            b"original media stays exact"
        );
        // Interrupted private stages are preserved, never implicitly reused
        // or deleted. Retry creates no additional lingering private stage.
        if published {
            // This lingering private name is the same inode as the canonical
            // published DB. Legitimate WAL initialization changes both names.
            assert!(
                std::fs::read(stage).unwrap() == std::fs::read(target(root)).unwrap(),
                "{point}: interrupted hard-link alias"
            );
        } else {
            assert!(
                stage_bytes == std::fs::read(stage).unwrap(),
                "{point}: unpublished stage changed"
            );
        }
        assert_eq!(
            std::fs::read_dir(root)
                .unwrap()
                .filter(|e| e
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".account-adoption-")
                    && e.as_ref()
                        .unwrap()
                        .path()
                        .extension()
                        .is_some_and(|ext| ext == "db"))
                .count(),
            1
        );
        drop(db);
    }
}
