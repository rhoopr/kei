//! Library-scoped provider asset/master mappings and legacy state ownership.

use std::collections::{HashMap, HashSet};

use chrono::Utc;
use rusqlite::OptionalExtension;

use super::SqliteStateDb;
use crate::state::error::StateError;

impl SqliteStateDb {
    pub(crate) async fn upsert_asset_master_mapping(
        &self,
        library: &str,
        asset_record_name: &str,
        master_record_name: &str,
    ) -> Result<(), StateError> {
        let library = library.to_owned();
        let asset_record_name = asset_record_name.to_owned();
        let master_record_name = master_record_name.to_owned();
        self.with_conn_mut("upsert_asset_master_mapping", move |conn| {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            upsert_asset_master_mapping_in_tx(
                &tx,
                &library,
                &asset_record_name,
                &master_record_name,
            )?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn get_master_record_name_for_asset(
        &self,
        library: &str,
        asset_record_name: &str,
    ) -> Result<Option<String>, StateError> {
        let library = library.to_owned();
        let asset_record_name = asset_record_name.to_owned();
        self.with_conn("get_master_record_name_for_asset", move |conn| {
            conn.query_row(
                "SELECT master_record_name FROM asset_master_mappings \
                 WHERE library = ?1 AND asset_record_name = ?2",
                rusqlite::params![library, asset_record_name],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| StateError::query("get_master_record_name_for_asset", e))
        })
        .await
    }

    pub(crate) async fn get_asset_record_names_for_master(
        &self,
        library: &str,
        master_record_name: &str,
    ) -> Result<Vec<String>, StateError> {
        let library = library.to_owned();
        let master_record_name = master_record_name.to_owned();
        self.with_conn("get_asset_record_names_for_master", move |conn| {
            let mut stmt = conn
                .prepare_cached(
                    "SELECT asset_record_name FROM asset_master_mappings \
                     WHERE library = ?1 AND master_record_name = ?2 \
                     ORDER BY asset_record_name",
                )
                .map_err(|e| StateError::query("get_asset_record_names_for_master::prepare", e))?;
            let rows = stmt
                .query_map(rusqlite::params![library, master_record_name], |row| {
                    row.get::<_, String>(0)
                })
                .map_err(|e| StateError::query("get_asset_record_names_for_master::query", e))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| StateError::query("get_asset_record_names_for_master::row", e))?;
            Ok(rows)
        })
        .await
    }

    pub(crate) async fn get_asset_master_mappings(
        &self,
    ) -> Result<HashSet<(String, String, String)>, StateError> {
        self.with_conn("get_asset_master_mappings", move |conn| {
            let mut stmt = conn
                .prepare_cached(
                    "SELECT library, asset_record_name, master_record_name \
                     FROM asset_master_mappings",
                )
                .map_err(|e| StateError::query("get_asset_master_mappings::prepare", e))?;
            stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(|e| StateError::query("get_asset_master_mappings::query", e))?
            .collect::<Result<HashSet<_>, _>>()
            .map_err(|e| StateError::query("get_asset_master_mappings::row", e))
        })
        .await
    }

    pub(crate) async fn get_legacy_master_state_owners(
        &self,
    ) -> Result<HashSet<(String, String, String)>, StateError> {
        self.with_conn("get_legacy_master_state_owners", move |conn| {
            let mut stmt = conn
                .prepare_cached(
                    "SELECT library, master_record_name, asset_record_name \
                     FROM legacy_master_state_owners",
                )
                .map_err(|e| StateError::query("get_legacy_master_state_owners::prepare", e))?;
            stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(|e| StateError::query("get_legacy_master_state_owners::query", e))?
            .collect::<Result<HashSet<_>, _>>()
            .map_err(|e| StateError::query("get_legacy_master_state_owners::row", e))
        })
        .await
    }

    pub(crate) async fn claim_legacy_master_state_owner(
        &self,
        library: &str,
        master_record_name: &str,
        asset_record_name: &str,
    ) -> Result<bool, StateError> {
        #[cfg(test)]
        {
            let mut remaining = self
                .legacy_owner_claim_failures
                .load(std::sync::atomic::Ordering::Relaxed);
            let inject_failure = loop {
                if remaining == 0 {
                    break false;
                }
                match self.legacy_owner_claim_failures.compare_exchange_weak(
                    remaining,
                    remaining - 1,
                    std::sync::atomic::Ordering::Relaxed,
                    std::sync::atomic::Ordering::Relaxed,
                ) {
                    Ok(_) => break true,
                    Err(actual) => remaining = actual,
                }
            };
            if inject_failure {
                return Err(StateError::LockPoisoned(
                    "injected legacy owner claim failure".into(),
                ));
            }
        }
        let library = library.to_owned();
        let master_record_name = master_record_name.to_owned();
        let asset_record_name = asset_record_name.to_owned();
        self.with_conn_mut("claim_legacy_master_state_owner", move |conn| {
            let tx = conn.transaction().map_err(|e| {
                StateError::query("claim_legacy_master_state_owner::transaction", e)
            })?;
            let claimed = claim_legacy_master_state_owner_in_tx(
                &tx,
                &library,
                &master_record_name,
                &asset_record_name,
            )?;
            tx.commit()
                .map_err(|e| StateError::query("claim_legacy_master_state_owner::commit", e))?;
            Ok(claimed)
        })
        .await
    }

    pub(crate) async fn backfill_asset_master_mappings_from_album_memberships(
        &self,
    ) -> Result<u64, StateError> {
        self.with_conn(
            "backfill_asset_master_mappings_from_album_memberships",
            move |conn| {
                let now = Utc::now().timestamp();
                let inserted = conn
                    .execute(
                        "INSERT OR IGNORE INTO asset_master_mappings \
                        (library, asset_record_name, master_record_name, updated_at) \
                     SELECT \
                        membership.library, \
                        membership.asset_record_name, \
                        MIN(membership.master_record_name), \
                        ?1 \
                     FROM asset_album_memberships AS membership \
                     WHERE membership.asset_record_name <> '' \
                       AND membership.master_record_name IS NOT NULL \
                       AND membership.master_record_name <> '' \
                       AND NOT EXISTS ( \
                           SELECT 1 \
                           FROM asset_master_mappings AS mapping \
                           WHERE mapping.library = membership.library \
                             AND mapping.asset_record_name = membership.asset_record_name \
                       ) \
                     GROUP BY membership.library, membership.asset_record_name \
                     HAVING COUNT(DISTINCT membership.master_record_name) = 1",
                        rusqlite::params![now],
                    )
                    .map_err(|e| {
                        StateError::query(
                            "backfill_asset_master_mappings_from_album_memberships",
                            e,
                        )
                    })?;
                Ok(inserted as u64)
            },
        )
        .await
    }
}

/// Explicit exclusive legacy owner rows, indexed by library and master.
/// Only `legacy_master_state_owners` proves an old master row's child owner.
/// `asset_master_mappings` is family history, never identity by itself.
#[derive(Clone, Debug, Default)]
pub(crate) struct LegacyOwnersByLibrary {
    children: HashMap<String, HashMap<String, String>>,
}

impl LegacyOwnersByLibrary {
    pub(crate) fn from_rows(rows: HashSet<(String, String, String)>) -> Self {
        let mut children: HashMap<String, HashMap<String, String>> = HashMap::new();
        for (library, master, child) in rows {
            children.entry(library).or_default().insert(master, child);
        }
        Self { children }
    }

    fn child<'a>(&'a self, library: &str, master: &str) -> Option<&'a str> {
        self.children
            .get(library)
            .and_then(|masters| masters.get(master))
            .map(String::as_str)
    }

    #[must_use]
    pub(crate) fn accepts_claim(&self, library: &str, master: &str, child: &str) -> bool {
        self.child(library, master)
            .is_none_or(|owner| owner == child)
    }

    /// Call only after all participating views accept the durable direct claim.
    pub(crate) fn remember_claim(&mut self, library: &str, master: &str, child: &str) {
        self.children
            .entry(library.to_owned())
            .or_default()
            .entry(master.to_owned())
            .or_insert_with(|| child.to_owned());
    }
}

/// One identity rule for baseline matching, planning, and adoption.
///
/// A target is usable only when its state id is its actual child or an explicit
/// legacy owner names that child. An owner is the same logical asset when it is
/// that child, the target's exact state id, or an explicit legacy owner naming
/// the same actual child. Raw master mappings and shared master names never
/// participate.
#[must_use]
pub(crate) fn same_logical_asset(
    library: &str,
    target_state_id: &str,
    actual_child: &str,
    owner_library: &str,
    owner_state_id: &str,
    owners: &LegacyOwnersByLibrary,
) -> bool {
    if library != owner_library {
        return false;
    }
    let target_is_canonical = target_state_id == actual_child
        || owners.child(library, target_state_id) == Some(actual_child);
    target_is_canonical
        && (owner_state_id == actual_child
            || owner_state_id == target_state_id
            || owners.child(library, owner_state_id) == Some(actual_child))
}

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
