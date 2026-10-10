//! Conditional recovery of a rendition's unchanged prior current publication.

use std::path::PathBuf;

use rusqlite::{Connection, OptionalExtension, params};
use tokio_util::sync::CancellationToken;

use super::SqliteStateDb;
use super::contracts::PendingPublicationRecord;
use crate::state::error::StateError;
use crate::state::{AssetRecord, VersionSizeKey};

fn read_publication(
    conn: &Connection,
    library: &str,
    id: &str,
    version: VersionSizeKey,
) -> Result<Option<PendingPublicationRecord>, StateError> {
    conn.query_row(
        "SELECT a.status, a.checksum, a.size_bytes, a.filename, a.created_at, a.added_at, \
                a.metadata_hash, a.local_path, a.local_checksum, a.download_checksum, \
                a.downloaded_at, a.download_attempts, a.last_error, \
                p.download_checksum, p.source_checksum \
         FROM assets a JOIN asset_metadata_paths p \
           ON p.library=a.library AND p.id=a.id AND p.version_size=a.version_size \
           AND p.local_path=a.local_path \
         WHERE a.library=?1 AND a.id=?2 AND a.version_size=?3 \
           AND a.status IN ('pending','downloaded') AND a.is_deleted=0 \
           AND a.downloaded_at IS NOT NULL AND a.size_bytes>=0 \
           AND a.checksum<>'' AND p.provider_checksum=a.checksum \
           AND length(a.local_checksum)=64 AND a.download_checksum=a.local_checksum \
           AND p.local_checksum=a.local_checksum \
           AND (p.download_checksum IS NULL OR p.download_checksum=a.download_checksum)",
        params![library, id, version.as_str()],
        |row| {
            Ok(PendingPublicationRecord {
                library: library.to_owned(),
                id: id.to_owned(),
                version_size: version,
                status: row.get(0)?,
                checksum: row.get(1)?,
                size: u64::try_from(row.get::<_, i64>(2)?).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        2,
                        rusqlite::types::Type::Integer,
                        Box::new(error),
                    )
                })?,
                filename: row.get(3)?,
                created_at: row.get(4)?,
                added_at: row.get(5)?,
                metadata_hash: row.get(6)?,
                local_path: PathBuf::from(row.get::<_, String>(7)?),
                local_checksum: row.get(8)?,
                download_checksum: row.get(9)?,
                downloaded_at: row.get(10)?,
                download_attempts: row.get(11)?,
                last_error: row.get(12)?,
                receipt_download_checksum: row.get(13)?,
                source_checksum: row.get(14)?,
            })
        },
    )
    .optional()
    .map_err(StateError::from)
}

fn confined_key(path: &std::path::Path) -> Result<String, StateError> {
    crate::fs_util::confined_path_key(path).map_err(|_private_error| StateError::Invariant {
        operation: "recover_pending_publication",
        detail: "publication ownership contains an invalid confined path".into(),
    })
}

/// Recheck all owners inside the writer transaction, including equivalent path
/// spellings and rows omitted from the current selection. An incomplete index
/// never grants ownership. No filesystem reads occur under the database lock.
fn owns_publication(
    conn: &Connection,
    proof: &PendingPublicationRecord,
) -> Result<bool, StateError> {
    let key = confined_key(&proof.local_path)?;
    let mut catalog = conn.prepare_cached(
        "SELECT library,id,version_size,local_path FROM assets WHERE local_path IS NOT NULL \
         UNION SELECT library,id,version_size,local_path FROM asset_metadata_paths",
    )?;
    let rows = catalog.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    let mut found = false;
    for row in rows {
        let (library, id, version, path) = row?;
        if confined_key(std::path::Path::new(&path))? == key {
            if library != proof.library || id != proof.id || version != proof.version_size.as_str()
            {
                return Ok(false);
            }
            found = true;
        }
    }
    let mut reservations = conn.prepare_cached(
        "SELECT library,id,version_size,destination_path,destination_path_key,provider_checksum,provider_size \
         FROM reconciliation_paths",
    )?;
    let rows = reservations.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, i64>(6)?,
        ))
    })?;
    for row in rows {
        let (library, id, version, path, saved_key, checksum, size) = row?;
        let actual_key = confined_key(std::path::Path::new(&path))?;
        if actual_key != saved_key {
            return Ok(false);
        }
        // A different saved destination for this rendition keeps its exact
        // pending publication policy; the old current path cannot complete it.
        if library == proof.library
            && id == proof.id
            && version == proof.version_size.as_str()
            && actual_key != key
        {
            return Ok(false);
        }
        if actual_key == key
            && (library != proof.library
                || id != proof.id
                || version != proof.version_size.as_str()
                || checksum != proof.checksum
                || u64::try_from(size).ok() != Some(proof.size))
        {
            return Ok(false);
        }
    }
    let protected: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM unattributed_legacy WHERE library=?1 AND asset_id=?2) \
         OR EXISTS(SELECT 1 FROM unattributed_legacy_paths WHERE path_key=?3) \
         OR EXISTS(SELECT 1 FROM asset_verifications WHERE library=?1 AND id=?2 \
                   AND version_size=?4 AND state='unknown')",
        params![proof.library, proof.id, key, proof.version_size.as_str()],
        |row| row.get(0),
    )?;
    Ok(found && !protected)
}

fn identity_matches(
    conn: &Connection,
    proof: &PendingPublicationRecord,
    child: &str,
    master: &str,
) -> Result<bool, StateError> {
    if proof.id != child && proof.id != master {
        return Ok(false);
    }
    let mapped: Option<String> = conn
        .query_row(
            "SELECT master_record_name FROM asset_master_mappings \
         WHERE library=?1 AND asset_record_name=?2",
            params![proof.library, child],
            |row| row.get(0),
        )
        .optional()?;
    if mapped.as_deref().is_some_and(|saved| saved != master) {
        return Ok(false);
    }
    if proof.id != child {
        let owner: Option<String> = conn
            .query_row(
                "SELECT asset_record_name FROM legacy_master_state_owners \
             WHERE library=?1 AND master_record_name=?2",
                params![proof.library, master],
                |row| row.get(0),
            )
            .optional()?;
        return Ok(mapped.as_deref() == Some(master) && owner.as_deref() == Some(child));
    }
    Ok(true)
}

impl SqliteStateDb {
    pub(super) async fn pending_publication(
        &self,
        library: &str,
        id: &str,
        version: VersionSizeKey,
    ) -> Result<Option<PendingPublicationRecord>, StateError> {
        let library = library.to_owned();
        let id = id.to_owned();
        self.with_conn("pending_publication", move |conn| {
            read_publication(conn, &library, &id, version)
        })
        .await
    }

    pub(super) async fn recover_pending_publication(
        &self,
        proof: &PendingPublicationRecord,
        selected: &AssetRecord,
        child: &str,
        master: &str,
        shutdown: &CancellationToken,
    ) -> Result<bool, StateError> {
        let proof = proof.clone();
        let selected = selected.clone();
        let child = child.to_owned();
        let master = master.to_owned();
        let shutdown = shutdown.clone();
        self.with_conn_mut("recover_pending_publication", move |conn| {
            if shutdown.is_cancelled() {
                return Ok(false);
            }
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            if selected.library.as_ref() != proof.library
                || selected.id.as_ref() != proof.id
                || selected.version_size != proof.version_size
                || selected.checksum.as_ref() != proof.checksum
                || selected.size_bytes != proof.size
                || selected.metadata.is_deleted
            {
                return Ok(false);
            }
            let Some(current) =
                read_publication(&tx, &proof.library, &proof.id, proof.version_size)?
            else {
                return Ok(false);
            };
            let mut completed = proof.clone();
            completed.status = "downloaded".into();
            completed.last_error = None;
            if current != proof && current != completed {
                return Ok(false);
            }
            if !owns_publication(&tx, &proof)? || !identity_matches(&tx, &proof, &child, &master)? {
                return Ok(false);
            }
            // A managed handover may claim this path after the reader snapshot.
            // Apply the same transactional generation and local-byte fences as
            // ordinary downloaded-state finalization before clearing retry debt.
            super::primary_layout::guard_slot(
                &tx,
                &proof.library,
                &proof.id,
                proof.version_size.as_str(),
                &proof.checksum,
                &proof.local_path,
            )?;
            super::primary_layout::guard_downloaded_checksum(
                &tx,
                &proof.local_path,
                &proof.local_checksum,
            )?;
            super::asset_writes::ensure_asset_has_no_prepared_capture_repair(
                &tx,
                &proof.library,
                &proof.id,
                "recover_pending_publication",
            )?;
            super::provider_work::guard_projected_generation(&tx, &selected)?;
            super::provider_work::guard_projected_mapping(&tx, &proof.library, &child, &master)?;
            if shutdown.is_cancelled() {
                return Ok(false);
            }
            if current.status == "pending" {
                // This recovers a prior publication. Preserve the path, original
                // publication timestamp, hashes, every receipt and metadata debt.
                let changed = tx.execute(
                    "UPDATE assets SET status='downloaded',last_error=NULL \
                     WHERE library=?1 AND id=?2 AND version_size=?3 AND status='pending'",
                    params![proof.library, proof.id, proof.version_size.as_str()],
                )?;
                if changed != 1 {
                    return Ok(false);
                }
            }
            tx.execute(
                "DELETE FROM asset_verifications WHERE library=?1 AND id=?2 AND version_size=?3 \
                 AND state<>'unknown'",
                params![proof.library, proof.id, proof.version_size.as_str()],
            )?;
            if shutdown.is_cancelled() {
                return Ok(false);
            }
            tx.commit()?;
            Ok(true)
        })
        .await
    }
}

#[cfg(test)]
mod tests;
