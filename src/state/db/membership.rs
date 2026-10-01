//! Album snapshots, provider relations, and compatibility grouping projection.

use std::collections::BTreeSet;

use async_trait::async_trait;
use chrono::Utc;
use rusqlite::{OptionalExtension, Transaction};

use super::rows::{collect_rows_with_warn, sqlite_placeholders, unique_sorted_strings};
use super::{AlbumMembershipRecord, AssetGroupingRows, MembershipStore, SqliteStateDb};
use crate::state::error::StateError;

/// Project provider relations into the compatibility grouping table. Only
/// explicit relation/container tombstones remove names; an unfinished snapshot
/// retains its prior live rows. External grouping sources are never removed.
pub(super) fn refresh_asset_album_groupings_tx(
    tx: &Transaction<'_>,
    library: &str,
    asset_id: &str,
    previous_name: Option<&str>,
) -> Result<(), StateError> {
    let operation = "refresh_asset_album_groupings";
    let mut stmt = tx
        .prepare_cached(
            "SELECT c.album_name, m.is_deleted OR c.is_deleted \
         FROM asset_album_memberships m JOIN album_containers c \
           ON c.library = m.library AND c.container_id = m.container_id \
         WHERE m.library = ?1 AND m.asset_record_name IN ( \
             SELECT ?2 UNION SELECT asset_record_name FROM legacy_master_state_owners \
             WHERE library = ?1 AND master_record_name = ?2)",
        )
        .map_err(|e| StateError::query(operation, e))?;
    let rows = stmt
        .query_map(rusqlite::params![library, asset_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?))
        })
        .map_err(|e| StateError::query(operation, e))?;
    let mut known = BTreeSet::new();
    let mut live = BTreeSet::new();
    for row in rows {
        let (name, deleted) = row.map_err(|e| StateError::query(operation, e))?;
        known.insert(name.clone());
        if !deleted {
            live.insert(name);
        }
    }
    if let Some(name) = previous_name {
        known.insert(name.to_owned());
    }
    let mut changed = 0;
    for name in known.difference(&live) {
        changed += tx
            .execute(
                "DELETE FROM asset_albums WHERE library = ?1 AND asset_id = ?2 \
             AND album_name = ?3 AND source = 'icloud'",
                rusqlite::params![library, asset_id, name],
            )
            .map_err(|e| StateError::query(operation, e))?;
    }
    for name in live {
        changed += tx
            .execute(
                "INSERT INTO asset_albums (library, asset_id, album_name, source) \
             SELECT ?1, ?2, ?3, 'icloud' WHERE NOT EXISTS ( \
                 SELECT 1 FROM asset_albums WHERE library = ?1 AND asset_id = ?2 \
                 AND album_name = ?3 AND source = 'icloud')",
                rusqlite::params![library, asset_id, name],
            )
            .map_err(|e| StateError::query(operation, e))?;
    }
    if changed > 0 {
        mark_album_groupings_dirty_tx(tx, library, asset_id)?;
    }
    Ok(())
}

fn mark_album_groupings_dirty_tx(
    tx: &Transaction<'_>,
    library: &str,
    asset_id: &str,
) -> Result<(), StateError> {
    tx.execute(
        "UPDATE assets SET metadata_write_failed_at = COALESCE(metadata_write_failed_at, ?3) \
         WHERE library = ?1 AND id = ?2 AND is_deleted = 0",
        rusqlite::params![library, asset_id, Utc::now().timestamp()],
    )
    .map_err(|e| StateError::query("mark_album_groupings_dirty", e))?;
    tx.execute(
        "UPDATE asset_metadata_paths SET metadata_write_failed_at = COALESCE(metadata_write_failed_at, ?3) \
         WHERE library = ?1 AND id = ?2",
        rusqlite::params![library, asset_id, Utc::now().timestamp()],
    ).map_err(|e| StateError::query("mark_album_metadata_paths_dirty", e))?;
    Ok(())
}

// Keep the optional member filter in separate branches so both scopes use
// indexed lookups. IN deduplicates child and legacy-owner identities.
const ALBUM_GROUPING_STATE_IDS_SQL: &str = "WITH members AS MATERIALIZED ( \
    SELECT asset_record_name FROM asset_album_memberships \
    WHERE library = ?1 AND container_id = ?2 AND asset_record_name = ?3 \
    UNION ALL \
    SELECT asset_record_name FROM asset_album_memberships \
    INDEXED BY idx_asset_album_memberships_container \
    WHERE library = ?1 AND container_id = ?2 AND ?3 IS NULL \
) SELECT DISTINCT id FROM assets WHERE library = ?1 AND id IN ( \
    SELECT asset_record_name FROM members \
    UNION ALL \
    SELECT master_record_name FROM legacy_master_state_owners \
    WHERE library = ?1 AND asset_record_name IN (SELECT asset_record_name FROM members))";

fn refresh_container_groupings_tx(
    tx: &Transaction<'_>,
    library: &str,
    container_id: &str,
    asset_record_name: Option<&str>,
    previous_name: Option<&str>,
) -> Result<(), StateError> {
    let operation = "refresh_container_groupings";
    let mut stmt = tx
        .prepare_cached(ALBUM_GROUPING_STATE_IDS_SQL)
        .map_err(|e| StateError::query(operation, e))?;
    let ids = stmt
        .query_map(
            rusqlite::params![library, container_id, asset_record_name],
            |row| row.get::<_, String>(0),
        )
        .map_err(|e| StateError::query(operation, e))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| StateError::query(operation, e))?;
    for id in ids {
        refresh_asset_album_groupings_tx(tx, library, &id, previous_name)?;
    }
    Ok(())
}

fn album_container_known_tx(
    tx: &Transaction<'_>,
    library: &str,
    container_id: &str,
    operation: &'static str,
) -> Result<bool, StateError> {
    tx.query_row(
        "SELECT 1 FROM album_containers \
         WHERE library = ?1 AND container_id = ?2",
        rusqlite::params![library, container_id],
        |_| Ok(()),
    )
    .optional()
    .map(|row| row.is_some())
    .map_err(|e| StateError::query(operation, e))
}

fn album_membership_record_from_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<AlbumMembershipRecord> {
    Ok(AlbumMembershipRecord {
        library: row.get(0)?,
        asset_record_name: row.get(1)?,
        master_record_name: row.get(2)?,
        container_id: row.get(3)?,
        generation: row.get(4)?,
        source: row.get(5)?,
    })
}

impl SqliteStateDb {
    pub(crate) async fn add_asset_album(
        &self,
        library: &str,
        asset_id: &str,
        album_name: &str,
        source: &str,
    ) -> Result<(), StateError> {
        let library = library.to_owned();
        let asset_id = asset_id.to_owned();
        let album_name = album_name.to_owned();
        let source = source.to_owned();
        self.with_conn_mut("add_asset_album", move |conn| {
            let tx = conn
                .transaction()
                .map_err(|e| StateError::query("add_asset_album::begin", e))?;
            let changed = tx
                .execute(
                    "INSERT OR IGNORE INTO asset_albums (library, asset_id, album_name, source) \
                 VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params![&library, &asset_id, album_name, source],
                )
                .map_err(|e| StateError::query("add_asset_album", e))?;
            if changed > 0 {
                mark_album_groupings_dirty_tx(&tx, &library, &asset_id)?;
            }
            tx.commit()
                .map_err(|e| StateError::query("add_asset_album::commit", e))?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn get_all_asset_albums(
        &self,
        library: &str,
    ) -> Result<Vec<(String, String)>, StateError> {
        let library = library.to_owned();
        self.with_conn("get_all_asset_albums", move |conn| {
            let mut stmt = conn
                .prepare_cached("SELECT asset_id, album_name FROM asset_albums WHERE library = ?1")
                .map_err(|e| StateError::query("get_all_asset_albums", e))?;
            let rows = stmt
                .query_map([&library], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(|e| StateError::query("get_all_asset_albums", e))?;
            Ok(collect_rows_with_warn(rows, "get_all_asset_albums"))
        })
        .await
    }

    pub(crate) async fn get_all_asset_people(
        &self,
        library: &str,
    ) -> Result<Vec<(String, String)>, StateError> {
        let library = library.to_owned();
        self.with_conn("get_all_asset_people", move |conn| {
            let mut stmt = conn
                .prepare_cached("SELECT asset_id, person_name FROM asset_people WHERE library = ?1")
                .map_err(|e| StateError::query("get_all_asset_people", e))?;
            let rows = stmt
                .query_map([&library], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(|e| StateError::query("get_all_asset_people", e))?;
            Ok(collect_rows_with_warn(rows, "get_all_asset_people"))
        })
        .await
    }

    pub(crate) async fn get_asset_groupings(
        &self,
        library: &str,
        asset_ids: &[&str],
    ) -> Result<AssetGroupingRows, StateError> {
        let library = library.to_owned();
        let asset_ids = unique_sorted_strings(asset_ids);
        if asset_ids.is_empty() {
            return Ok(AssetGroupingRows::default());
        }
        self.with_conn("get_asset_groupings", move |conn| {
            let placeholders = sqlite_placeholders(asset_ids.len());
            let mut params: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(asset_ids.len() + 1);
            params.push(&library);
            for asset_id in &asset_ids {
                params.push(asset_id);
            }

            let album_sql = format!(
                "SELECT asset_id, album_name FROM asset_albums \
                 WHERE library = ? AND asset_id IN ({placeholders}) \
                 ORDER BY asset_id, album_name"
            );
            let mut album_stmt = conn
                .prepare(&album_sql)
                .map_err(|e| StateError::query("get_asset_groupings", e))?;
            let albums = album_stmt
                .query_map(rusqlite::params_from_iter(params.iter().copied()), |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(|e| StateError::query("get_asset_groupings", e))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| StateError::query("get_asset_groupings", e))?;

            let people_sql = format!(
                "SELECT asset_id, person_name FROM asset_people \
                 WHERE library = ? AND asset_id IN ({placeholders}) \
                 ORDER BY asset_id, person_name"
            );
            let mut people_stmt = conn
                .prepare(&people_sql)
                .map_err(|e| StateError::query("get_asset_groupings", e))?;
            let people = people_stmt
                .query_map(rusqlite::params_from_iter(params), |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(|e| StateError::query("get_asset_groupings", e))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| StateError::query("get_asset_groupings", e))?;

            Ok(AssetGroupingRows { albums, people })
        })
        .await
    }

    pub(crate) async fn upsert_album_container(
        &self,
        library: &str,
        container_id: &str,
        album_name: &str,
        pass_kind: &str,
    ) -> Result<(), StateError> {
        let library = library.to_owned();
        let container_id = container_id.to_owned();
        let album_name = album_name.to_owned();
        let pass_kind = pass_kind.to_owned();
        self.with_conn_mut("upsert_album_container", move |conn| {
            let now = Utc::now().timestamp();
            let tx = conn
                .transaction()
                .map_err(|e| StateError::query("upsert_album_container::begin", e))?;
            let previous_name: Option<String> = tx.query_row(
                "SELECT album_name FROM album_containers WHERE library = ?1 AND container_id = ?2",
                rusqlite::params![&library, &container_id], |row| row.get(0),
            ).optional().map_err(|e| StateError::query("upsert_album_container::name", e))?;
            tx.execute(
                "INSERT INTO album_containers \
                    (library, container_id, album_name, pass_kind, is_deleted, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, 0, ?5) \
                 ON CONFLICT(library, container_id) DO UPDATE SET \
                    album_name = excluded.album_name, \
                    pass_kind = excluded.pass_kind, \
                    is_deleted = 0, \
                    updated_at = excluded.updated_at",
                rusqlite::params![&library, &container_id, album_name, pass_kind, now],
            )
            .map_err(|e| StateError::query("upsert_album_container", e))?;
            refresh_container_groupings_tx(
                &tx,
                &library,
                &container_id,
                None,
                previous_name.as_deref(),
            )?;
            tx.commit()
                .map_err(|e| StateError::query("upsert_album_container::commit", e))?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn mark_album_container_deleted(
        &self,
        library: &str,
        container_id: &str,
    ) -> Result<(), StateError> {
        let library = library.to_owned();
        let container_id = container_id.to_owned();
        self.with_conn_mut("mark_album_container_deleted", move |conn| {
            let now = Utc::now().timestamp();
            let tx = conn
                .transaction()
                .map_err(|e| StateError::query("mark_album_container_deleted::begin", e))?;
            let previous_name: Option<String> = tx.query_row(
                "SELECT album_name FROM album_containers WHERE library = ?1 AND container_id = ?2",
                rusqlite::params![&library, &container_id], |row| row.get(0),
            ).optional().map_err(|e| StateError::query("mark_album_container_deleted::name", e))?;
            tx.execute(
                "UPDATE album_containers \
                 SET is_deleted = 1, updated_at = ?1 \
                 WHERE library = ?2 AND container_id = ?3",
                rusqlite::params![now, &library, &container_id],
            )
            .map_err(|e| StateError::query("mark_album_container_deleted", e))?;
            refresh_container_groupings_tx(
                &tx,
                &library,
                &container_id,
                None,
                previous_name.as_deref(),
            )?;
            tx.commit()
                .map_err(|e| StateError::query("mark_album_container_deleted::commit", e))?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn start_album_membership_snapshot(
        &self,
        library: &str,
        container_id: &str,
        enum_config_hash: Option<&str>,
    ) -> Result<i64, StateError> {
        let library = library.to_owned();
        let container_id = container_id.to_owned();
        let enum_config_hash = enum_config_hash.map(ToOwned::to_owned);
        self.with_conn_mut("start_album_membership_snapshot", move |conn| {
            let now = Utc::now().timestamp();
            let tx = conn
                .transaction()
                .map_err(|e| StateError::query("start_album_membership_snapshot::begin", e))?;
            let generation: i64 = tx
                .query_row(
                    "SELECT COALESCE(MAX(generation), 0) + 1 \
                     FROM album_membership_snapshots \
                     WHERE library = ?1 AND container_id = ?2",
                    rusqlite::params![&library, &container_id],
                    |row| row.get(0),
                )
                .map_err(|e| StateError::query("start_album_membership_snapshot::generation", e))?;
            tx.execute(
                "INSERT INTO album_membership_snapshots \
                    (library, container_id, generation, status, enum_config_hash, started_at) \
                 VALUES (?1, ?2, ?3, 'running', ?4, ?5)",
                rusqlite::params![
                    &library,
                    &container_id,
                    generation,
                    enum_config_hash.as_deref(),
                    now
                ],
            )
            .map_err(|e| StateError::query("start_album_membership_snapshot::insert", e))?;
            tx.commit()
                .map_err(|e| StateError::query("start_album_membership_snapshot::commit", e))?;
            Ok(generation)
        })
        .await
    }

    pub(crate) async fn add_album_membership_to_snapshot(
        &self,
        library: &str,
        container_id: &str,
        generation: i64,
        asset_record_name: &str,
        master_record_name: Option<&str>,
        source: &str,
    ) -> Result<(), StateError> {
        let library = library.to_owned();
        let container_id = container_id.to_owned();
        let asset_record_name = asset_record_name.to_owned();
        let master_record_name = master_record_name.map(ToOwned::to_owned);
        let source = source.to_owned();
        self.with_conn_mut("add_album_membership_to_snapshot", move |conn| {
            let now = Utc::now().timestamp();
            let tx = conn
                .transaction()
                .map_err(|e| StateError::query("add_album_membership_to_snapshot::begin", e))?;
            let snapshot_exists: bool = tx
                .query_row(
                    "SELECT 1 FROM album_membership_snapshots \
                     WHERE library = ?1 AND container_id = ?2 \
                       AND generation = ?3 AND status = 'running'",
                    rusqlite::params![&library, &container_id, generation],
                    |_| Ok(()),
                )
                .optional()
                .map_err(|e| {
                    StateError::query("add_album_membership_to_snapshot::snapshot", e)
                })?
                .is_some();
            if !snapshot_exists {
                return Err(StateError::Invariant {
                    operation: "add_album_membership_to_snapshot",
                    detail: format!(
                        "no running snapshot for library {library} container {container_id} generation {generation}"
                    ),
                });
            }
            tx.execute(
                "INSERT INTO asset_album_memberships \
                    (library, asset_record_name, master_record_name, container_id, generation, \
                     is_deleted, source, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6, ?7) \
                 ON CONFLICT(library, asset_record_name, container_id) DO UPDATE SET \
                    master_record_name = COALESCE(excluded.master_record_name, asset_album_memberships.master_record_name), \
                    generation = excluded.generation, \
                    is_deleted = 0, \
                    source = excluded.source, \
                    updated_at = excluded.updated_at",
                rusqlite::params![
                    &library,
                    &asset_record_name,
                    master_record_name.as_deref(),
                    &container_id,
                    generation,
                    &source,
                    now
                ],
            )
            .map_err(|e| StateError::query("add_album_membership_to_snapshot::upsert", e))?;
            refresh_container_groupings_tx(&tx, &library, &container_id, Some(&asset_record_name), None)?;
            tx.commit()
                .map_err(|e| StateError::query("add_album_membership_to_snapshot::commit", e))?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn upsert_album_membership_delta(
        &self,
        library: &str,
        container_id: &str,
        asset_record_name: &str,
        master_record_name: Option<&str>,
        source: &str,
    ) -> Result<bool, StateError> {
        let library = library.to_owned();
        let container_id = container_id.to_owned();
        let asset_record_name = asset_record_name.to_owned();
        let master_record_name = master_record_name.map(ToOwned::to_owned);
        let source = source.to_owned();
        self.with_conn_mut("upsert_album_membership_delta", move |conn| {
            let now = Utc::now().timestamp();
            let tx = conn
                .transaction()
                .map_err(|e| StateError::query("upsert_album_membership_delta::begin", e))?;
            let container_known = album_container_known_tx(
                &tx,
                &library,
                &container_id,
                "upsert_album_membership_delta::container",
            )?;
            tx.execute(
                "INSERT INTO asset_album_memberships \
                    (library, asset_record_name, master_record_name, container_id, generation, \
                     is_deleted, source, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, 0, 0, ?5, ?6) \
                 ON CONFLICT(library, asset_record_name, container_id) DO UPDATE SET \
                    master_record_name = COALESCE(excluded.master_record_name, asset_album_memberships.master_record_name), \
                    is_deleted = 0, \
                    source = excluded.source, \
                    updated_at = excluded.updated_at",
                rusqlite::params![
                    &library,
                    &asset_record_name,
                    master_record_name.as_deref(),
                    &container_id,
                    &source,
                    now
                ],
            )
            .map_err(|e| StateError::query("upsert_album_membership_delta::upsert", e))?;
            refresh_container_groupings_tx(&tx, &library, &container_id, Some(&asset_record_name), None)?;
            tx.commit()
                .map_err(|e| StateError::query("upsert_album_membership_delta::commit", e))?;
            Ok(container_known)
        })
        .await
    }

    pub(crate) async fn mark_album_membership_deleted(
        &self,
        library: &str,
        container_id: &str,
        asset_record_name: &str,
    ) -> Result<bool, StateError> {
        let library = library.to_owned();
        let container_id = container_id.to_owned();
        let asset_record_name = asset_record_name.to_owned();
        self.with_conn_mut("mark_album_membership_deleted", move |conn| {
            let now = Utc::now().timestamp();
            let tx = conn
                .transaction()
                .map_err(|e| StateError::query("mark_album_membership_deleted::begin", e))?;
            let container_known = album_container_known_tx(
                &tx,
                &library,
                &container_id,
                "mark_album_membership_deleted::container",
            )?;
            tx.execute(
                "UPDATE asset_album_memberships \
                 SET is_deleted = 1, updated_at = ?1 \
                 WHERE library = ?2 AND container_id = ?3 AND asset_record_name = ?4",
                rusqlite::params![now, &library, &container_id, &asset_record_name],
            )
            .map_err(|e| StateError::query("mark_album_membership_deleted::update", e))?;
            refresh_container_groupings_tx(
                &tx,
                &library,
                &container_id,
                Some(&asset_record_name),
                None,
            )?;
            tx.commit()
                .map_err(|e| StateError::query("mark_album_membership_deleted::commit", e))?;
            Ok(container_known)
        })
        .await
    }

    pub(crate) async fn complete_album_membership_snapshot(
        &self,
        library: &str,
        container_id: &str,
        generation: i64,
    ) -> Result<(), StateError> {
        let library = library.to_owned();
        let container_id = container_id.to_owned();
        self.with_conn_mut("complete_album_membership_snapshot", move |conn| {
            let now = Utc::now().timestamp();
            let tx = conn
                .transaction()
                .map_err(|e| StateError::query("complete_album_membership_snapshot::begin", e))?;
            let updated = tx
                .execute(
                    "UPDATE album_membership_snapshots \
                     SET status = 'complete', completed_at = ?1 \
                     WHERE library = ?2 AND container_id = ?3 \
                       AND generation = ?4 AND status = 'running'",
                    rusqlite::params![now, &library, &container_id, generation],
                )
                .map_err(|e| {
                    StateError::query("complete_album_membership_snapshot::snapshot", e)
                })?;
            if updated == 0 {
                return Err(StateError::Invariant {
                    operation: "complete_album_membership_snapshot",
                    detail: format!(
                        "no running snapshot for library {library} container {container_id} generation {generation}"
                    ),
                });
            }
            tx.execute(
                "UPDATE asset_album_memberships \
                 SET is_deleted = 1, updated_at = ?1 \
                 WHERE library = ?2 AND container_id = ?3 \
                   AND generation <> ?4 AND is_deleted = 0",
                rusqlite::params![now, &library, &container_id, generation],
            )
            .map_err(|e| StateError::query("complete_album_membership_snapshot::prune", e))?;
            refresh_container_groupings_tx(&tx, &library, &container_id, None, None)?;
            tx.commit()
                .map_err(|e| StateError::query("complete_album_membership_snapshot::commit", e))?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn invalidate_album_membership_snapshot(
        &self,
        library: &str,
        container_id: &str,
    ) -> Result<(), StateError> {
        let library = library.to_owned();
        let container_id = container_id.to_owned();
        self.with_conn("invalidate_album_membership_snapshot", move |conn| {
            let now = Utc::now().timestamp();
            conn.execute(
                "UPDATE album_membership_snapshots \
                 SET status = 'invalidated', completed_at = COALESCE(completed_at, ?1) \
                 WHERE library = ?2 AND container_id = ?3 \
                   AND status IN ('running', 'complete')",
                rusqlite::params![now, library, container_id],
            )
            .map_err(|e| StateError::query("invalidate_album_membership_snapshot", e))?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn selected_album_containers_have_complete_snapshots(
        &self,
        library: &str,
        container_ids: &[&str],
    ) -> Result<bool, StateError> {
        let container_ids = unique_sorted_strings(container_ids);
        if container_ids.is_empty() {
            return Ok(true);
        }
        let library = library.to_owned();
        let placeholders = sqlite_placeholders(container_ids.len());
        self.with_conn(
            "selected_album_containers_have_complete_snapshots",
            move |conn| {
                let sql = format!(
                    "SELECT COUNT(DISTINCT s.container_id) \
                     FROM album_membership_snapshots s \
                     JOIN album_containers c \
                       ON c.library = s.library AND c.container_id = s.container_id \
                     WHERE s.library = ? AND s.container_id IN ({placeholders}) \
                       AND s.status = 'complete' AND c.is_deleted = 0"
                );
                let mut params: Vec<&dyn rusqlite::ToSql> =
                    Vec::with_capacity(1 + container_ids.len());
                params.push(&library);
                for container_id in &container_ids {
                    params.push(container_id);
                }
                let complete_count: i64 = conn
                    .query_row(&sql, rusqlite::params_from_iter(params), |row| row.get(0))
                    .map_err(|e| {
                        StateError::query(
                            "selected_album_containers_have_complete_snapshots::query",
                            e,
                        )
                    })?;
                Ok(complete_count == container_ids.len() as i64)
            },
        )
        .await
    }

    pub(crate) async fn get_live_selected_album_memberships_for_asset(
        &self,
        library: &str,
        asset_record_name: &str,
        selected_container_ids: &[&str],
    ) -> Result<Vec<AlbumMembershipRecord>, StateError> {
        let selected_container_ids = unique_sorted_strings(selected_container_ids);
        if selected_container_ids.is_empty() {
            return Ok(Vec::new());
        }
        let library = library.to_owned();
        let asset_record_name = asset_record_name.to_owned();
        let placeholders = sqlite_placeholders(selected_container_ids.len());
        self.with_conn(
            "get_live_selected_album_memberships_for_asset",
            move |conn| {
                let sql = format!(
                    "SELECT library, asset_record_name, master_record_name, container_id, \
                            generation, source \
                     FROM asset_album_memberships \
                     WHERE library = ? AND asset_record_name = ? AND is_deleted = 0 \
                       AND container_id IN ({placeholders}) \
                     ORDER BY container_id",
                );
                let mut params: Vec<&dyn rusqlite::ToSql> =
                    Vec::with_capacity(2 + selected_container_ids.len());
                params.push(&library);
                params.push(&asset_record_name);
                for container_id in &selected_container_ids {
                    params.push(container_id);
                }
                let mut stmt = conn.prepare(&sql).map_err(|e| {
                    StateError::query("get_live_selected_album_memberships_for_asset", e)
                })?;
                let rows = stmt
                    .query_map(rusqlite::params_from_iter(params), |row| {
                        album_membership_record_from_row(row)
                    })
                    .map_err(|e| {
                        StateError::query("get_live_selected_album_memberships_for_asset", e)
                    })?;
                Ok(collect_rows_with_warn(
                    rows,
                    "get_live_selected_album_memberships_for_asset",
                ))
            },
        )
        .await
    }
}

#[cfg(test)]
impl SqliteStateDb {
    pub(crate) fn fail_asset_album_writes_for_test(&self) {
        let conn = self.acquire_lock("test_fail_asset_album_writes").unwrap();
        conn.execute_batch(
            "CREATE TEMP TRIGGER fail_asset_album_writes \
             BEFORE INSERT ON asset_albums \
             BEGIN SELECT RAISE(FAIL, 'simulated asset album write failure'); END;",
        )
        .unwrap();
    }

    pub(crate) fn allow_asset_album_writes_for_test(&self) {
        let conn = self.acquire_lock("test_allow_asset_album_writes").unwrap();
        conn.execute_batch("DROP TRIGGER fail_asset_album_writes")
            .unwrap();
    }
}

#[async_trait]
impl MembershipStore for SqliteStateDb {
    async fn add_asset_album(
        &self,
        library: &str,
        asset_id: &str,
        album_name: &str,
        source: &str,
    ) -> Result<(), StateError> {
        SqliteStateDb::add_asset_album(self, library, asset_id, album_name, source).await
    }

    async fn get_all_asset_albums(
        &self,
        library: &str,
    ) -> Result<Vec<(String, String)>, StateError> {
        SqliteStateDb::get_all_asset_albums(self, library).await
    }

    async fn get_all_asset_people(
        &self,
        library: &str,
    ) -> Result<Vec<(String, String)>, StateError> {
        SqliteStateDb::get_all_asset_people(self, library).await
    }

    async fn get_asset_groupings(
        &self,
        library: &str,
        asset_ids: &[&str],
    ) -> Result<AssetGroupingRows, StateError> {
        SqliteStateDb::get_asset_groupings(self, library, asset_ids).await
    }

    async fn upsert_album_container(
        &self,
        library: &str,
        container_id: &str,
        album_name: &str,
        pass_kind: &str,
    ) -> Result<(), StateError> {
        SqliteStateDb::upsert_album_container(self, library, container_id, album_name, pass_kind)
            .await
    }

    async fn mark_album_container_deleted(
        &self,
        library: &str,
        container_id: &str,
    ) -> Result<(), StateError> {
        SqliteStateDb::mark_album_container_deleted(self, library, container_id).await
    }

    async fn start_album_membership_snapshot(
        &self,
        library: &str,
        container_id: &str,
        enum_config_hash: Option<&str>,
    ) -> Result<i64, StateError> {
        SqliteStateDb::start_album_membership_snapshot(
            self,
            library,
            container_id,
            enum_config_hash,
        )
        .await
    }

    async fn add_album_membership_to_snapshot(
        &self,
        library: &str,
        container_id: &str,
        generation: i64,
        asset_record_name: &str,
        master_record_name: Option<&str>,
        source: &str,
    ) -> Result<(), StateError> {
        SqliteStateDb::add_album_membership_to_snapshot(
            self,
            library,
            container_id,
            generation,
            asset_record_name,
            master_record_name,
            source,
        )
        .await
    }

    async fn upsert_album_membership_delta(
        &self,
        library: &str,
        container_id: &str,
        asset_record_name: &str,
        master_record_name: Option<&str>,
        source: &str,
    ) -> Result<bool, StateError> {
        SqliteStateDb::upsert_album_membership_delta(
            self,
            library,
            container_id,
            asset_record_name,
            master_record_name,
            source,
        )
        .await
    }

    async fn mark_album_membership_deleted(
        &self,
        library: &str,
        container_id: &str,
        asset_record_name: &str,
    ) -> Result<bool, StateError> {
        SqliteStateDb::mark_album_membership_deleted(self, library, container_id, asset_record_name)
            .await
    }

    async fn complete_album_membership_snapshot(
        &self,
        library: &str,
        container_id: &str,
        generation: i64,
    ) -> Result<(), StateError> {
        SqliteStateDb::complete_album_membership_snapshot(self, library, container_id, generation)
            .await
    }

    async fn invalidate_album_membership_snapshot(
        &self,
        library: &str,
        container_id: &str,
    ) -> Result<(), StateError> {
        SqliteStateDb::invalidate_album_membership_snapshot(self, library, container_id).await
    }

    async fn selected_album_containers_have_complete_snapshots(
        &self,
        library: &str,
        container_ids: &[&str],
    ) -> Result<bool, StateError> {
        SqliteStateDb::selected_album_containers_have_complete_snapshots(
            self,
            library,
            container_ids,
        )
        .await
    }

    async fn get_live_selected_album_memberships_for_asset(
        &self,
        library: &str,
        asset_record_name: &str,
        selected_container_ids: &[&str],
    ) -> Result<Vec<AlbumMembershipRecord>, StateError> {
        SqliteStateDb::get_live_selected_album_memberships_for_asset(
            self,
            library,
            asset_record_name,
            selected_container_ids,
        )
        .await
    }
}

#[cfg(test)]
mod tests;
