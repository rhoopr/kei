//! Atomic import adoption and imported-record reads.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use chrono::Utc;

use super::asset_writes::{
    record_metadata_capture_revision, update_status_to_downloaded, upsert_asset_row,
};
use super::{ImportStateStore, ImportedRecord, SqliteStateDb};
use crate::state::error::StateError;
use crate::state::types::{AssetRecord, METADATA_CAPTURE_REVISION};

impl SqliteStateDb {
    pub(crate) async fn import_adopt(
        &self,
        record: &AssetRecord,
        local_path: &Path,
        local_checksum: &str,
        imported_size: u64,
        imported_mtime: Option<i64>,
    ) -> Result<(), StateError> {
        let record = record.clone();
        let local_path = local_path.to_path_buf();
        let local_checksum = local_checksum.to_owned();

        self.with_conn_mut("import_adopt", move |conn| {
            let tx = conn
                .transaction()
                .map_err(|e| StateError::query("import_adopt::begin", e))?;

            let has_reservations: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM reconciliation_paths)",
                [],
                |row| row.get(0),
            )?;
            if has_reservations {
                // Check before any catalog or metadata mutation, in the same transaction.
                let path_key = crate::fs_util::confined_path_key(&local_path).map_err(|error| {
                    StateError::Invariant {
                        operation: "import_adopt",
                        detail: format!("invalid import destination: {error}"),
                    }
                })?;
                let foreign_owner: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM reconciliation_paths \
                     WHERE destination_path_key = ?1 AND (library != ?2 OR id != ?3 OR version_size != ?4 \
                     OR provider_checksum != ?5 OR ?6 IS NULL OR provider_size != ?6))",
                    rusqlite::params![
                        path_key, record.library.as_ref(), record.id.as_ref(),
                        record.version_size.as_str(), record.checksum.as_ref(),
                        i64::try_from(record.size_bytes).ok(),
                    ],
                    |row| row.get(0),
                )?;
                if foreign_owner {
                    return Err(StateError::Invariant {
                        operation: "import_adopt",
                        detail: "import destination is reserved for another asset, rendition, or content generation".into(),
                    });
                }
            }

            let now = Utc::now().timestamp();
            upsert_asset_row(&tx, &record, now)?;
            let rows = update_status_to_downloaded(
                &tx,
                &record.library,
                &record.id,
                record.version_size.as_str(),
                &local_path,
                &local_checksum,
                None,
                false,
                now,
            )?;
            debug_assert_eq!(
                rows, 1,
                "import_adopt UPDATE missed the row inserted by UPSERT in the same tx — SQL bug"
            );
            record_metadata_capture_revision(
                &tx,
                &record.library,
                &record.id,
                METADATA_CAPTURE_REVISION,
                now,
            )?;

            // Snapshot on-disk size + mtime so the next import-existing run
            // can short-circuit the SHA-256 re-read when the file is
            // unchanged. Done as a separate UPDATE (rather than rolled into
            // `update_status_to_downloaded`) because the production download
            // path doesn't have these values and shouldn't carry them.
            tx.execute(
                "UPDATE assets SET imported_size = ?1, imported_mtime = ?2 \
                 WHERE library = ?3 AND id = ?4 AND version_size = ?5",
                rusqlite::params![
                    i64::try_from(imported_size).unwrap_or(i64::MAX),
                    imported_mtime,
                    &record.library,
                    &record.id,
                    record.version_size.as_str(),
                ],
            )
            .map_err(|e| StateError::query("import_adopt::imported_meta", e))?;

            tx.commit()
                .map_err(|e| StateError::query("import_adopt::commit", e))?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn get_all_imported_records(
        &self,
        library: &str,
    ) -> Result<HashMap<(String, String), ImportedRecord>, StateError> {
        let library = library.to_owned();

        self.with_conn("get_all_imported_records", move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id, version_size, local_path, local_checksum, \
                            imported_size, imported_mtime \
                     FROM assets \
                     WHERE library = ?1 AND status = 'downloaded'",
                )
                .map_err(|e| StateError::query("get_all_imported_records::prepare", e))?;
            let rows = stmt
                .query_map([&library], |row| {
                    let id: String = row.get(0)?;
                    let version_size: String = row.get(1)?;
                    let local_path: String = row.get(2)?;
                    let local_checksum: String = row.get(3)?;
                    let imported_size: Option<i64> = row.get(4)?;
                    let imported_mtime: Option<i64> = row.get(5)?;
                    Ok((
                        (id, version_size),
                        ImportedRecord {
                            local_path: PathBuf::from(local_path),
                            local_checksum,
                            imported_size: imported_size.and_then(|v| u64::try_from(v).ok()),
                            imported_mtime,
                        },
                    ))
                })
                .map_err(|e| StateError::query("get_all_imported_records::query", e))?;
            let mut out = HashMap::new();
            for r in rows {
                let (k, v) =
                    r.map_err(|e| StateError::query("get_all_imported_records::row", e))?;
                out.insert(k, v);
            }
            Ok(out)
        })
        .await
    }
}

#[async_trait]
impl ImportStateStore for SqliteStateDb {
    async fn import_adopt(
        &self,
        record: &AssetRecord,
        local_path: &Path,
        local_checksum: &str,
        imported_size: u64,
        imported_mtime: Option<i64>,
    ) -> Result<(), StateError> {
        SqliteStateDb::import_adopt(
            self,
            record,
            local_path,
            local_checksum,
            imported_size,
            imported_mtime,
        )
        .await
    }

    async fn get_all_imported_records(
        &self,
        library: &str,
    ) -> Result<HashMap<(String, String), ImportedRecord>, StateError> {
        SqliteStateDb::get_all_imported_records(self, library).await
    }
}

#[cfg(test)]
mod tests;
