//! A privacy-safe summary of an account's sync state for `support-export`:
//! counts only. It never upgrades, writes, or creates SQLite companions, and
//! it refuses to read a file another process is writing.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};

use crate::state::db::account::AccountOwner;

/// Summarize the account's state file under `directory`.
pub(crate) fn support_summary(directory: &Path, username: &str, realm: &str) -> Value {
    let legacy = directory.join(format!("{}.db", crate::account::namespace(username, realm)));
    let path = super::State::path_for(&legacy);
    if !path.exists() {
        // A previous release's file is upgraded by the next sync, not here.
        return if legacy.exists() {
            json!({"status": "upgrade_pending", "migration_performed": false})
        } else {
            json!({"status": "absent"})
        };
    }
    // Immutable reads create no journal or shared memory and cannot see a
    // WAL, so a non-empty WAL or journal means the snapshot would be stale.
    let journal = companion(&path, "-journal");
    let wal = companion(&path, "-wal");
    if non_empty(&journal) {
        return json!({"status": "active_journal_snapshot_unavailable"});
    }
    if non_empty(&wal) {
        return live_wal();
    }
    let before = stamp(&path);
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
    let owner = AccountOwner::configured(username, realm);
    let bound: Option<String> = conn
        .query_row(
            "SELECT value FROM kv WHERE key = 'account_key'",
            [],
            |row| row.get(0),
        )
        .ok();
    if bound.as_deref() != Some(owner.key()) {
        return json!({"status": "account_owner_unavailable_or_mismatched", "migration_performed": false});
    }
    let count = |sql: &str| conn.query_row(sql, [], |row| row.get::<_, i64>(0)).ok();
    let summary = json!({
        "status": "available",
        "schema_version": count("PRAGMA user_version"),
        "copies_saved": count("SELECT COUNT(*) FROM files WHERE wanted = 1 AND state = 'done'"),
        "copies_owed": count("SELECT COUNT(*) FROM files WHERE wanted = 1 AND state IN ('pending', 'adopt')"),
        "copies_failed": count("SELECT COUNT(*) FROM files WHERE wanted = 1 AND state = 'failed'"),
        "copies_locally_modified": count("SELECT COUNT(*) FROM files WHERE wanted = 1 AND state = 'modified'"),
        "copies_no_longer_wanted": count("SELECT COUNT(*) FROM files WHERE wanted = 0"),
        "zones": count("SELECT COUNT(*) FROM zones"),
        "zones_listed": count("SELECT COUNT(*) FROM zones WHERE listed = 1"),
        "carried_over": count("SELECT COUNT(*) FROM imported"),
        "previous_state_present": legacy.exists(),
    });
    drop(conn);
    // A writer that appeared during the read makes the snapshot unreliable.
    if stamp(&path) != before || non_empty(&journal) {
        return json!({"status": "concurrent_change_snapshot_unavailable"});
    }
    if non_empty(&wal) {
        return live_wal();
    }
    summary
}

fn live_wal() -> Value {
    json!({
        "status": "live_wal_snapshot_unavailable",
        "wal_present": true,
        "snapshot": "not_collected",
        "next_action": "stop_the_sync_process_cleanly_then_export_again_without_a_new_sync",
    })
}

fn companion(path: &Path, suffix: &str) -> PathBuf {
    let mut name = OsString::from(path.as_os_str());
    name.push(suffix);
    PathBuf::from(name)
}

fn non_empty(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|meta| meta.len() > 0)
}

fn stamp(path: &Path) -> Option<(u64, Option<std::time::SystemTime>, u64)> {
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
