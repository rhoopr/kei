//! Connection-local, transactionally invalidated index of protected path spellings.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, params};

use super::invalid;
use crate::state::error::StateError;

fn key(path: &Path) -> Result<String, StateError> {
    crate::fs_util::confined_path_key(path).map_err(|_private_error| invalid())
}

// Start with catalogue rows: mapping-only libraries need no family enumeration.
fn has_family(library: &str, id: &str) -> String {
    format!(
        "EXISTS(SELECT 1 FROM main.asset_master_mappings m
            WHERE m.library={library} AND m.master_record_name={id} LIMIT 1 OFFSET 1)"
    )
}

fn has_paths(library: &str, id: &str) -> String {
    format!(
        "(EXISTS(SELECT 1 FROM main.assets a WHERE a.library={library} AND a.id={id}
                AND a.local_path IS NOT NULL)
        OR EXISTS(SELECT 1 FROM main.asset_metadata_paths p
            WHERE p.library={library} AND p.id={id}))"
    )
}

fn ambiguous(library: &str, id: &str) -> String {
    format!(
        "({} AND NOT EXISTS(SELECT 1 FROM main.legacy_master_state_owners o
            WHERE o.library={library} AND o.master_record_name={id})
        AND NOT EXISTS(SELECT 1 FROM main.asset_master_mappings c
            WHERE c.library={library} AND c.asset_record_name={id}))",
        has_family(library, id)
    )
}

fn relevant_family(library: &str, id: &str) -> String {
    format!(
        "({} AND {})",
        has_family(library, id),
        has_paths(library, id)
    )
}

fn census(conn: &Connection) -> Result<Vec<PathBuf>, StateError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT a.local_path FROM main.assets a WHERE a.local_path IS NOT NULL AND {}
        UNION SELECT p.local_path FROM main.asset_metadata_paths p WHERE {}",
        ambiguous("a.library", "a.id"),
        ambiguous("p.library", "p.id")
    ))?;
    let originals = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let mut paths = std::collections::HashSet::new();
    for original in originals {
        paths.insert(PathBuf::from(&original));
        paths.insert(PathBuf::from(format!("{original}.xmp")));
    }
    let mut stmt = conn.prepare("SELECT local_path FROM main.unattributed_legacy_paths")?;
    for path in stmt.query_map([], |row| row.get::<_, String>(0))? {
        paths.insert(PathBuf::from(path?));
    }
    for path in &paths {
        key(path)?;
    }
    Ok(paths.into_iter().collect())
}

#[derive(PartialEq, Eq)]
struct Version {
    data: i64,
    schema: i64,
    cwd: String,
}

fn version(conn: &Connection) -> Result<Version, StateError> {
    Ok(Version {
        data: conn.query_row("PRAGMA main.data_version", [], |row| row.get(0))?,
        schema: conn.query_row("PRAGMA main.schema_version", [], |row| row.get(0))?,
        cwd: key(&std::env::current_dir().map_err(|_private_error| invalid())?)?,
    })
}

fn install_triggers(conn: &Connection) -> Result<(), StateError> {
    for (table, id, condition) in [
        (
            "asset_master_mappings",
            "asset_record_name",
            "OLD.library IS NOT NEW.library OR OLD.asset_record_name IS NOT NEW.asset_record_name OR OLD.master_record_name IS NOT NEW.master_record_name",
        ),
        (
            "assets",
            "id",
            "OLD.library IS NOT NEW.library OR OLD.id IS NOT NEW.id OR OLD.local_path IS NOT NEW.local_path",
        ),
        (
            "asset_metadata_paths",
            "id",
            "OLD.library IS NOT NEW.library OR OLD.id IS NOT NEW.id OR OLD.local_path IS NOT NEW.local_path",
        ),
        (
            "legacy_master_state_owners",
            "master_record_name",
            "OLD.library IS NOT NEW.library OR OLD.master_record_name IS NOT NEW.master_record_name OR OLD.asset_record_name IS NOT NEW.asset_record_name",
        ),
        (
            "unattributed_legacy_paths",
            "asset_id",
            "OLD.path_key IS NOT NEW.path_key OR OLD.local_path IS NOT NEW.local_path",
        ),
    ] {
        for (event, timing, row) in [
            ("INSERT", "AFTER", "NEW"),
            ("DELETE", "BEFORE", "OLD"),
            ("UPDATE", "BEFORE", "OLD"),
            ("UPDATE", "AFTER", "NEW"),
        ] {
            let library = format!("{row}.library");
            let identity = format!("{row}.{id}");
            let affected = match table {
                "asset_master_mappings" => format!(
                    "({} OR {})",
                    relevant_family(&library, &format!("{row}.master_record_name")),
                    relevant_family(&library, &identity)
                ),
                "legacy_master_state_owners" => relevant_family(&library, &identity),
                "unattributed_legacy_paths" => "1".into(),
                _ => ambiguous(&library, &identity),
            };
            let changed = if event == "UPDATE" { condition } else { "1" };
            let has_path = if table == "assets" {
                format!("{row}.local_path IS NOT NULL")
            } else {
                "1".into()
            };
            let name = format!("legacy_path_cache_{table}_{event}_{timing}");
            // TEMP triggers and their validity row roll back with the caller's writes.
            conn.execute_batch(&format!(
                "DROP TRIGGER IF EXISTS temp.{name};
                CREATE TEMP TRIGGER {name} {timing} {event} ON main.{table}
                WHEN ({changed}) AND ({has_path}) AND ({affected})
                BEGIN DELETE FROM legacy_path_cache_valid; END;"
            ))?;
        }
    }
    Ok(())
}

fn ensure(conn: &Connection, expected: &Version) -> Result<(), StateError> {
    conn.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS legacy_path_cache(
            path_key TEXT NOT NULL, local_path TEXT NOT NULL,
            PRIMARY KEY(path_key,local_path)) WITHOUT ROWID;
        CREATE TEMP TABLE IF NOT EXISTS legacy_path_cache_valid(
            singleton INTEGER PRIMARY KEY CHECK(singleton=1),
            data_version INTEGER NOT NULL, schema_version INTEGER NOT NULL, cwd TEXT NOT NULL);",
    )?;
    let valid: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM temp.legacy_path_cache_valid
            WHERE data_version=?1 AND schema_version=?2 AND cwd=?3)",
        params![expected.data, expected.schema, expected.cwd],
        |row| row.get(0),
    )?;
    if valid {
        return Ok(());
    }
    conn.execute_batch("SAVEPOINT legacy_path_cache_rebuild")?;
    let rebuilt = (|| {
        install_triggers(conn)?;
        let paths = census(conn)?;
        conn.execute("DELETE FROM temp.legacy_path_cache_valid", [])?;
        conn.execute("DELETE FROM temp.legacy_path_cache", [])?;
        let mut insert = conn.prepare_cached(
            "INSERT OR IGNORE INTO temp.legacy_path_cache(path_key,local_path) VALUES (?1,?2)",
        )?;
        for path in paths {
            insert.execute(params![key(&path)?, path.to_str().ok_or_else(invalid)?])?;
        }
        conn.execute(
            "INSERT INTO temp.legacy_path_cache_valid VALUES (1,?1,?2,?3)",
            params![expected.data, expected.schema, expected.cwd],
        )?;
        Ok::<_, StateError>(())
    })();
    if rebuilt.is_err() {
        conn.execute_batch(
            "ROLLBACK TO legacy_path_cache_rebuild; RELEASE legacy_path_cache_rebuild",
        )?;
    } else {
        conn.execute_batch("RELEASE legacy_path_cache_rebuild")?;
    }
    rebuilt
}

fn inspect<T>(
    conn: &Connection,
    query: impl Fn(&Connection) -> Result<T, StateError>,
) -> Result<T, StateError> {
    // External commits have no TEMP triggers on this connection.
    for _ in 0..3 {
        let before = version(conn)?;
        ensure(conn, &before)?;
        let result = query(conn)?;
        if before == version(conn)? {
            return Ok(result);
        }
        conn.execute("DELETE FROM temp.legacy_path_cache_valid", [])?;
    }
    Err(invalid())
}

fn query_only(conn: &Connection) -> Result<bool, StateError> {
    Ok(conn.pragma_query_value(None, "query_only", |row| row.get(0))?)
}

pub(super) fn protected_paths(conn: &Connection) -> Result<Vec<PathBuf>, StateError> {
    if query_only(conn)? {
        return census(conn);
    }
    inspect(conn, |conn| {
        Ok(conn
            .prepare_cached("SELECT local_path FROM temp.legacy_path_cache")?
            .query_map([], |row| row.get::<_, String>(0).map(PathBuf::from))?
            .collect::<Result<_, _>>()?)
    })
}

pub(super) fn path_is_protected(conn: &Connection, path: &Path) -> Result<bool, StateError> {
    if query_only(conn)? {
        let path_key = key(path)?;
        let mut sidecar = path.as_os_str().to_os_string();
        sidecar.push(".xmp");
        let sidecar_key = key(Path::new(&sidecar))?;
        for protected in census(conn)? {
            let protected_key = key(&protected)?;
            if protected_key == path_key || protected_key == sidecar_key {
                return Ok(true);
            }
        }
        return Ok(false);
    }
    inspect(conn, |conn| {
        let path_key = key(path)?;
        let mut sidecar = path.as_os_str().to_os_string();
        sidecar.push(".xmp");
        let sidecar_key = key(Path::new(&sidecar))?;
        Ok(conn
            .prepare_cached(
                "SELECT 1 FROM temp.legacy_path_cache WHERE path_key IN (?1,?2) LIMIT 1",
            )?
            .query_row(params![path_key, sidecar_key], |_| Ok(()))
            .optional()?
            .is_some())
    })
}

#[cfg(test)]
mod tests;
