//! Library-scoped provider asset/master mappings and legacy state ownership.

use chrono::Utc;
use rusqlite::OptionalExtension;

use crate::state::error::StateError;

#[cfg(test)]
mod tests;

/// Claim a compatible legacy owner inside the caller's transaction.
/// Import commits this with adoption, so refusal or failure rolls it back.
pub(super) fn claim_legacy_master_state_owner_in_tx(
    tx: &rusqlite::Transaction<'_>,
    library: &str,
    master_record_name: &str,
    asset_record_name: &str,
) -> Result<bool, StateError> {
    let existing: Option<String> = tx.query_row(
        "SELECT asset_record_name FROM legacy_master_state_owners WHERE library=?1 AND master_record_name=?2",
        rusqlite::params![library, master_record_name], |row| row.get(0),
    ).optional()?;
    if let Some(owner) = existing {
        return Ok(owner == asset_record_name);
    }
    let conflicting_history: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM asset_master_mappings WHERE library=?1 AND master_record_name=?2 AND asset_record_name<>?3) \
         OR (SELECT COUNT(DISTINCT added_at)>1 OR (COUNT(*)>1 AND COUNT(added_at)<>COUNT(*)) FROM assets WHERE library=?1 AND id=?2 AND is_deleted=0)",
        rusqlite::params![library, master_record_name, asset_record_name], |row| row.get(0),
    )?;
    if conflicting_history {
        return Ok(false);
    }
    tx.execute(
        "INSERT OR IGNORE INTO legacy_master_state_owners \
            (library, master_record_name, asset_record_name, claimed_at) \
         VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![
            library,
            master_record_name,
            asset_record_name,
            Utc::now().timestamp()
        ],
    )
    .map_err(|e| StateError::query("claim_legacy_master_state_owner::insert", e))?;
    let owner: String = tx
        .query_row(
            "SELECT asset_record_name FROM legacy_master_state_owners \
             WHERE library = ?1 AND master_record_name = ?2",
            rusqlite::params![library, master_record_name],
            |row| row.get(0),
        )
        .map_err(|e| StateError::query("claim_legacy_master_state_owner::query", e))?;
    Ok(owner == asset_record_name)
}

/// Retain provider identity with the caller's atomic state transition.
pub(super) fn upsert_asset_master_mapping_in_tx(
    tx: &rusqlite::Transaction<'_>,
    library: &str,
    asset_record_name: &str,
    master_record_name: &str,
) -> Result<(), StateError> {
    super::provider_work::guard_projected_mapping(
        tx,
        library,
        asset_record_name,
        master_record_name,
    )?;
    let now = Utc::now().timestamp();
    tx.execute(
        "INSERT INTO asset_master_mappings \
            (library, asset_record_name, master_record_name, updated_at) \
         VALUES (?1, ?2, ?3, ?4) \
         ON CONFLICT(library, asset_record_name) DO UPDATE SET \
            master_record_name = excluded.master_record_name, \
            updated_at = excluded.updated_at",
        rusqlite::params![library, asset_record_name, master_record_name, now],
    )
    .map_err(|e| StateError::query("upsert_asset_master_mapping", e))?;
    Ok(())
}
