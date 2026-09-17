//! Tests moved from `state::db::tests`, with their original names and assertions.
use std::fs;

use super::SqliteStateDb;
use super::test_support::test_dir;
use crate::state::error::StateError;
use crate::test_helpers::TestAssetRecord;

#[tokio::test]
async fn test_open_creates_db() {
    let dir = test_dir();
    let path = dir.path().join("test.db");
    let db = SqliteStateDb::open(&path).await.unwrap();
    assert!(path.exists());
    assert_eq!(path, db.path());
}

#[tokio::test]
async fn open_corrupt_db_returns_error() {
    let dir = test_dir();
    let path = dir.path().join("corrupt.db");

    // Write garbage bytes (not a valid SQLite header)
    fs::write(&path, b"this is not a sqlite database at all").unwrap();

    let result = SqliteStateDb::open(&path).await;
    assert!(result.is_err(), "opening a corrupt DB should fail");

    let err = result.unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("not a database"),
        "error should indicate corruption, got: {msg}"
    );
}

#[tokio::test]
async fn open_truncated_db_returns_error() {
    let dir = test_dir();
    let path = dir.path().join("truncated.db");

    // Write a partial SQLite header (valid magic, but truncated)
    let mut header = b"SQLite format 3\0".to_vec();
    header.extend_from_slice(&[0u8; 16]); // truncated page header
    fs::write(&path, &header).unwrap();

    let result = SqliteStateDb::open(&path).await;
    assert!(result.is_err(), "opening a truncated DB should fail");
}

// Regression test for #264: on a fresh install the cookie/data directory
// doesn't exist yet for commands that open the DB before auth runs
// (import-existing, status, verify, reset, reconcile). SqliteStateDb::open
// must create the parent directory itself rather than letting SQLite fail
// with a generic "unable to open database file" (error code 14).
#[tokio::test]
async fn open_creates_missing_parent_directory() {
    let dir = test_dir();
    let nested = dir.path().join("does/not/exist/yet");
    let path = nested.join("state.db");

    assert!(!nested.exists(), "precondition: parent dir must be missing");

    let db = SqliteStateDb::open(&path)
        .await
        .expect("open should create the missing parent directory");

    assert!(nested.is_dir(), "parent directory should be created");
    assert!(path.is_file(), "DB file should be created");

    // Sanity: the DB is actually usable, not just opened then closed.
    let summary = db.get_summary().await.unwrap();
    assert_eq!(summary.downloaded, 0);
    assert_eq!(summary.pending, 0);
}

#[cfg(unix)]
#[tokio::test]
async fn open_returns_parent_dir_error_on_unwritable_parent() {
    use std::os::unix::fs::PermissionsExt;

    // Skip when running as root: 0o555 doesn't restrict root, so the
    // mkdir would succeed and the assertion below would falsely fail.
    // SAFETY: libc::geteuid() is a stateless POSIX FFI call with no
    // preconditions, no side effects, and a uid_t return value; it cannot
    // violate Rust memory safety.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }

    let dir = test_dir();
    let readonly = dir.path().join("readonly");
    tokio::fs::create_dir(&readonly).await.unwrap();
    tokio::fs::set_permissions(&readonly, fs::Permissions::from_mode(0o555))
        .await
        .unwrap();

    let path = readonly.join("nested/state.db");
    let result = SqliteStateDb::open(&path).await;

    // Restore writable permissions so TempDir cleanup can remove it.
    tokio::fs::set_permissions(&readonly, fs::Permissions::from_mode(0o755))
        .await
        .unwrap();

    let err = result.expect_err("expected ParentDir error on read-only parent");
    match &err {
        StateError::ParentDir { path: p, .. } => {
            assert!(
                p.starts_with(&readonly),
                "ParentDir path {} should be under {}",
                p.display(),
                readonly.display(),
            );
        }
        other => panic!("expected StateError::ParentDir, got {other:?}"),
    }
}

// WAL mid-transaction rollback invariant: a second connection that
// begins a transaction modifying committed rows, then is dropped
// without calling commit(), must not leave those modifications
// visible to any subsequent SqliteStateDb::open(). This stands in
// for the crash-between-BEGIN-and-COMMIT scenario (OOM kill, power
// loss, SIGKILL) that WAL mode is supposed to make safe.
//
// If kei ever flipped journal_mode away from WAL (or mis-used
// autocommit so every write lands without tx grouping), a single
// interrupted mark_downloaded batch could leave half-complete state
// that the next sync would read as truth, silently drifting the DB
// from the file system.
#[tokio::test]
async fn wal_uncommitted_transaction_is_invisible_on_reopen() {
    use rusqlite::Connection;

    let dir = test_dir();
    let path = dir.path().join("wal_rollback.db");
    let file_path = dir.path().join("keeper.jpg");
    fs::write(&file_path, b"bytes").unwrap();

    // Step 1: open, commit a downloaded row, close.
    {
        let db = SqliteStateDb::open(&path).await.unwrap();
        let record = TestAssetRecord::new("WAL_KEEPER")
            .checksum("ck_w")
            .filename("keeper.jpg")
            .size(5)
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "WAL_KEEPER",
            "original",
            &file_path,
            "localhash",
            None,
        )
        .await
        .unwrap();
    }

    // Step 2: open a raw rusqlite connection, begin an explicit
    // transaction that would corrupt the downloaded row, then drop
    // the connection WITHOUT committing. rusqlite's Connection Drop
    // rolls back any open transaction — which is exactly the
    // observable state SQLite exposes after a hard crash mid-tx.
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("BEGIN; UPDATE assets SET status = 'failed', last_error = 'would be set by crashed tx' WHERE id = 'WAL_KEEPER'; ").unwrap();
        // Confirm the UPDATE actually landed inside the open tx, so the
        // final assertion is proving rollback rather than a silent no-op
        // (e.g. row missing, UPDATE matching zero rows).
        let mid_tx_status: String = conn
            .query_row(
                "SELECT status FROM assets WHERE id = 'WAL_KEEPER'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(mid_tx_status, "failed");
        // Drop without commit → rollback.
        drop(conn);
    }

    // Step 3: reopen. The rolled-back UPDATE must not be visible.
    let db = SqliteStateDb::open(&path).await.unwrap();
    let ids = db.get_downloaded_ids().await.unwrap();
    assert!(
        ids.contains(&("PrimarySync".into(), "WAL_KEEPER".into(), "original".into())),
        "committed downloaded row must survive an adjacent uncommitted \
             transaction's rollback; got downloaded set: {ids:?}"
    );
    let summary = db.get_summary().await.unwrap();
    assert_eq!(
        summary.downloaded, 1,
        "rolled-back UPDATE must not alter the committed status"
    );
    assert_eq!(
        summary.failed, 0,
        "WAL_KEEPER must not have been promoted to failed by the \
             rolled-back transaction"
    );
}

#[tokio::test]
async fn open_db_at_future_schema_version_returns_loud_error() {
    let dir = test_dir();
    let db_path = dir.path().join("future.db");

    {
        let db = SqliteStateDb::open(&db_path).await.unwrap();
        db.with_conn("bump_version", |conn| {
            conn.pragma_update(
                None,
                "user_version",
                crate::state::schema::SCHEMA_VERSION + 1,
            )?;
            Ok(())
        })
        .await
        .unwrap();
    }

    let result = SqliteStateDb::open(&db_path).await;
    assert!(result.is_err(), "opening a future-version DB must fail");
    let err = result.unwrap_err();
    assert!(
        matches!(
            err,
            StateError::UnsupportedSchemaVersion { found, expected }
                if found == crate::state::schema::SCHEMA_VERSION + 1
                && expected == crate::state::schema::SCHEMA_VERSION
        ),
        "expected UnsupportedSchemaVersion, got: {err:?}"
    );
}

/// Corrupted state DB file (random bytes, truncated SQLite header)
/// must be detected on open, not silently misread or panicked.
#[tokio::test]
async fn corrupted_db_file_detected_on_open() {
    let dir = test_dir();
    let db_path = dir.path().join("corrupt.db");
    std::fs::write(&db_path, b"not a valid sqlite database file!").unwrap();
    let result = SqliteStateDb::open(&db_path).await;
    assert!(result.is_err(), "corrupted file must fail to open");
    let err = result.unwrap_err();
    assert!(
        err.to_string().contains("not a database")
            || err.to_string().contains("file is not a database"),
        "error must indicate the file is not a valid DB, got: {err:?}"
    );
}

/// Truncated state DB (valid SQLite header, short body) must be
/// detected on open or first query.
#[tokio::test]
async fn truncated_db_file_detected_on_open() {
    let dir = test_dir();
    let db_path = dir.path().join("trunc.db");
    // Write the SQLite magic header but nothing else
    let header: &[u8] = b"SQLite format 3\x00";
    std::fs::write(&db_path, header).unwrap();
    let result = SqliteStateDb::open(&db_path).await;
    assert!(result.is_err(), "truncated SQLite file must fail");
}
