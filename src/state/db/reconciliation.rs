//! Durable reconciliation destination reservations and catalogue reads.

use std::path::PathBuf;

use async_trait::async_trait;

use super::{
    ReconciliationCatalogPath, ReconciliationContent, ReconciliationPathKey,
    ReconciliationReservation, ReconciliationStateStore, SqliteStateDb,
};
use crate::state::error::StateError;
use crate::state::types::VersionSizeKey;

#[async_trait]
impl ReconciliationStateStore for SqliteStateDb {
    async fn get_reconciliation_catalog_paths(
        &self,
    ) -> Result<Vec<ReconciliationCatalogPath>, StateError> {
        self.with_conn("get_reconciliation_catalog_paths", move |conn| {
            let mut statement = conn.prepare_cached(
                "SELECT library, id, version_size, local_path FROM assets WHERE local_path IS NOT NULL \
                 UNION SELECT library, id, version_size, local_path FROM asset_metadata_paths",
            )?;
            let rows = statement.query_map([], |row| {
                Ok(ReconciliationCatalogPath {
                    library: row.get::<_, String>(0)?.into(),
                    asset_id: row.get::<_, String>(1)?.into_boxed_str(),
                    version_size: VersionSizeKey::from_str(&row.get::<_, String>(2)?)
                        .ok_or(rusqlite::Error::InvalidQuery)?,
                    path: PathBuf::from(row.get::<_, String>(3)?),
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(StateError::from)
        })
        .await
    }

    async fn get_reconciliation_reservations(
        &self,
    ) -> Result<Vec<ReconciliationReservation>, StateError> {
        self.with_conn("get_reconciliation_reservations", move |conn| {
            let mut statement = conn.prepare_cached(
                "SELECT library, id, version_size, requested_path_key, \
                 destination_path_key, destination_path, provider_checksum, \
                 CASE WHEN provider_size >= 0 THEN provider_size END FROM reconciliation_paths",
            )?;
            let rows = statement.query_map([], |row| {
                let version: String = row.get(2)?;
                Ok(ReconciliationReservation {
                    content: row
                        .get::<_, Option<i64>>(7)?
                        .map(|size| {
                            Ok::<_, rusqlite::Error>(ReconciliationContent {
                                checksum: row.get::<_, String>(6)?.into_boxed_str(),
                                size: u64::try_from(size).map_err(|error| {
                                    rusqlite::Error::FromSqlConversionFailure(
                                        7,
                                        rusqlite::types::Type::Integer,
                                        Box::new(error),
                                    )
                                })?,
                            })
                        })
                        .transpose()?,
                    library: row.get::<_, String>(0)?.into(),
                    asset_id: row.get::<_, String>(1)?.into_boxed_str(),
                    version_size: VersionSizeKey::from_str(&version)
                        .ok_or(rusqlite::Error::InvalidQuery)?,
                    requested_path_key: ReconciliationPathKey(row.get(3)?),
                    destination_path_key: ReconciliationPathKey(row.get(4)?),
                    destination_path: PathBuf::from(row.get::<_, String>(5)?),
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(StateError::from)
        })
        .await
    }

    async fn reserve_reconciliation_paths(
        &self,
        reservations: &[ReconciliationReservation],
    ) -> Result<(), StateError> {
        let reservations = reservations.to_vec();
        self.with_conn("reserve_reconciliation_paths", move |conn| {
            let tx = conn.unchecked_transaction()?;
            for reservation in reservations {
                let destination = reservation.destination_path.to_str().ok_or_else(|| StateError::Invariant {
                    operation: "reserve_reconciliation_paths",
                    detail: "reconciliation destination is not representable in SQLite text".into(),
                })?;
                let provider_checksum = reservation.content.as_ref().map(|content| content.checksum.as_ref());
                let provider_size = reservation.content.as_ref().map(|content| i64::try_from(content.size)).transpose().map_err(|error| StateError::Invariant {
                    operation: "reserve_reconciliation_paths",
                    detail: format!("provider size exceeds SQLite integer range: {error}"),
                })?;
                let foreign_owner: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM reconciliation_paths \
                     WHERE destination_path_key = ?1 AND (library != ?2 OR id != ?3 OR version_size != ?4 \
                     OR provider_checksum != COALESCE(?5, '') OR provider_size != COALESCE(?6, -1)))",
                    rusqlite::params![reservation.destination_path_key.0, reservation.library.as_ref(), reservation.asset_id.as_ref(), reservation.version_size.as_str(), provider_checksum, provider_size],
                    |row| row.get(0),
                )?;
                if foreign_owner {
                    return Err(StateError::Invariant {
                        operation: "reserve_reconciliation_paths",
                        detail: "reconciliation destination belongs to another asset, rendition, or content generation".into(),
                    });
                }
                let changed = tx.execute(
                    "INSERT INTO reconciliation_paths \
                     (library, id, version_size, requested_path_key, destination_path_key, destination_path, provider_checksum, provider_size) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, COALESCE(?7, ''), COALESCE(?8, -1)) \
                     ON CONFLICT(library, id, version_size, requested_path_key, provider_checksum, provider_size) DO UPDATE \
                     SET destination_path = reconciliation_paths.destination_path \
                     WHERE reconciliation_paths.destination_path_key = excluded.destination_path_key",
                    rusqlite::params![reservation.library.as_ref(), reservation.asset_id.as_ref(), reservation.version_size.as_str(), reservation.requested_path_key.0, reservation.destination_path_key.0, destination, provider_checksum, provider_size],
                )?;
                if changed != 1 {
                    return Err(StateError::Invariant {
                        operation: "reserve_reconciliation_paths",
                        detail: "reconciliation retry changed its reserved destination".into(),
                    });
                }
            }
            tx.commit()?;
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
mod tests;
