//! Disposable connection-local path candidates, never completion or ownership proof.
//!
//! Uses the existing legacy path-cache protocol: TEMP invalidation rolls back
//! with local writes; external commits/schema changes invalidate the snapshot.
use super::{StateError, pending_keys, pending_manifest_paths};
use rusqlite::{Connection, params};

#[derive(PartialEq, Eq)]
struct Version {
    data: i64,
    schema: i64,
    cwd: String,
}

fn version(conn: &Connection) -> Result<Version, StateError> {
    let cwd = std::env::current_dir().map_err(|_| StateError::ProviderSelectionInvalid)?;
    Ok(Version {
        data: conn.query_row("PRAGMA main.data_version", [], |r| r.get(0))?,
        schema: conn.query_row("PRAGMA main.schema_version", [], |r| r.get(0))?,
        cwd: super::super::primary_layout::path_key(&cwd)?,
    })
}

fn uncached(conn: &Connection, chosen: &str) -> Result<Vec<(String, String, String)>, StateError> {
    let mut candidates = Vec::new();
    for (generation, pass, child) in pending_keys(conn)? {
        if pending_manifest_paths(conn, &generation, &pass, &child)?
            .iter()
            .any(|path| path == chosen)
        {
            candidates.push((generation, pass, child));
        }
    }
    Ok(candidates)
}

fn ensure(conn: &Connection, expected: &Version) -> Result<(), StateError> {
    conn.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS pending_path_candidates(
            path TEXT NOT NULL,generation TEXT NOT NULL,pass TEXT NOT NULL,child TEXT NOT NULL,
            PRIMARY KEY(path,generation,pass,child)) WITHOUT ROWID;
         CREATE TEMP TABLE IF NOT EXISTS pending_path_candidates_valid(
            singleton INTEGER PRIMARY KEY CHECK(singleton=1),
            data_version INTEGER NOT NULL,schema_version INTEGER NOT NULL,cwd TEXT NOT NULL);",
    )?;
    let valid: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM temp.pending_path_candidates_valid
         WHERE data_version=?1 AND schema_version=?2 AND cwd=?3)",
        params![expected.data, expected.schema, expected.cwd],
        |r| r.get(0),
    )?;
    if valid {
        return Ok(());
    }
    conn.execute_batch("SAVEPOINT pending_path_candidates_rebuild")?;
    let rebuilt = (|| {
        // Any change to the census, original evidence or destination projection
        // invalidates absence. Unrelated asset/receipt writes need not do so.
        for table in [
            "provider_active_generations",
            "provider_active_decisions",
            "provider_active_destinations",
        ] {
            for event in ["INSERT", "UPDATE", "DELETE"] {
                let name = format!("pending_path_candidates_{table}_{event}");
                conn.execute_batch(&format!(
                    "DROP TRIGGER IF EXISTS temp.{name};
                     CREATE TEMP TRIGGER {name} AFTER {event} ON main.{table}
                     BEGIN DELETE FROM pending_path_candidates_valid; END;"
                ))?;
            }
        }
        conn.execute("DELETE FROM temp.pending_path_candidates_valid", [])?;
        conn.execute("DELETE FROM temp.pending_path_candidates", [])?;
        let mut insert = conn.prepare_cached(
            "INSERT OR IGNORE INTO temp.pending_path_candidates VALUES(?1,?2,?3,?4)",
        )?;
        for (generation, pass, child) in pending_keys(conn)? {
            for path in pending_manifest_paths(conn, &generation, &pass, &child)? {
                insert.execute(params![path, generation, pass, child])?;
            }
        }
        conn.execute(
            "INSERT INTO temp.pending_path_candidates_valid VALUES(1,?1,?2,?3)",
            params![expected.data, expected.schema, expected.cwd],
        )?;
        Ok::<_, StateError>(())
    })();
    if rebuilt.is_err() {
        conn.execute_batch(
            "ROLLBACK TO pending_path_candidates_rebuild; RELEASE pending_path_candidates_rebuild",
        )?;
    } else {
        conn.execute_batch("RELEASE pending_path_candidates_rebuild")?;
    }
    rebuilt
}

pub(super) fn candidates(
    conn: &Connection,
    chosen: &str,
) -> Result<Vec<(String, String, String)>, StateError> {
    if conn.pragma_query_value(None, "query_only", |r| r.get::<_, bool>(0))? {
        return uncached(conn, chosen);
    }
    for _ in 0..3 {
        let before = version(conn)?;
        ensure(conn, &before)?;
        let candidates = conn.prepare_cached(
            "SELECT generation,pass,child FROM temp.pending_path_candidates WHERE path=?1 ORDER BY generation,pass,child")?
            .query_map([chosen],|r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?
            .collect::<Result<Vec<_>,_>>()?;
        if before == version(conn)? {
            return Ok(candidates);
        }
        conn.execute("DELETE FROM temp.pending_path_candidates_valid", [])?;
    }
    Err(StateError::ProviderSelectionInvalid)
}
