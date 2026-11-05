//! Durable reconciliation destination reservations and catalogue reads.

use std::path::PathBuf;

use async_trait::async_trait;

use super::identity::{LegacyOwnersByLibrary, same_logical_asset};
use super::{
    ReconciliationCatalogPath, ReconciliationContent, ReconciliationPathKey,
    ReconciliationReservation, ReconciliationStateStore, SqliteStateDb,
};
use crate::state::error::StateError;
use crate::state::types::VersionSizeKey;

#[async_trait]
impl ReconciliationStateStore for SqliteStateDb {
    #[cfg(test)]
    async fn primary_layout_binding(
        &self,
        family: String,
    ) -> Result<Option<super::primary_layout::LayoutBinding>, StateError> {
        self.primary_layout_binding(family).await
    }
    async fn primary_layout_operations(
        &self,
        library: String,
    ) -> Result<Vec<super::primary_layout::LayoutOperation>, StateError> {
        self.primary_layout_operations(library).await
    }
    #[cfg(test)]
    async fn begin_primary_layout(
        &self,
        op: super::primary_layout::LayoutOperation,
    ) -> Result<(), StateError> {
        self.begin_primary_layout(op).await
    }

    async fn capture_layout_recovery(
        &self,
        op: super::primary_layout::LayoutOperation,
    ) -> Result<Option<super::primary_layout::LayoutRecoverySnapshot>, StateError> {
        self.capture_layout_recovery(op).await
    }
    async fn save_primary_layout_with_recovery(
        &self,
        op: super::primary_layout::LayoutOperation,
        proof: super::primary_layout::LayoutRecoverySnapshot,
    ) -> Result<(), StateError> {
        self.save_primary_layout_with_recovery(op, Some(proof))
            .await
    }
    async fn commit_primary_layout_with_recovery(
        &self,
        op: super::primary_layout::LayoutOperation,
        proof: super::primary_layout::LayoutRecoverySnapshot,
        validate_files: Option<Box<dyn FnOnce() -> Result<(), StateError> + Send>>,
    ) -> Result<(), StateError> {
        self.commit_primary_layout_with_recovery(op, Some(proof), validate_files)
            .await
    }
    async fn save_primary_layout(
        &self,
        op: super::primary_layout::LayoutOperation,
    ) -> Result<(), StateError> {
        self.save_primary_layout(op).await
    }
    async fn commit_primary_layout(
        &self,
        op: super::primary_layout::LayoutOperation,
    ) -> Result<(), StateError> {
        self.commit_primary_layout(op).await
    }
    async fn has_primary_layouts(&self, library: &str) -> Result<bool, StateError> {
        let library = library.to_owned();
        self.with_conn("has_primary_layouts",move |conn| {
            Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM primary_layout_bindings WHERE library=?1 OR source_library=?1) OR EXISTS(SELECT 1 FROM primary_layout_operations WHERE source_library=?1 AND phase NOT IN ('committed','cancelled'))",[library],|r|r.get(0))?)
        }).await
    }

    async fn guard_primary_slot(
        &self,
        library: &str,
        child: &str,
        version: &str,
        checksum: &str,
        path: &std::path::Path,
    ) -> Result<(), StateError> {
        let (library, child, version, checksum, path) = (
            library.to_owned(),
            child.to_owned(),
            version.to_owned(),
            checksum.to_owned(),
            path.to_owned(),
        );
        self.with_conn("guard_primary_slot", move |conn| {
            super::primary_layout::guard_slot(conn, &library, &child, &version, &checksum, &path)
        })
        .await
    }

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
            let mut reservations = rows.collect::<Result<Vec<_>, _>>()?;
            reservations
                .extend(super::provider_generations::pending_destination_reservations(conn)?);
            Ok(reservations)
        })
        .await
    }

    async fn reserve_reconciliation_paths(
        &self,
        reservations: &[ReconciliationReservation],
        actual_child: Option<&str>,
    ) -> Result<(), StateError> {
        self.reserve_paths(reservations, actual_child, |_, _| Ok(()))
            .await
    }
}

impl SqliteStateDb {
    /// Confirm exact storage intent, not pass provenance or completed media.
    pub(crate) async fn publication_choice_is_persisted(
        &self,
        binding: super::PublicationBinding,
        reservation: ReconciliationReservation,
        requested: super::provider_selection::SelectionPath,
    ) -> Result<bool, StateError> {
        self.with_conn("read exact publication choice", move |conn| {
            binding.validate(conn)?;
            let invalid = || StateError::ProviderWorkConflict;
            let content = reservation.content.as_ref().ok_or_else(invalid)?;
            let size = i64::try_from(content.size).map_err(|_error| invalid())?;
            if reservation.library.as_ref() != binding.library
                || reservation.asset_id.as_ref() != binding.state_id
                || content.checksum.trim().is_empty()
                || crate::fs_util::confined_path_key(&requested.to_path())
                    .map_err(|_error| invalid())?
                    != reservation.requested_path_key.0
                || crate::fs_util::confined_path_key(&reservation.destination_path)
                    .map_err(|_error| invalid())?
                    != reservation.destination_path_key.0
            {
                return Err(invalid());
            }
            let mut statement = conn.prepare_cached(
                "SELECT destination_path_key,destination_path,provider_checksum,provider_size \
                 FROM reconciliation_paths WHERE library=?1 AND id=?2 AND version_size=?3 \
                 AND requested_path_key=?4 AND ((provider_checksum=?5 AND provider_size=?6) \
                 OR trim(provider_checksum)='' OR provider_size<0) LIMIT 2",
            )?;
            let rows = statement
                .query_map(
                    rusqlite::params![
                        reservation.library.as_ref(),
                        reservation.asset_id.as_ref(),
                        reservation.version_size.as_str(),
                        reservation.requested_path_key.0,
                        content.checksum.as_ref(),
                        size
                    ],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, i64>(3)?,
                        ))
                    },
                )?
                .collect::<Result<Vec<_>, _>>()?;
            match rows.as_slice() {
                [] => Ok(false),
                [(key, native, checksum, saved_size)] => {
                    let native = PathBuf::from(native);
                    if checksum != content.checksum.as_ref()
                        || *saved_size != size
                        || key != &reservation.destination_path_key.0
                        || !native.is_absolute()
                        || native != reservation.destination_path
                        || crate::fs_util::confined_path_key(&native).map_err(|_error| invalid())?
                            != *key
                    {
                        return Err(invalid());
                    }
                    Ok(true)
                }
                _ => Err(invalid()),
            }
        })
        .await
    }

    pub(crate) async fn reserve_bound_publication_choices(
        &self,
        binding: super::PublicationBinding,
        choices: Vec<(
            ReconciliationReservation,
            super::provider_selection::SelectionPath,
        )>,
    ) -> Result<(), StateError> {
        let (reservations, paths): (Vec<_>, Vec<_>) = choices.into_iter().unzip();
        let mut paths = paths.into_iter();
        let child = binding.child.clone();
        self.reserve_paths(&reservations, Some(&child), move |conn, reservation| {
            let requested = paths.next().ok_or(StateError::ProviderSelectionInvalid)?;
            super::provider_generations::guard_publication_choice(
                conn,
                &binding,
                reservation,
                &requested.to_path(),
            )
        })
        .await
    }

    async fn reserve_paths<F>(
        &self,
        reservations: &[ReconciliationReservation],
        actual_child: Option<&str>,
        mut guard: F,
    ) -> Result<(), StateError>
    where
        F: FnMut(&rusqlite::Connection, &ReconciliationReservation) -> Result<(), StateError>
            + Send
            + 'static,
    {
        let reservations = reservations.to_vec();
        let actual_child = actual_child.map(str::to_owned);
        self.with_conn("reserve_reconciliation_paths", move |conn| {
            let tx = conn.unchecked_transaction()?;
            for reservation in reservations {
                guard(&tx, &reservation)?;
                let destination = reservation.destination_path.to_str().ok_or_else(|| StateError::Invariant {
                    operation: "reserve_reconciliation_paths",
                    detail: "reconciliation destination is not representable in SQLite text".into(),
                })?;
                let provider_checksum = reservation.content.as_ref().map(|content| content.checksum.as_ref());
                let provider_size = reservation.content.as_ref().map(|content| i64::try_from(content.size)).transpose().map_err(|error| StateError::Invariant {
                    operation: "reserve_reconciliation_paths",
                    detail: format!("provider size exceeds SQLite integer range: {error}"),
                })?;
                let foreign_owner = if let Some(child) = actual_child.as_deref() {
                    let mut claim_statement = tx.prepare_cached(
                        "SELECT master_record_name,asset_record_name \
                         FROM legacy_master_state_owners WHERE library=?1",
                    )?;
                    let claims = claim_statement
                        .query_map(rusqlite::params![reservation.library.as_ref()], |row| {
                            Ok((
                                reservation.library.to_string(),
                                row.get::<_, String>(0)?,
                                row.get::<_, String>(1)?,
                            ))
                        })?
                        .collect::<Result<std::collections::HashSet<_>, _>>()?;
                    let owners = LegacyOwnersByLibrary::from_rows(claims);
                    if !same_logical_asset(
                        reservation.library.as_ref(),
                        reservation.asset_id.as_ref(),
                        child,
                        reservation.library.as_ref(),
                        reservation.asset_id.as_ref(),
                        &owners,
                    ) {
                        return Err(StateError::Invariant {
                            operation: "reserve_reconciliation_paths",
                            detail: "reservation state identity does not own the actual child".into(),
                        });
                    }
                    let mut existing = tx.prepare_cached(
                        "SELECT library,id,version_size,provider_checksum,provider_size \
                         FROM reconciliation_paths WHERE destination_path_key=?1",
                    )?;
                    let rows = existing.query_map(
                        rusqlite::params![reservation.destination_path_key.0],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, String>(2)?,
                                row.get::<_, String>(3)?,
                                row.get::<_, i64>(4)?,
                            ))
                        },
                    )?;
                    let mut foreign = false;
                    for row in rows {
                        let (library, state_id, version, checksum, size) = row?;
                        if library != reservation.library.as_ref()
                            || version != reservation.version_size.as_str()
                            || checksum.as_str() != provider_checksum.unwrap_or_default()
                            || size != provider_size.unwrap_or(-1)
                            || !same_logical_asset(
                                reservation.library.as_ref(),
                                reservation.asset_id.as_ref(),
                                child,
                                &library,
                                &state_id,
                                &owners,
                            )
                        {
                            foreign = true;
                            break;
                        }
                    }
                    foreign
                } else {
                    tx.query_row(
                        "SELECT EXISTS(SELECT 1 FROM reconciliation_paths \
                         WHERE destination_path_key = ?1 AND (library != ?2 OR id != ?3 OR version_size != ?4 \
                         OR provider_checksum != COALESCE(?5, '') OR provider_size != COALESCE(?6, -1)))",
                        rusqlite::params![reservation.destination_path_key.0, reservation.library.as_ref(), reservation.asset_id.as_ref(), reservation.version_size.as_str(), provider_checksum, provider_size],
                        |row| row.get(0),
                    )?
                };
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
