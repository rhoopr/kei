//! Strictly read-only bounded support snapshot. Never calls migration or normal open.
use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};
use std::path::Path;

const ROW_LIMIT: u64 = 10_000;

pub(crate) fn collect(directory: &Path, username: &str, realm: &str) -> Value {
    let path = directory.join(format!("{}.db", crate::account::namespace(username, realm)));
    if !path.exists() {
        let legacy = directory.join(format!(
            "{}.db",
            crate::auth::session::sanitize_username(username)
        ));
        return json!({"status": if legacy.exists() { "legacy_namespace_requires_explicit_migration" } else { "unavailable" }});
    }
    // Immutable SQLite reads do not create journals or SHM and cannot see WAL.
    // Refuse a non-empty WAL rather than silently reading a stale main file.
    let before = file_stamp(&path);
    let journal = path.with_extension("db-journal");
    if std::fs::metadata(&journal).is_ok_and(|m| m.len() > 0) {
        return json!({"status": "active_journal_snapshot_unavailable"});
    }
    let wal = path.with_extension("db-wal");
    if std::fs::metadata(&wal).is_ok_and(|m| m.len() > 0) {
        return json!({"status": "live_wal_snapshot_unavailable", "wal_present": true,
            "snapshot": "not_collected", "next_action": "stop_the_sync_process_cleanly_then_export_again_without_a_new_sync",
            "runtime_counts": "see_retained_cycle_stats"});
    }
    let Ok(mut uri) = url::Url::from_file_path(&path) else {
        return json!({"status": "unavailable"});
    };
    uri.set_query(Some("mode=ro&immutable=1"));
    let Ok(conn) = Connection::open_with_flags(
        uri.as_str(),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    ) else {
        return json!({"status": "unreadable"});
    };
    match inspect(&conn) {
        Ok(mut value) => {
            if value.get("status").and_then(Value::as_str) == Some("available")
                && super::account::validate(
                    &conn,
                    &super::account::AccountOwner::configured(username, realm),
                )
                .is_err()
            {
                return json!({"status": "account_owner_unavailable_or_mismatched", "migration_performed": false});
            }
            // Detect a writer that appeared during this immutable snapshot.
            // Without a source lock it cannot be called a live WAL snapshot.
            if before != file_stamp(&path) || std::fs::metadata(&journal).is_ok_and(|m| m.len() > 0)
            {
                return json!({"status": "concurrent_change_snapshot_unavailable"});
            }
            if std::fs::metadata(&wal).is_ok_and(|m| m.len() > 0) {
                value = json!({"status": "live_wal_snapshot_unavailable", "wal_present": true});
            }
            value
        }
        Err(_) => json!({"status": "query_unavailable", "error_text": "excluded"}),
    }
}

fn file_stamp(path: &Path) -> Option<(u64, Option<std::time::SystemTime>, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    #[cfg(unix)]
    let identity = {
        use std::os::unix::fs::MetadataExt;
        meta.ino()
    };
    #[cfg(not(unix))]
    let identity = 0;
    Some((meta.len(), meta.modified().ok(), identity))
}

fn inspect(conn: &Connection) -> rusqlite::Result<Value> {
    conn.execute_batch("BEGIN DEFERRED")?;
    let schema: i32 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if schema != crate::state::schema::SCHEMA_VERSION {
        return Ok(
            json!({"status": if schema < crate::state::schema::SCHEMA_VERSION { "older_schema" } else { "unsupported_schema" },
            "schema_version": schema, "migration_performed": false}),
        );
    }
    let mut counts = serde_json::Map::new();
    let mut statement = conn.prepare(
        "SELECT status,COUNT(*) FROM (SELECT CASE status WHEN 'downloaded' THEN 'downloaded' WHEN 'pending' THEN 'pending' WHEN 'failed' THEN 'failed' WHEN 'policy_excluded' THEN 'policy_excluded' ELSE 'other_status' END AS status FROM assets LIMIT 10001) GROUP BY status",
    )?;
    let rows = statement.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?.unsigned_abs()))
    })?;
    let mut observed = 0u64;
    for row in rows {
        let (status, count) = row?;
        observed = observed.saturating_add(count);
        let key = match status.as_str() {
            "downloaded" => "downloaded",
            "pending" => "pending",
            "failed" => "failed",
            "policy_excluded" => "policy_excluded",
            _ => "other_status",
        };
        let old = counts.get(key).and_then(Value::as_u64).unwrap_or(0);
        counts.insert(key.into(), json!(old.saturating_add(count)));
    }
    let mut tables = serde_json::Map::new();
    // Fixed names only; row scans stop at a tight cap and expose no raw values.
    for table in [
        "owned_temp_files",
        "unresolved_sparse_identities",
        "metadata_capture_retries",
        "reconciliation_paths",
        "unattributed_legacy",
        "unattributed_legacy_paths",
        "provider_work_retries",
        "provider_work_obligations",
        "provider_selection_sources",
        "provider_active_decisions",
        "provider_active_destinations",
        "provider_catalog_debt",
        "primary_layout_operations",
        "primary_layout_preserved",
        "primary_layout_bindings",
    ] {
        let count = conn.query_row(
            &format!("SELECT COUNT(*) FROM (SELECT 1 FROM {table} LIMIT 10001)"),
            [],
            |r| Ok(r.get::<_, i64>(0)?.unsigned_abs()),
        );
        tables.insert(table.into(), match count {
            Ok(n) => json!({"observed_rows": n, "complete": n <= ROW_LIMIT, "omitted": if n > ROW_LIMIT { "unknown" } else { "none" }}),
            Err(_) => json!({"status": "unavailable"}),
        });
    }
    let mut metadata = serde_json::Map::new();
    for key in [
        "last_checkpoint_status",
        "last_recovery_action",
        "last_full_enumeration_reason",
    ] {
        let value = conn
            .query_row(
                "SELECT substr(value,1,128) FROM metadata WHERE key=?1 LIMIT 1",
                [key],
                |r| r.get::<_, String>(0),
            )
            .ok()
            .filter(|v| crate::support::fixed_label(v));
        metadata.insert(
            key.into(),
            value.map_or(
                json!({"status":"unavailable"}),
                |v| json!({"status":"available","value":v}),
            ),
        );
    }
    let captures = capture_progress(conn).unwrap_or_else(|_| json!({"status":"unavailable"}));
    let complete = observed <= ROW_LIMIT
        && tables
            .values()
            .all(|v| v.get("complete").and_then(Value::as_bool) == Some(true))
        && captures.get("complete").and_then(Value::as_bool) == Some(true);
    conn.execute_batch("ROLLBACK")?;
    Ok(
        json!({"status": "available", "schema_version": schema, "migration_performed": false,
        "snapshot": "immutable_main_file_no_nonempty_wal_observed", "wal_present": false,
        "scope": "whole_configured_account_bounded_rows_not_current_selection",
        "row_limit": ROW_LIMIT, "observed_asset_rows": observed, "complete": complete,
        "omitted_rows": if observed > ROW_LIMIT { "unknown" } else { "none" },
        "status_counts": counts, "durable_work_counts": tables, "checkpoint_metadata": metadata, "metadata_capture_progress": captures}),
    )
}

fn capture_progress(conn: &Connection) -> rusqlite::Result<Value> {
    let mut statement=conn.prepare("SELECT CASE library WHEN 'PrimarySync' THEN 'primary' ELSE 'other' END,active_revision,pending_revision,processed_assets,failed_assets FROM metadata_capture_state LIMIT 33")?;
    let rows=statement.query_map([], |r| Ok(json!({
        "scope":r.get::<_,String>(0)?, "active_revision":r.get::<_,i64>(1)?, "pending_revision":r.get::<_,Option<i64>>(2)?,
        "processed_assets":r.get::<_,i64>(3)?, "failed_assets":r.get::<_,i64>(4)?,
    })))?;
    let mut records = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    let complete = records.len() <= 32;
    records.truncate(32);
    Ok(
        json!({"status":"available","scope":"whole_configured_account","max_records":32,"complete":complete,
        "omitted":if complete {"none"} else {"unknown"},"records":records}),
    )
}

#[cfg(test)]
mod tests {
    use super::inspect;
    #[test]
    fn old_and_future_schemas_are_never_upgraded() {
        for schema in [1, crate::state::schema::SCHEMA_VERSION + 1] {
            let conn = rusqlite::Connection::open_in_memory().unwrap();
            conn.pragma_update(None, "user_version", schema).unwrap();
            let result = inspect(&conn).unwrap();
            assert_eq!(result["schema_version"], schema);
            assert_eq!(
                conn.pragma_query_value(None, "user_version", |r| r.get::<_, i32>(0))
                    .unwrap(),
                schema
            );
        }
    }
}
