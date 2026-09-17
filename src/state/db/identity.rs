//! Library-scoped provider asset/master mappings and legacy state ownership.

use std::collections::HashSet;

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
        self.with_conn("upsert_asset_master_mapping", move |conn| {
            let now = Utc::now().timestamp();
            conn.execute(
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
            tx.commit()
                .map_err(|e| StateError::query("claim_legacy_master_state_owner::commit", e))?;
            Ok(owner == asset_record_name)
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

#[cfg(test)]
mod tests;
