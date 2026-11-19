//! One-time physical reclamation after retiring the v0.30 shadow inbox.

use std::path::Path;

use rusqlite::Connection;

use crate::state::schema::INBOX_COMPACTION_PENDING;

const MIN_RECLAIM_BYTES: u64 = 8 * 1024 * 1024;

fn pending(conn: &Connection) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM metadata WHERE key=?1)",
        [INBOX_COMPACTION_PENDING],
        |row| row.get(0),
    )
}

pub(super) fn retry(conn: &mut Connection, path: &Path) -> rusqlite::Result<()> {
    if !pending(conn)? {
        return Ok(());
    }
    // Keep the SQLite file lock across the transaction boundary: VACUUM
    // cannot run inside a transaction. Enter from normal WAL locking mode.
    conn.pragma_update(None, "locking_mode", "EXCLUSIVE")?;
    let result = compact(conn, path, || fs4::available_space(path));
    conn.pragma_update(None, "locking_mode", "NORMAL")?;
    // SQLite releases the retained lock on the next database access.
    conn.query_row("SELECT count(*) FROM sqlite_schema", [], |_| Ok(()))?;
    if let Err(error) = result {
        tracing::warn!(%error, "State compaction deferred until the next writable open");
    }
    Ok(())
}

fn checkpoint(conn: &Connection) -> anyhow::Result<()> {
    let busy: bool = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))?;
    anyhow::ensure!(!busy, "state checkpoint busy");
    Ok(())
}

fn compact(
    conn: &mut Connection,
    path: &Path,
    available_space: impl FnOnce() -> std::io::Result<u64>,
) -> anyhow::Result<()> {
    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Exclusive)?
        .commit()?;
    if !pending(conn)? {
        return Ok(());
    }
    // If killed after VACUUM committed, its truncation may still be in WAL.
    checkpoint(conn)?;
    let free: u32 = conn.pragma_query_value(None, "freelist_count", |row| row.get(0))?;
    let page: u32 = conn.pragma_query_value(None, "page_size", |row| row.get(0))?;
    let reclaim = u64::from(free) * u64::from(page);
    if reclaim >= MIN_RECLAIM_BYTES {
        let before = std::fs::metadata(path)?.len();
        // Budget for the compact temporary database and its WAL publication.
        let needed = before.saturating_sub(reclaim).saturating_mul(2) + MIN_RECLAIM_BYTES;
        anyhow::ensure!(
            available_space()? >= needed,
            "insufficient disk space for state compaction"
        );
        #[cfg(all(test, target_os = "linux"))]
        if std::env::var("KEI_TEST_PROCESS_DEATH_POINT").as_deref() == Ok("inbox-during-vacuum") {
            conn.progress_handler(
                100,
                Some(|| {
                    crate::test_helpers::process_death_point("inbox-during-vacuum");
                    false
                }),
            )?;
        }
        conn.execute_batch("VACUUM")?;
        #[cfg(all(test, target_os = "linux"))]
        crate::test_helpers::process_death_point("inbox-vacuum-committed");
        checkpoint(conn)?;
        let after = std::fs::metadata(path)?.len();
        tracing::info!(
            before_bytes = before,
            after_bytes = after,
            "Compacted state database"
        );
    }
    conn.execute(
        "DELETE FROM metadata WHERE key=?1",
        [INBOX_COMPACTION_PENDING],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{INBOX_COMPACTION_PENDING, compact, pending};
    use rusqlite::Connection;

    #[test]
    fn inbox_compaction_insufficient_disk_keeps_committed_cleanup_retryable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let mut conn = Connection::open(&path).unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA locking_mode=EXCLUSIVE;
            CREATE TABLE metadata(key TEXT PRIMARY KEY, value TEXT);
            CREATE TABLE retired(body BLOB); INSERT INTO retired VALUES(zeroblob(12582912)); DROP TABLE retired;").unwrap();
        conn.execute(
            "INSERT INTO metadata VALUES(?1,'1')",
            [INBOX_COMPACTION_PENDING],
        )
        .unwrap();
        let cookie = |conn: &Connection| {
            conn.pragma_query_value::<i64, _>(None, "schema_version", |r| r.get(0))
                .unwrap()
        };
        let original = cookie(&conn);
        let error = compact(&mut conn, &path, || Ok(0)).unwrap_err();
        assert!(error.to_string().contains("insufficient disk space"));
        assert_eq!(cookie(&conn), original);
        assert!(pending(&conn).unwrap());
        let before = path.metadata().unwrap().len();
        compact(&mut conn, &path, || Ok(u64::MAX)).unwrap();
        assert!(!pending(&conn).unwrap());
        drop(conn);
        assert!(before - path.metadata().unwrap().len() >= 12 * 1024 * 1024);
    }
}
