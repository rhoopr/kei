//! Transactional candidates for root-change admission. Entries never grant trust.
use super::{StateError, params};
use rusqlite::{Connection, OptionalExtension};

#[cfg(test)]
std::thread_local! {
    static COUNTS: std::cell::Cell<[usize; 3]> = const { std::cell::Cell::new([0; 3]) };
}
#[cfg(test)]
pub(super) fn reset_counts() {
    COUNTS.with(|counter| counter.set([0; 3]));
}
#[cfg(test)]
pub(super) fn counts() -> [usize; 3] {
    COUNTS.with(std::cell::Cell::get)
}
#[cfg(test)]
pub(super) fn note_manifest_load() {
    count(2);
}

#[cfg(test)]
fn count(index: usize) {
    COUNTS.with(|counter| {
        let mut values = counter.get();
        if let Some(value) = values.get_mut(index) {
            *value += 1;
        }
        counter.set(values);
    });
}

type DecisionKey = (String, String, String);
type OwnerKey = (String, String, String, String, String);

fn invalid() -> StateError {
    StateError::ProviderSelectionInvalid
}

fn version(conn: &Connection) -> Result<(i64, i64, String), StateError> {
    let cwd = std::env::current_dir().map_err(|_error| invalid())?;
    Ok((
        conn.query_row("PRAGMA main.data_version", [], |r| r.get(0))?,
        conn.query_row("PRAGMA main.schema_version", [], |r| r.get(0))?,
        super::super::primary_layout::path_key(&cwd)?,
    ))
}

fn install(conn: &Connection) -> Result<(), StateError> {
    conn.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS root_owner_dirty(
            kind TEXT NOT NULL,a TEXT NOT NULL,b TEXT NOT NULL,c TEXT NOT NULL,d TEXT NOT NULL,
            PRIMARY KEY(kind,a,b,c,d)) WITHOUT ROWID;
         CREATE TEMP TABLE IF NOT EXISTS root_owner_candidates(
            kind TEXT NOT NULL,a TEXT NOT NULL,b TEXT NOT NULL,c TEXT NOT NULL,d TEXT NOT NULL,path TEXT NOT NULL,
            PRIMARY KEY(kind,a,b,c,d)) WITHOUT ROWID;
         CREATE INDEX IF NOT EXISTS temp.root_owner_path ON root_owner_candidates(path);
         CREATE TEMP TABLE IF NOT EXISTS root_decision_dirty(
            generation TEXT NOT NULL,pass TEXT NOT NULL,child TEXT NOT NULL,
            PRIMARY KEY(generation,pass,child)) WITHOUT ROWID;
         CREATE TEMP TABLE IF NOT EXISTS root_generation_dirty(
            generation TEXT PRIMARY KEY) WITHOUT ROWID;
         CREATE TEMP TABLE IF NOT EXISTS root_generation_header_dirty(
            generation TEXT PRIMARY KEY) WITHOUT ROWID;
         CREATE TEMP TABLE IF NOT EXISTS root_pending_candidates(
            generation TEXT NOT NULL,pass TEXT NOT NULL,child TEXT NOT NULL,
            path TEXT NOT NULL,library TEXT NOT NULL,asset TEXT NOT NULL,version TEXT NOT NULL,
            PRIMARY KEY(generation,pass,child,path,version)) WITHOUT ROWID;
         CREATE INDEX IF NOT EXISTS temp.root_pending_path ON root_pending_candidates(path);
         CREATE INDEX IF NOT EXISTS temp.root_pending_identity ON root_pending_candidates(library,asset,version);
         CREATE TEMP TABLE IF NOT EXISTS root_candidates_valid(
            singleton INTEGER PRIMARY KEY CHECK(singleton=1),
            data_version INTEGER NOT NULL,schema_version INTEGER NOT NULL,cwd TEXT NOT NULL);",
    )?;
    for (table, kind, old, new, changed) in [
        (
            "assets",
            "asset",
            "OLD.library,OLD.id,OLD.version_size,''",
            "NEW.library,NEW.id,NEW.version_size,''",
            "OLD.library IS NOT NEW.library OR OLD.id IS NOT NEW.id OR OLD.version_size IS NOT NEW.version_size OR OLD.local_path IS NOT NEW.local_path",
        ),
        (
            "asset_metadata_paths",
            "metadata",
            "OLD.library,OLD.id,OLD.version_size,OLD.local_path",
            "NEW.library,NEW.id,NEW.version_size,NEW.local_path",
            "OLD.library IS NOT NEW.library OR OLD.id IS NOT NEW.id OR OLD.version_size IS NOT NEW.version_size OR OLD.local_path IS NOT NEW.local_path",
        ),
        (
            "download_receipts",
            "receipt",
            "OLD.id,'','',''",
            "NEW.id,'','',''",
            "OLD.id IS NOT NEW.id OR OLD.native_path IS NOT NEW.native_path",
        ),
    ] {
        for (event, values, condition) in [
            ("INSERT", new, "1".to_owned()),
            ("DELETE", old, "1".to_owned()),
            ("UPDATE", old, changed.to_owned()),
        ] {
            let name = format!("root_candidate_{table}_{event}");
            let second = if event == "UPDATE" {
                format!("INSERT OR IGNORE INTO root_owner_dirty VALUES('{kind}',{new});")
            } else {
                String::new()
            };
            conn.execute_batch(&format!(
                "CREATE TEMP TRIGGER IF NOT EXISTS {name} AFTER {event} ON main.{table}
                 WHEN {condition} BEGIN
                   INSERT OR IGNORE INTO root_owner_dirty VALUES('{kind}',{values});
                   {second}
                 END;"
            ))?;
        }
    }
    for table in [
        "provider_active_decisions",
        "provider_active_destinations",
        "provider_active_sources",
    ] {
        for (event, old, new) in [
            ("INSERT", false, true),
            ("DELETE", true, false),
            ("UPDATE", true, true),
        ] {
            let mut body = String::new();
            if old {
                body.push_str("INSERT OR IGNORE INTO root_decision_dirty VALUES(OLD.generation,OLD.pass_key,OLD.child);");
            }
            if new {
                body.push_str("INSERT OR IGNORE INTO root_decision_dirty VALUES(NEW.generation,NEW.pass_key,NEW.child);");
            }
            conn.execute_batch(&format!(
                "CREATE TEMP TRIGGER IF NOT EXISTS root_candidate_{table}_{event}
                 AFTER {event} ON main.{table} BEGIN {body} END;"
            ))?;
        }
    }
    // Structural mutations in a sealed generation must revalidate its
    // complete seal; progress-only updates refresh just their decision.
    for (table, changed) in [
        (
            "provider_active_decisions",
            "OLD.generation IS NOT NEW.generation OR OLD.pass_key IS NOT NEW.pass_key OR OLD.child IS NOT NEW.child OR OLD.manifest_hash IS NOT NEW.manifest_hash",
        ),
        (
            "provider_active_destinations",
            "OLD.generation IS NOT NEW.generation OR OLD.pass_key IS NOT NEW.pass_key OR OLD.child IS NOT NEW.child OR OLD.path IS NOT NEW.path OR OLD.grouping_hash IS NOT NEW.grouping_hash",
        ),
    ] {
        for (event, old, new) in [
            ("INSERT", false, true),
            ("DELETE", true, false),
            ("UPDATE", true, true),
        ] {
            let mut body = String::new();
            if old {
                body.push_str("INSERT OR IGNORE INTO root_generation_dirty SELECT OLD.generation WHERE EXISTS(SELECT 1 FROM main.provider_active_generations WHERE id=OLD.generation AND sealed=1);");
            }
            if new {
                body.push_str("INSERT OR IGNORE INTO root_generation_dirty SELECT NEW.generation WHERE EXISTS(SELECT 1 FROM main.provider_active_generations WHERE id=NEW.generation AND sealed=1);");
            }
            let condition = if event == "UPDATE" { changed } else { "1" };
            conn.execute_batch(&format!(
                "CREATE TEMP TRIGGER IF NOT EXISTS root_candidate_seal_{table}_{event}
                 AFTER {event} ON main.{table} WHEN {condition} BEGIN {body} END;"
            ))?;
        }
    }
    for table in [
        "provider_selection_rank_pages",
        "provider_selection_rank_records",
    ] {
        for (event, old, new) in [
            ("INSERT", false, true),
            ("DELETE", true, false),
            ("UPDATE", true, true),
        ] {
            let mut body = String::new();
            for (enabled, row) in [(old, "OLD"), (new, "NEW")] {
                if enabled {
                    let generation = if table == "provider_selection_rank_pages" {
                        format!("{row}.generation")
                    } else {
                        format!(
                            "(SELECT generation FROM main.provider_selection_rank_pages WHERE id={row}.page_id)"
                        )
                    };
                    body.push_str(&format!(
                        "INSERT OR IGNORE INTO root_generation_dirty SELECT {generation} WHERE EXISTS(SELECT 1 FROM main.provider_active_generations WHERE id={generation} AND sealed=1);"
                    ));
                }
            }
            conn.execute_batch(&format!(
                "CREATE TEMP TRIGGER IF NOT EXISTS root_candidate_seal_{table}_{event}
                 AFTER {event} ON main.{table} BEGIN {body} END;"
            ))?;
        }
    }
    for (event, old, new) in [
        ("INSERT", false, true),
        ("DELETE", true, false),
        ("UPDATE", true, true),
    ] {
        let mut body = String::new();
        if old {
            body.push_str("INSERT OR IGNORE INTO root_generation_dirty VALUES(OLD.id);");
        }
        if new {
            body.push_str("INSERT OR IGNORE INTO root_generation_dirty VALUES(NEW.id);");
        }
        let condition = if event == "UPDATE" {
            "OLD.id IS NOT NEW.id OR OLD.account_key IS NOT NEW.account_key OR OLD.provider_key IS NOT NEW.provider_key OR OLD.scope IS NOT NEW.scope OR OLD.config_hash IS NOT NEW.config_hash OR OLD.basis IS NOT NEW.basis OR OLD.metadata_enabled IS NOT NEW.metadata_enabled OR OLD.specification IS NOT NEW.specification OR OLD.specification_hash IS NOT NEW.specification_hash OR OLD.sealed IS NOT NEW.sealed OR OLD.checkpoint_ready IS NOT NEW.checkpoint_ready OR OLD.checkpoint_veto IS NOT NEW.checkpoint_veto OR OLD.seal_hash IS NOT NEW.seal_hash OR OLD.seal_header_hash IS NOT NEW.seal_header_hash OR OLD.created_at IS NOT NEW.created_at"
        } else {
            "1"
        };
        conn.execute_batch(&format!(
            "CREATE TEMP TRIGGER IF NOT EXISTS root_candidate_generation_{event}
             AFTER {event} ON main.provider_active_generations WHEN {condition} BEGIN {body} END;"
        ))?;
    }
    // Replay cursors and accounting can change per batch without changing
    // membership, paths or the seal. Validate the header, not every decision.
    conn.execute_batch(
        "CREATE TEMP TRIGGER IF NOT EXISTS root_candidate_generation_header_update
         AFTER UPDATE ON main.provider_active_generations BEGIN
           INSERT OR IGNORE INTO root_generation_header_dirty VALUES(OLD.id);
           INSERT OR IGNORE INTO root_generation_header_dirty VALUES(NEW.id);
         END;",
    )?;
    Ok(())
}

fn refresh_owners(conn: &Connection) -> Result<(), StateError> {
    let keys: Vec<OwnerKey> = conn
        .prepare("SELECT kind,a,b,c,d FROM temp.root_owner_dirty ORDER BY kind,a,b,c,d")?
        .query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .collect::<Result<_, _>>()?;
    for (kind, a, b, c, d) in keys {
        #[cfg(test)]
        count(1);
        conn.execute("DELETE FROM temp.root_owner_candidates WHERE kind=?1 AND a=?2 AND b=?3 AND c=?4 AND d=?5",
            params![kind,a,b,c,d])?;
        let native: Option<String> = match kind.as_str() {
            "asset" => conn.query_row(
                "SELECT local_path FROM assets WHERE library=?1 AND id=?2 AND version_size=?3 AND local_path IS NOT NULL",
                params![a,b,c], |r| r.get(0)).optional()?,
            "metadata" => conn.query_row(
                "SELECT local_path FROM asset_metadata_paths WHERE library=?1 AND id=?2 AND version_size=?3 AND local_path=?4",
                params![a,b,c,d], |r| r.get(0)).optional()?,
            "receipt" => conn.query_row(
                "SELECT native_path FROM download_receipts WHERE id=?1", [&a], |r| r.get(0)).optional()?,
            _ => return Err(invalid()),
        };
        if let Some(native) = native {
            let path = if kind == "receipt" {
                let value: super::super::provider_selection::SelectionPath =
                    serde_json::from_str(&native).map_err(|_error| invalid())?;
                if super::super::provider_selection::SelectionPath::from_path(&value.to_path())
                    != value
                {
                    return Err(invalid());
                }
                value.to_path()
            } else {
                std::path::PathBuf::from(native)
            };
            let key = super::super::primary_layout::path_key(&path)?;
            conn.execute(
                "INSERT INTO temp.root_owner_candidates VALUES(?1,?2,?3,?4,?5,?6)",
                params![kind, a, b, c, d, key],
            )?;
        }
        conn.execute(
            "DELETE FROM temp.root_owner_dirty WHERE kind=?1 AND a=?2 AND b=?3 AND c=?4 AND d=?5",
            params![kind, a, b, c, d],
        )?;
    }
    Ok(())
}

fn validate_retained_seal(conn: &Connection, generation: &str) -> Result<(), StateError> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM provider_active_generations WHERE id=?1)",
        [generation],
        |r| r.get(0),
    )?;
    if !exists {
        return Ok(());
    }
    let root = super::load_generation(conn, generation)?;
    if root.spec.format == 2 && root.sealed {
        let expected: String = conn.query_row(
            "SELECT seal_hash FROM provider_active_generations WHERE id=?1",
            [generation],
            |r| r.get(0),
        )?;
        if super::seal_hash(
            conn,
            &root,
            root.checkpoint_ready,
            root.checkpoint_veto.as_deref(),
        )? != expected
        {
            return Err(invalid());
        }
    }
    Ok(())
}

fn refresh_pending(conn: &Connection) -> Result<(), StateError> {
    let headers: Vec<String> = conn
        .prepare("SELECT generation FROM temp.root_generation_header_dirty")?
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    for generation in headers {
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM provider_active_generations WHERE id=?1)",
            [&generation],
            |r| r.get(0),
        )?;
        if exists {
            super::load_generation(conn, &generation)?;
        }
        conn.execute(
            "DELETE FROM temp.root_generation_header_dirty WHERE generation=?1",
            [&generation],
        )?;
    }
    let generations: Vec<String> = conn
        .prepare("SELECT generation FROM temp.root_generation_dirty")?
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    for generation in generations {
        validate_retained_seal(conn, &generation)?;
        conn.execute("INSERT OR IGNORE INTO temp.root_decision_dirty SELECT generation,pass,child FROM temp.root_pending_candidates WHERE generation=?1", [&generation])?;
        conn.execute("INSERT OR IGNORE INTO temp.root_decision_dirty SELECT generation,pass_key,child FROM provider_active_decisions WHERE generation=?1", [&generation])?;
        conn.execute("INSERT OR IGNORE INTO temp.root_decision_dirty SELECT DISTINCT generation,pass_key,child FROM provider_active_destinations WHERE generation=?1", [&generation])?;
        conn.execute("INSERT OR IGNORE INTO temp.root_decision_dirty SELECT DISTINCT generation,pass_key,child FROM provider_active_sources WHERE generation=?1", [&generation])?;
        conn.execute(
            "DELETE FROM temp.root_generation_dirty WHERE generation=?1",
            [&generation],
        )?;
    }
    let keys: Vec<DecisionKey> = conn.prepare("SELECT generation,pass,child FROM temp.root_decision_dirty ORDER BY generation,pass,child")?
        .query_map([], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?.collect::<Result<_,_>>()?;
    for (generation, pass, child) in keys {
        conn.execute(
            "DELETE FROM temp.root_pending_candidates WHERE generation=?1 AND pass=?2 AND child=?3",
            params![generation, pass, child],
        )?;
        let still_exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM provider_active_decisions WHERE generation=?1 AND pass_key=?2 AND child=?3)",
            params![generation, pass, child], |r| r.get(0))?;
        if still_exists {
            // The retained decision and its complete projection are the
            // authority for absence. Projected pending bits cannot decide
            // whether the decision itself needs validation.
            let root = super::load_generation(conn, &generation)?;
            if root.spec.format != 2 {
                conn.execute("DELETE FROM temp.root_decision_dirty WHERE generation=?1 AND pass=?2 AND child=?3", params![generation,pass,child])?;
                continue;
            }
            let manifest = super::load_decision(conn, &root, &pass, &child)?;
            let eligible: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM provider_active_destinations WHERE generation=?1 AND pass_key=?2 AND child=?3 AND admitted=1 AND verified_media=0)",
                params![generation,pass,child], |r| r.get(0))?;
            if !eligible {
                conn.execute("DELETE FROM temp.root_decision_dirty WHERE generation=?1 AND pass=?2 AND child=?3", params![generation,pass,child])?;
                continue;
            }
            super::pending_manifest_paths(conn, &generation, &pass, &child)?;
            #[cfg(test)]
            count(2);
            let library = root
                .spec
                .zone
                .get("zoneName")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(invalid)?;
            super::guard_library_scope(conn, &root.spec.scope, library)?;
            for destination in &manifest.decision.destinations {
                let pending: bool = conn.query_row(
                    "SELECT admitted=1 AND verified_media=0 FROM provider_active_destinations WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5",
                    params![generation,pass,child,destination.version_size,destination.path.key()?], |r| r.get(0))?;
                if pending {
                    let path = super::super::primary_layout::path_key(&destination.path.to_path())?;
                    conn.execute(
                        "INSERT INTO temp.root_pending_candidates VALUES(?1,?2,?3,?4,?5,?6,?7)",
                        params![
                            generation,
                            pass,
                            child,
                            path,
                            library,
                            manifest.state_id,
                            destination.version_size
                        ],
                    )?;
                }
            }
        } else {
            // A sealed generation's seal detects a decision and projection
            // deleted together, even though no candidate row remains.
            validate_retained_seal(conn, &generation)?;
            let orphaned: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM provider_active_destinations WHERE generation=?1 AND pass_key=?2 AND child=?3) OR EXISTS(SELECT 1 FROM provider_active_sources WHERE generation=?1 AND pass_key=?2 AND child=?3)",
                params![generation,pass,child], |r| r.get(0))?;
            if orphaned {
                return Err(invalid());
            }
        }
        conn.execute(
            "DELETE FROM temp.root_decision_dirty WHERE generation=?1 AND pass=?2 AND child=?3",
            params![generation, pass, child],
        )?;
    }
    Ok(())
}

#[cfg(test)]
static CACHE_READS: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<String, usize>>,
> = std::sync::OnceLock::new();

#[cfg(test)]
pub(super) fn cache_reads(path: &std::path::Path) -> usize {
    let values = CACHE_READS.get_or_init(Default::default).lock().unwrap();
    values
        .get(&path.to_string_lossy().into_owned())
        .copied()
        .unwrap_or(0)
}

fn ensure(conn: &Connection) -> Result<(), StateError> {
    #[cfg(test)]
    if let Some(path) = conn.path() {
        let mut values = CACHE_READS
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *values.entry(path.to_owned()).or_default() += 1;
    }
    if conn.is_autocommit() {
        return Err(invalid());
    }
    let before = version(conn)?;
    install(conn)?;
    let valid: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM temp.root_candidates_valid WHERE data_version=?1 AND schema_version=?2 AND cwd=?3)",
        params![before.0,before.1,before.2], |r| r.get(0))?;
    if !valid {
        #[cfg(test)]
        count(0);
        conn.execute_batch(
            "DELETE FROM temp.root_candidates_valid;
             DELETE FROM temp.root_owner_dirty; DELETE FROM temp.root_owner_candidates;
             DELETE FROM temp.root_decision_dirty; DELETE FROM temp.root_generation_dirty;
             DELETE FROM temp.root_generation_header_dirty;
             DELETE FROM temp.root_pending_candidates;
             INSERT INTO temp.root_owner_dirty SELECT 'asset',library,id,version_size,'' FROM assets WHERE local_path IS NOT NULL;
             INSERT INTO temp.root_owner_dirty SELECT 'metadata',library,id,version_size,local_path FROM asset_metadata_paths;
             INSERT INTO temp.root_owner_dirty SELECT 'receipt',id,'','','' FROM download_receipts;
             INSERT INTO temp.root_generation_dirty SELECT id FROM provider_active_generations;
             INSERT OR IGNORE INTO temp.root_decision_dirty SELECT generation,pass_key,child FROM provider_active_decisions;
             INSERT OR IGNORE INTO temp.root_decision_dirty SELECT DISTINCT generation,pass_key,child FROM provider_active_destinations;
             INSERT OR IGNORE INTO temp.root_decision_dirty SELECT DISTINCT generation,pass_key,child FROM provider_active_sources;")?;
    }
    refresh_owners(conn)?;
    refresh_pending(conn)?;
    if before != version(conn)? {
        return Err(invalid());
    }
    if !valid {
        conn.execute(
            "INSERT INTO temp.root_candidates_valid VALUES(1,?1,?2,?3)",
            params![before.0, before.1, before.2],
        )?;
    }
    Ok(())
}

pub(super) fn guard_owners(
    conn: &Connection,
    receipt: &super::super::download_receipts::DownloadReceipt,
) -> Result<(), StateError> {
    ensure(conn)?;
    let key = super::super::primary_layout::path_key(&receipt.native_path.to_path())?;
    let own = receipt.id()?;
    let candidates: Vec<OwnerKey> = conn
        .prepare("SELECT kind,a,b,c,d FROM temp.root_owner_candidates WHERE path=?1")?
        .query_map([&key], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .collect::<Result<_, _>>()?;
    for (kind, a, b, c, d) in candidates {
        let native: Option<String> = match kind.as_str() {
            "asset" => conn.query_row(
                "SELECT local_path FROM assets WHERE library=?1 AND id=?2 AND version_size=?3 AND local_path IS NOT NULL",
                params![a,b,c], |r| r.get(0)).optional()?,
            "metadata" => conn.query_row(
                "SELECT local_path FROM asset_metadata_paths WHERE library=?1 AND id=?2 AND version_size=?3 AND local_path=?4",
                params![a,b,c,d], |r| r.get(0)).optional()?,
            "receipt" => conn.query_row(
                "SELECT native_path FROM download_receipts WHERE id=?1", [&a], |r| r.get(0)).optional()?,
            _ => return Err(invalid()),
        };
        let native = native.ok_or_else(invalid)?;
        let path = if kind == "receipt" {
            let selected: super::super::provider_selection::SelectionPath =
                serde_json::from_str(&native).map_err(|_error| invalid())?;
            if super::super::provider_selection::SelectionPath::from_path(&selected.to_path())
                != selected
            {
                return Err(invalid());
            }
            selected.to_path()
        } else {
            std::path::PathBuf::from(native)
        };
        if super::super::primary_layout::path_key(&path)? != key {
            return Err(invalid());
        }
        if (kind == "receipt" && a != own)
            || (kind != "receipt"
                && (a != receipt.library || b != receipt.state_id || c != receipt.version_size))
        {
            return Err(StateError::ProviderWorkConflict);
        }
    }
    Ok(())
}

pub(super) fn pending_keys_for_path(
    conn: &Connection,
    path: &std::path::Path,
) -> Result<Vec<DecisionKey>, StateError> {
    ensure(conn)?;
    let key = super::super::primary_layout::path_key(path)?;
    Ok(conn
        .prepare(
            "SELECT DISTINCT generation,pass,child FROM temp.root_pending_candidates WHERE path=?1",
        )?
        .query_map([key], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<Result<_, _>>()?)
}

pub(super) fn pending_keys_for_resource(
    conn: &Connection,
    library: &str,
    asset: &str,
    version: &str,
) -> Result<Vec<DecisionKey>, StateError> {
    ensure(conn)?;
    Ok(conn.prepare("SELECT DISTINCT generation,pass,child FROM temp.root_pending_candidates WHERE library=?1 AND asset=?2 AND version=?3")?
        .query_map(params![library,asset,version], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?.collect::<Result<_,_>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn schema(conn: &Connection) {
        conn.execute_batch(
            "CREATE TABLE assets(library TEXT NOT NULL,id TEXT NOT NULL,version_size TEXT NOT NULL,local_path TEXT,PRIMARY KEY(library,id,version_size));
             CREATE TABLE asset_metadata_paths(library TEXT NOT NULL,id TEXT NOT NULL,version_size TEXT NOT NULL,local_path TEXT NOT NULL,PRIMARY KEY(library,id,version_size,local_path));
             CREATE TABLE download_receipts(id TEXT PRIMARY KEY,native_path TEXT NOT NULL);
             CREATE TABLE provider_active_generations(id TEXT PRIMARY KEY,specification BLOB);
             CREATE TABLE provider_active_decisions(generation TEXT NOT NULL,pass_key TEXT NOT NULL,child TEXT NOT NULL,manifest_hash TEXT,PRIMARY KEY(generation,pass_key,child));
             CREATE TABLE provider_active_destinations(generation TEXT NOT NULL,pass_key TEXT NOT NULL,child TEXT NOT NULL,path TEXT,grouping_hash TEXT,admitted INTEGER,verified_media INTEGER);
             CREATE TABLE provider_active_sources(generation TEXT NOT NULL,pass_key TEXT NOT NULL,child TEXT NOT NULL);
             CREATE TABLE provider_selection_rank_pages(id TEXT PRIMARY KEY,generation TEXT NOT NULL);
             CREATE TABLE provider_selection_rank_records(page_id TEXT NOT NULL);",
        ).unwrap();
    }

    fn owner_count(conn: &Connection, path: &Path) -> i64 {
        let key = super::super::super::primary_layout::path_key(path).unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM temp.root_owner_candidates WHERE path=?1",
            [key],
            |r| r.get(0),
        )
        .unwrap()
    }

    #[test]
    fn root_candidate_index_tracks_local_rollback_and_external_owners() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.sqlite");
        let conn = Connection::open(&db).unwrap();
        schema(&conn);
        let old = dir.path().join("Photo 10 AM.JPG");
        let alias = dir.path().join("photo 10AM.jpg");
        let new = dir.path().join("other.JPG");
        conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        ensure(&conn).unwrap();
        assert_eq!(owner_count(&conn, &old), 0);
        conn.execute(
            "INSERT INTO assets VALUES('L','foreign','original',?1)",
            [old.to_str().unwrap()],
        )
        .unwrap();
        ensure(&conn).unwrap();
        assert_eq!(owner_count(&conn, &alias), 1);
        conn.execute_batch("SAVEPOINT path_change").unwrap();
        conn.execute(
            "UPDATE assets SET local_path=?1 WHERE id='foreign'",
            [new.to_str().unwrap()],
        )
        .unwrap();
        ensure(&conn).unwrap();
        assert_eq!(owner_count(&conn, &old), 0);
        assert_eq!(owner_count(&conn, &new), 1);
        conn.execute_batch("ROLLBACK TO path_change; RELEASE path_change")
            .unwrap();
        ensure(&conn).unwrap();
        assert_eq!(owner_count(&conn, &old), 1);
        assert_eq!(owner_count(&conn, &new), 0);
        conn.execute_batch("SAVEPOINT identity_change").unwrap();
        conn.execute("UPDATE assets SET id='renamed' WHERE id='foreign'", [])
            .unwrap();
        ensure(&conn).unwrap();
        let renamed: i64 = conn.query_row(
            "SELECT COUNT(*) FROM temp.root_owner_candidates WHERE kind='asset' AND b='renamed'",
            [], |r| r.get(0)).unwrap();
        assert_eq!(renamed, 1);
        conn.execute_batch("ROLLBACK TO identity_change; RELEASE identity_change")
            .unwrap();
        ensure(&conn).unwrap();
        let restored: i64 = conn.query_row(
            "SELECT COUNT(*) FROM temp.root_owner_candidates WHERE kind='asset' AND b='foreign'",
            [], |r| r.get(0)).unwrap();
        assert_eq!(restored, 1);
        conn.execute_batch("COMMIT").unwrap();
        let external = Connection::open(&db).unwrap();
        external
            .execute("DELETE FROM assets WHERE id='foreign'", [])
            .unwrap();
        conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        ensure(&conn).unwrap();
        assert_eq!(owner_count(&conn, &old), 0);
        conn.execute_batch("COMMIT").unwrap();
        external
            .execute(
                "INSERT INTO assets VALUES('L','outside','original',?1)",
                [old.to_str().unwrap()],
            )
            .unwrap();
        conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        ensure(&conn).unwrap();
        assert_eq!(owner_count(&conn, &old), 1);
        conn.execute_batch("COMMIT").unwrap();
        external
            .execute(
                "UPDATE assets SET id='outside-renamed',local_path=?1 WHERE id='outside'",
                [new.to_str().unwrap()],
            )
            .unwrap();
        conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        ensure(&conn).unwrap();
        assert_eq!(owner_count(&conn, &old), 0);
        assert_eq!(owner_count(&conn, &new), 1);
        let outside: i64 = conn.query_row(
            "SELECT COUNT(*) FROM temp.root_owner_candidates WHERE kind='asset' AND b='outside-renamed'",
            [], |r| r.get(0)).unwrap();
        assert_eq!(outside, 1);
        conn.execute_batch("COMMIT").unwrap();
        external
            .execute("DELETE FROM assets WHERE id='outside-renamed'", [])
            .unwrap();
        conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        ensure(&conn).unwrap();
        assert_eq!(owner_count(&conn, &new), 0);
        conn.execute_batch("COMMIT").unwrap();
        external
            .execute("INSERT INTO download_receipts VALUES('bad','not-json')", [])
            .unwrap();
        conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        assert!(ensure(&conn).is_err());
        conn.execute_batch("ROLLBACK").unwrap();
        external
            .execute("DELETE FROM download_receipts WHERE id='bad'", [])
            .unwrap();
        conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        ensure(&conn).unwrap();
        assert_eq!(owner_count(&conn, &old), 0);
        conn.execute_batch("COMMIT").unwrap();
    }

    #[test]
    fn root_candidate_cold_census_rejects_orphaned_decision_and_destination() {
        for kind in ["decision", "destination", "source"] {
            let conn = Connection::open_in_memory().unwrap();
            schema(&conn);
            match kind {
                "decision" => conn.execute(
                    "INSERT INTO provider_active_decisions(generation,pass_key,child) VALUES('missing','pass','child')", [],
                ).unwrap(),
                "destination" => conn.execute(
                    "INSERT INTO provider_active_destinations(generation,pass_key,child,admitted,verified_media) VALUES('missing','pass','child',0,0)", [],
                ).unwrap(),
                "source" => conn.execute(
                    "INSERT INTO provider_active_sources(generation,pass_key,child) VALUES('missing','pass','child')", [],
                ).unwrap(),
                _ => panic!("unknown orphan"),
            };
            conn.execute_batch("BEGIN IMMEDIATE").unwrap();
            assert!(ensure(&conn).is_err());
            conn.execute_batch("ROLLBACK").unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn root_candidate_index_keeps_non_utf8_and_rebuilds_after_schema_change() {
        use std::os::unix::ffi::OsStringExt;
        let conn = Connection::open_in_memory().unwrap();
        schema(&conn);
        let path = std::path::PathBuf::from(std::ffi::OsString::from_vec(
            b"/synthetic/non-utf8-\xff.jpg".to_vec(),
        ));
        let native = super::super::super::provider_selection::SelectionPath::from_path(&path);
        conn.execute(
            "INSERT INTO download_receipts VALUES('synthetic-index-only',?1)",
            [serde_json::to_string(&native).unwrap()],
        )
        .unwrap();
        COUNTS.with(|counter| counter.set([0; 3]));
        conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        ensure(&conn).unwrap();
        assert_eq!(owner_count(&conn, &path), 1);
        conn.execute_batch("COMMIT").unwrap();
        conn.execute_batch("CREATE TABLE root_test_schema_change(value TEXT)")
            .unwrap();
        conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        ensure(&conn).unwrap();
        assert_eq!(owner_count(&conn, &path), 1);
        assert_eq!(COUNTS.with(|counter| counter.get())[0], 2);
        conn.execute_batch("COMMIT").unwrap();
    }

    #[test]
    fn root_candidate_index_rebuilds_when_cwd_changes() {
        const CHILD: &str = "KEI_ROOT_CACHE_CWD_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "state::db::provider_generations::root_admission_cache::tests::root_candidate_index_rebuilds_when_cwd_changes",
                    "--test-threads=1",
                ])
                .env(CHILD, "1")
                .status()
                .unwrap();
            assert!(result.success());
            return;
        }
        let original = std::env::current_dir().unwrap();
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        std::env::set_current_dir(first.path()).unwrap();
        let conn = Connection::open_in_memory().unwrap();
        schema(&conn);
        conn.execute(
            "INSERT INTO assets VALUES('L','relative','original','photo.jpg')",
            [],
        )
        .unwrap();
        COUNTS.with(|counter| counter.set([0; 3]));
        conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        ensure(&conn).unwrap();
        assert_eq!(owner_count(&conn, &first.path().join("photo.jpg")), 1);
        conn.execute_batch("COMMIT").unwrap();
        std::env::set_current_dir(second.path()).unwrap();
        conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        ensure(&conn).unwrap();
        assert_eq!(owner_count(&conn, &second.path().join("photo.jpg")), 1);
        assert_eq!(owner_count(&conn, &first.path().join("photo.jpg")), 0);
        assert_eq!(COUNTS.with(|counter| counter.get())[0], 2);
        conn.execute_batch("COMMIT").unwrap();
        std::env::set_current_dir(original).unwrap();
    }

    #[test]
    fn root_candidate_index_census_is_amortized_for_unchanged_local_owners() {
        for size in [32, 128] {
            let conn = Connection::open_in_memory().unwrap();
            schema(&conn);
            for n in 0..size {
                conn.execute(
                    "INSERT INTO assets VALUES('L',?1,'original',?2)",
                    params![format!("asset-{n}"), format!("/synthetic/file-{n}.jpg")],
                )
                .unwrap();
            }
            COUNTS.with(|counter| counter.set([0; 3]));
            conn.execute_batch("BEGIN IMMEDIATE").unwrap();
            ensure(&conn).unwrap();
            conn.execute_batch("COMMIT").unwrap();
            for n in 0..size {
                conn.execute_batch("BEGIN IMMEDIATE").unwrap();
                ensure(&conn).unwrap();
                assert_eq!(
                    owner_count(&conn, Path::new(&format!("/synthetic/file-{n}.jpg"))),
                    1
                );
                conn.execute_batch("COMMIT").unwrap();
            }
            assert_eq!(COUNTS.with(|counter| counter.get()), [1, size, 0]);
            eprintln!(
                "root owner workload: size={size}, builds=1, refreshed_owners={size}, manifest_loads=0"
            );
            conn.execute_batch("BEGIN IMMEDIATE").unwrap();
            conn.execute(
                "UPDATE assets SET local_path='/synthetic/replaced.jpg' WHERE id='asset-0'",
                [],
            )
            .unwrap();
            ensure(&conn).unwrap();
            conn.execute_batch("COMMIT").unwrap();
            assert_eq!(COUNTS.with(|counter| counter.get()), [1, size + 1, 0]);
        }
    }
}
