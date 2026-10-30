//! Verified first-publication evidence retained by the existing temporary ledger.

use std::path::Path;

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::{OwnedTempPathKey, SqliteStateDb};
use crate::state::db::provider_selection::SelectionPath;
use crate::state::{VersionSizeKey, error::StateError};

/// Authenticated source and selected pass for a fresh publication. This is
/// carried only from the selected album owner, never inferred from old hashes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PublicationBinding {
    pub(crate) account_key: String,
    pub(crate) provider_key: String,
    pub(crate) scope: String,
    pub(crate) library: String,
    pub(crate) child: String,
    pub(crate) master: String,
    pub(crate) state_id: String,
    pub(crate) origin_pass: String,
}

impl PublicationBinding {
    fn validate(&self, conn: &rusqlite::Connection) -> Result<(), StateError> {
        let owner: (String, String) = conn.query_row(
            "SELECT account_key,provider_key FROM account_owner WHERE singleton=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if owner != (self.account_key.clone(), self.provider_key.clone())
            || [
                &self.library,
                &self.child,
                &self.master,
                &self.state_id,
                &self.origin_pass,
            ]
            .iter()
            .any(|value| value.trim().is_empty())
        {
            return Err(invalid());
        }
        let scope: serde_json::Value =
            serde_json::from_str(&self.scope).map_err(|_redacted_error| invalid())?;
        if !matches!(
            scope.get("database").and_then(serde_json::Value::as_str),
            Some("private" | "shared")
        ) || scope
            .pointer("/zone/zoneName")
            .and_then(serde_json::Value::as_str)
            != Some(self.library.as_str())
            || scope
                .pointer("/zone/ownerRecordName")
                .and_then(serde_json::Value::as_str)
                .is_none_or(|v| v.trim().is_empty())
        {
            return Err(invalid());
        }
        super::super::provider_generations::guard_library_scope(conn, &self.scope, &self.library)
    }
}

/// Exact current original resource and retained-file fingerprint supplied by the
/// caller. The fingerprint must come from a confined, no-follow file read.
pub(crate) struct CrossRenditionOriginal {
    pub(crate) path: SelectionPath,
    pub(crate) provider_checksum: String,
    pub(crate) provider_size: u64,
    pub(crate) local_checksum: Option<String>,
}

/// Evidence of an original occupant, not evidence of the adjusted rendition.
/// Callers must revalidate the receipt and retained file at the eventual commit.
pub(crate) struct CrossRenditionOccupant {
    pub(crate) receipt: super::super::download_receipts::DownloadReceipt,
}

impl SqliteStateDb {
    /// Read-only proof for the original occupying an adjusted destination. This
    /// does not reserve a path, authorize adoption, or validate a later commit.
    pub(crate) async fn bound_cross_rendition_original(
        &self,
        binding: PublicationBinding,
        requested_version: VersionSizeKey,
        original: CrossRenditionOriginal,
    ) -> Result<Option<CrossRenditionOccupant>, StateError> {
        self.with_conn("read cross-rendition original receipt", move |conn| {
            binding.validate(conn)?;
            if requested_version != VersionSizeKey::Adjusted {
                return Ok(None);
            }
            let Some(fingerprint) = original.local_checksum.as_deref() else {
                return Ok(None);
            };
            let mut occupants = super::super::download_receipts::historical(
                conn,
                &binding.scope,
                &binding.child,
                &binding.master,
                "original",
            )?
            .into_iter()
            .filter(|receipt| receipt.native_path == original.path);
            let Some(receipt) = occupants.next() else {
                return Ok(None);
            };
            if occupants.next().is_some()
                || receipt.account_key != binding.account_key
                || receipt.provider_key != binding.provider_key
                || receipt.library != binding.library
                || receipt.state_id != binding.state_id
                || !receipt.fulfills_pass(&binding.origin_pass)
                || receipt.provider_checksum != original.provider_checksum
                || receipt.provider_size != original.provider_size
                || receipt.local_checksum != fingerprint
            {
                return Ok(None);
            }
            Ok(Some(CrossRenditionOccupant { receipt }))
        })
        .await
    }

    pub(crate) async fn bound_download_receipts(
        &self,
        binding: PublicationBinding,
        version: String,
        checksum: String,
        size: u64,
    ) -> Result<Vec<super::super::download_receipts::DownloadReceipt>, StateError> {
        self.with_conn("read bound completed downloads", move |conn| {
            binding.validate(conn)?;
            Ok(super::super::download_receipts::matching(
                conn,
                &binding.scope,
                &binding.child,
                &binding.master,
                &version,
                &checksum,
                size,
            )?
            .into_iter()
            .filter(|receipt| {
                receipt.library == binding.library
                    && receipt.state_id == binding.state_id
                    && receipt.fulfills_pass(&binding.origin_pass)
            })
            .collect())
        })
        .await
    }

    /// Old completed resources can identify their own unchanged occupants only.
    pub(crate) async fn bound_historical_downloads(
        &self,
        binding: PublicationBinding,
        version: String,
        checksum: String,
        size: u64,
    ) -> Result<Vec<super::super::download_receipts::DownloadReceipt>, StateError> {
        self.with_conn("read bound historical downloads", move |conn| {
            binding.validate(conn)?;
            Ok(super::super::download_receipts::historical(
                conn,
                &binding.scope,
                &binding.child,
                &binding.master,
                &version,
            )?
            .into_iter()
            .filter(|receipt| {
                receipt.library == binding.library
                    && receipt.state_id == binding.state_id
                    && receipt.fulfills_pass(&binding.origin_pass)
                    && (receipt.provider_checksum != checksum || receipt.provider_size != size)
            })
            .collect())
        })
        .await
    }

    pub(crate) async fn reuse_bound_download(
        &self,
        binding: PublicationBinding,
        receipt: super::super::download_receipts::DownloadReceipt,
        record: crate::state::AssetRecord,
        validate_file: Box<dyn FnOnce() -> Result<(), StateError> + Send>,
        authorization: super::super::provider_generations::ReuseAuthorization,
    ) -> Result<bool, StateError> {
        self.with_conn_mut("reuse bound completed download", move |conn| {
            let tx=conn.transaction()?;
            binding.validate(&tx)?;
            if receipt.scope != binding.scope || receipt.library != binding.library
                || receipt.child != binding.child || receipt.master != binding.master
                || receipt.state_id != binding.state_id || !receipt.fulfills_pass(&binding.origin_pass)
                || record.library.as_ref()!=receipt.library || record.id.as_ref()!=receipt.state_id
                || record.version_size.as_str()!=receipt.version_size
                || record.checksum.as_ref()!=receipt.provider_checksum || record.size_bytes!=receipt.provider_size
                || super::super::download_receipts::load(&tx,&receipt.id()?)?.as_ref()!=Some(&receipt)
            { return Err(invalid()); }
            let current: Option<(String,i64,bool)> = tx.query_row(
                "SELECT checksum,size_bytes,is_deleted FROM assets WHERE library=?1 AND id=?2 AND version_size=?3",
                params![receipt.library,receipt.state_id,receipt.version_size],
                |row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).optional()?;
            if current.as_ref().is_none_or(|(checksum,size,deleted)|
                checksum!=&receipt.provider_checksum || u64::try_from(*size).ok()!=Some(receipt.provider_size) || *deleted)
            { return Err(invalid()); }
            let path=receipt.native_path.to_path();
            let mapped:Option<String>=tx.query_row(
                "SELECT master_record_name FROM asset_master_mappings WHERE library=?1 AND asset_record_name=?2",
                params![receipt.library,receipt.child],|row|row.get(0)).optional()?;
            if mapped.as_deref().is_some_and(|master|master!=receipt.master) {return Err(invalid());}
            let chosen=super::super::primary_layout::path_key(&path)?;
            let collision_key=crate::fs_util::confined_path_key(&path).map_err(|_redacted_error|invalid())?;
            let foreign:bool=tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM reconciliation_paths WHERE destination_path_key=?1
                 AND (library<>?2 OR id<>?3 OR version_size<>?4 OR provider_checksum<>?5 OR provider_size<>?6))",
                params![collision_key,receipt.library,receipt.state_id,receipt.version_size,
                    receipt.provider_checksum,i64::try_from(receipt.provider_size).map_err(|_redacted_error|invalid())?],
                |row|row.get(0))?;
            if foreign {return Err(invalid());}
            for reservation in super::super::provider_generations::pending_destination_reservations_for_path(&tx, &path)? {
                if super::super::primary_layout::path_key(&reservation.destination_path)?==chosen
                    && (reservation.library.as_ref()!=receipt.library
                        || reservation.asset_id.as_ref()!=receipt.state_id
                        || reservation.version_size.as_str()!=receipt.version_size
                        || reservation.content.as_ref().is_none_or(|content|
                            content.checksum.as_ref()!=receipt.provider_checksum || content.size!=receipt.provider_size))
                {return Err(invalid());}
            }

            let reservations=tx.prepare("SELECT destination_path,provider_checksum,provider_size FROM reconciliation_paths
                WHERE library=?1 AND id=?2 AND version_size=?3 AND
                ((provider_checksum=?4 AND provider_size=?5) OR (provider_checksum='' AND provider_size=-1))")?
                .query_map(params![receipt.library,receipt.state_id,receipt.version_size,receipt.provider_checksum,
                    i64::try_from(receipt.provider_size).map_err(|_redacted_error|invalid())?],
                    |row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,i64>(2)?)))?
                .collect::<Result<Vec<_>,_>>()?;
            for (reserved,checksum,size) in reservations {
                if checksum.is_empty() || size<0 || super::super::primary_layout::path_key(Path::new(&reserved))?!=chosen {
                    return Err(invalid());
                }
            }
            super::super::primary_layout::guard_slot(&tx,&receipt.library,&receipt.state_id,
                &receipt.version_size,&receipt.provider_checksum,&path)?;
            super::super::primary_layout::guard_downloaded_checksum(&tx,&path,&receipt.local_checksum)?;
            validate_file()?;
            // The compact receipt already owns this exact pass and native destination.
            // Preserve completed rows and optional metadata; only finish outstanding retry status.
            let pending: bool=tx.query_row("SELECT status!='downloaded' FROM assets WHERE library=?1 AND id=?2 AND version_size=?3",
                params![receipt.library,receipt.state_id,receipt.version_size],|row|row.get(0))?;
            if pending {
                super::super::asset_writes::update_status_to_downloaded(&tx,&receipt.library,&receipt.state_id,
                    &receipt.version_size,&path,&receipt.local_checksum,Some(&receipt.source_checksum),false,
                    chrono::Utc::now().timestamp())?;
                tx.execute("DELETE FROM asset_verifications WHERE library=?1 AND id=?2 AND version_size=?3",
                    params![receipt.library,receipt.state_id,receipt.version_size])?;
            }
            let protect_readonly = !super::super::provider_generations::reuse_metadata_is_writable(
                &tx,
                &receipt.library,
                &receipt.state_id,
                &receipt.version_size,
                &receipt.native_path.key()?,
                &receipt.provider_checksum,
                &authorization,
            )?;
            if protect_readonly {
                match super::super::primary_layout::persist_readonly_marker(
                &tx,&receipt.library,&receipt.state_id,&receipt.version_size,&path,
                &receipt.provider_checksum,&receipt.local_checksum,&receipt.source_checksum,
            )? {
                super::super::primary_layout::ReadonlyMarker::Marked
                | super::super::primary_layout::ReadonlyMarker::Unchanged => {}
                super::super::primary_layout::ReadonlyMarker::Conflict => {
                    return Err(StateError::ProviderWorkConflict)
                }
                super::super::primary_layout::ReadonlyMarker::Missing
                    | super::super::primary_layout::ReadonlyMarker::Unsupported => {
                        // The compact receipt is valid, but no exact
                        // compatibility row can carry durable read-only
                        // protection. Do not settle; leave the pending owner
                        // for the caller to re-plan.
                        return Ok(false);
                    }
                }
            }
            super::super::provider_generations::record_readonly_destination(
                &tx,&receipt.library,&receipt.state_id,&receipt.version_size,&path,
                &receipt.local_checksum,&receipt.source_checksum,
            )?;
            tx.commit()?;
            Ok(true)
        }).await
    }

    pub(crate) async fn bind_publication(
        &self,
        owner: crate::state::db::account::AccountOwner,
        mut binding: PublicationBinding,
    ) -> Result<PublicationBinding, StateError> {
        self.with_conn("bind authenticated publication", move |conn| {
            super::super::account::validate_authenticated(conn, &owner)?;
            (binding.account_key, binding.provider_key) = conn.query_row(
                "SELECT account_key,provider_key FROM account_owner WHERE singleton=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            binding.validate(conn)?;
            Ok(binding)
        })
        .await
    }
}

pub(in crate::state::db) fn record_bound_receipt(
    conn: &rusqlite::Connection,
    proof: &PreparedPublication,
) -> Result<(), StateError> {
    let Some(binding) = &proof.binding else {
        return Ok(());
    };
    binding.validate(conn)?;
    if binding.library != proof.library || binding.state_id != proof.id {
        return Err(invalid());
    }
    let conflicting_master: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM asset_master_mappings WHERE library=?1 AND asset_record_name=?2 AND master_record_name<>?3)",
        params![binding.library,binding.child,binding.master],|row|row.get(0))?;
    if conflicting_master {
        return Err(invalid());
    }
    super::super::download_receipts::DownloadReceipt {
        account_key: binding.account_key.clone(),
        provider_key: binding.provider_key.clone(),
        scope: binding.scope.clone(),
        library: proof.library.clone(),
        child: binding.child.clone(),
        master: binding.master.clone(),
        state_id: proof.id.clone(),
        origin_pass: binding.origin_pass.clone(),
        version_size: proof.version.clone(),
        provider_checksum: proof.provider_checksum.clone(),
        provider_size: proof.provider_size,
        native_path: proof.destination.clone(),
        source_checksum: proof.source_checksum.clone(),
        local_checksum: proof.local_checksum.clone(),
    }
    .save(conn)?;
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PreparedPublication {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) binding: Option<PublicationBinding>,
    pub(crate) library: String,
    pub(crate) id: String,
    pub(crate) version: String,
    pub(crate) provider_checksum: String,
    pub(crate) provider_size: u64,
    pub(crate) temporary: SelectionPath,
    pub(crate) destination: SelectionPath,
    pub(crate) local_checksum: String,
    pub(crate) source_checksum: String,
}

// Return the typed invariant without exposing provider scope or native paths
// from parsing and confinement errors.
fn invalid() -> StateError {
    StateError::Invariant {
        operation: "prepared_publication",
        detail: "prepared publication evidence is invalid or no longer matches current work".into(),
    }
}

impl PreparedPublication {
    fn validate(&self) -> Result<(), StateError> {
        let digest = |value: &str| {
            value.len() == 64
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        };
        if self.library.is_empty()
            || self.id.is_empty()
            || self.provider_checksum.trim().is_empty()
            || VersionSizeKey::from_str(&self.version).is_none()
            || self.provider_size > i64::MAX as u64
            || !digest(&self.local_checksum)
            || !digest(&self.source_checksum)
        {
            return Err(invalid());
        }
        crate::fs_util::confined_path_key(&self.destination.to_path())
            .map_err(|_error| invalid())?;
        crate::fs_util::confined_path_key(&self.temporary.to_path()).map_err(|_error| invalid())?;
        Ok(())
    }
}

impl SqliteStateDb {
    pub(super) async fn prepare_publication(
        &self,
        part: &Path,
        proof: &PreparedPublication,
    ) -> Result<(), StateError> {
        proof.validate()?;
        let key = OwnedTempPathKey::from_path(part)?.0;
        if OwnedTempPathKey::from_path(&proof.temporary.to_path())?.0 != key {
            return Err(invalid());
        }
        let proof = proof.clone();
        let encoded = serde_json::to_string(&proof).map_err(|_error| invalid())?;
        if encoded.len() > 64 * 1024 {
            return Err(invalid());
        }
        self.with_conn_mut("prepare_temp_publication", move |conn| {
            let tx = conn.transaction()?;
            if let Some(binding)=&proof.binding {
                binding.validate(&tx)?;
                if binding.library != proof.library || binding.state_id != proof.id { return Err(invalid()); }
                tx.execute("INSERT INTO metadata(key,value) VALUES(?1,?2) ON CONFLICT(key) DO NOTHING",
                    params![format!("download_ledger_scope:{}",binding.library),binding.scope])?;
            }
            let current: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM assets WHERE library=?1 AND id=?2 AND version_size=?3
                 AND checksum=?4 AND size_bytes=?5 AND is_deleted=0 AND status IN ('pending','failed','downloaded'))",
                params![proof.library,proof.id,proof.version,proof.provider_checksum,proof.provider_size as i64],
                |row| row.get(0),
            )?;
            if !current {
                return Err(invalid());
            }
            let saved: Option<Option<String>> = tx.query_row(
                "SELECT publication FROM owned_temp_files WHERE path=?1",
                [&key], |row| row.get(0),
            ).optional()?;
            match saved {
                Some(Some(prior)) if prior == encoded => return Ok(()),
                Some(None) => {},
                _ => return Err(invalid()),
            }
            tx.execute("UPDATE owned_temp_files SET publication=?2 WHERE path=?1 AND publication IS NULL",
                params![key,encoded])?;
            tx.commit()?;
            Ok(())
        }).await
    }

    pub(super) async fn prepared_publications(
        &self,
        library: &str,
        id: &str,
        version: VersionSizeKey,
    ) -> Result<Vec<PreparedPublication>, StateError> {
        let library = library.to_owned();
        let id = id.to_owned();
        self.with_conn("get_prepared_publications", move |conn| {
            let mut statement = conn.prepare_cached(
                "SELECT publication FROM owned_temp_files WHERE publication IS NOT NULL
                 AND json_extract(publication,'$.library')=?1 AND json_extract(publication,'$.id')=?2
                 AND json_extract(publication,'$.version')=?3",
            )?;
            let encoded = statement.query_map(params![library,id,version.as_str()], |row| row.get::<_,String>(0))?;
            let mut proofs = Vec::new();
            for value in encoded {
                let proof: PreparedPublication = serde_json::from_str(&value?).map_err(|_error| invalid())?;
                proof.validate()?;
                proofs.push(proof);
            }
            Ok(proofs)
        }).await
    }
}

/// Runs in the same transaction as the final receipt. Any mismatch preserves
/// both the unfinished transfer evidence and the previous asset row.
pub(in crate::state::db) fn retire_prepared_receipt(
    conn: &rusqlite::Connection,
    library: &str,
    id: &str,
    version: &str,
    destination: &Path,
    local_checksum: &str,
    source_checksum: Option<&str>,
) -> Result<Vec<PreparedPublication>, StateError> {
    let destination_key =
        crate::fs_util::confined_path_key(destination).map_err(|_error| invalid())?;
    let mut statement = conn.prepare_cached(
        "SELECT path,publication FROM owned_temp_files WHERE publication IS NOT NULL
         AND json_extract(publication,'$.library')=?1 AND json_extract(publication,'$.id')=?2
         AND json_extract(publication,'$.version')=?3",
    )?;
    let rows = statement.query_map(params![library, id, version], |row| {
        Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut proofs = Vec::new();
    for row in rows {
        let (key, encoded) = row?;
        let proof: PreparedPublication =
            serde_json::from_str(&encoded).map_err(|_error| invalid())?;
        proof.validate()?;
        if OwnedTempPathKey::from_path(&proof.temporary.to_path())?.0 != key {
            return Err(invalid());
        }
        if crate::fs_util::confined_path_key(&proof.destination.to_path())
            .map_err(|_error| invalid())?
            != destination_key
        {
            continue;
        }
        if proof.local_checksum != local_checksum
            || source_checksum != Some(proof.source_checksum.as_str())
        {
            return Err(invalid());
        }
        let current: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM assets WHERE library=?1 AND id=?2 AND version_size=?3
             AND checksum=?4 AND size_bytes=?5 AND is_deleted=0)",
            params![
                library,
                id,
                version,
                proof.provider_checksum,
                proof.provider_size as i64
            ],
            |row| row.get(0),
        )?;
        if !current {
            return Err(invalid());
        }
        proofs.push(proof);
    }
    Ok(proofs)
}

/// Retire only after an independently validated compact successor exists.
/// Unscoped older prepared rows keep their genuine publication lineage.
pub(in crate::state::db) fn retire_handed_off_publication(
    conn: &rusqlite::Connection,
    proof: &PreparedPublication,
) -> Result<(), StateError> {
    let ids: Vec<String> = conn
        .prepare(
            "SELECT id FROM download_receipts WHERE source_checksum IS NOT NULL AND library=?1 AND state_id=?2 AND version_size=?3
         AND provider_checksum=?4 AND provider_size=?5 AND native_path=?6
         AND local_checksum=?7 AND source_checksum=?8",
        )?
        .query_map(
            params![
                proof.library,
                proof.id,
                proof.version,
                proof.provider_checksum,
                proof.provider_size as i64,
                proof.destination.key()?,
                proof.local_checksum,
                proof.source_checksum
            ],
            |row| row.get(0),
        )?
        .collect::<Result<_, _>>()?;
    if ids.is_empty() {
        return Ok(());
    }
    for id in ids {
        super::super::download_receipts::load(conn, &id)?.ok_or_else(invalid)?;
    }
    let key = OwnedTempPathKey::from_path(&proof.temporary.to_path())?.0;
    let encoded: String = conn.query_row(
        "SELECT publication FROM owned_temp_files WHERE path=?1",
        [&key],
        |row| row.get(0),
    )?;
    let saved: PreparedPublication =
        serde_json::from_str(&encoded).map_err(|_redacted_error| invalid())?;
    if saved != *proof {
        return Err(invalid());
    }
    if conn.execute(
        "DELETE FROM owned_temp_files WHERE path=?1 AND publication=?2",
        params![key, encoded],
    )? != 1
    {
        return Err(invalid());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::PreparedPublication;
    use crate::state::db::{TempFileOwnershipStore, provider_selection::SelectionPath};
    use crate::state::{SqliteStateDb, VersionSizeKey};

    #[tokio::test]
    async fn prepared_transfer_survives_reopen_stale_cleanup_and_claim_conflicts() {
        let root = tempfile::tempdir().unwrap();
        let database = root.path().join("state.sqlite");
        let part = root.path().join("pending.part");
        let destination = root.path().join("photo.JPG");
        let db = SqliteStateDb::open(&database).await.unwrap();
        let record = crate::test_helpers::TestAssetRecord::new("child")
            .checksum("opaque-generation")
            .size(4)
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.claim_temp_file(&part).await.unwrap();
        let proof = PreparedPublication {
            binding: None,
            library: record.library.to_string(),
            id: record.id.to_string(),
            version: record.version_size.as_str().to_owned(),
            provider_checksum: record.checksum.to_string(),
            provider_size: record.size_bytes,
            temporary: SelectionPath::from_path(&part),
            destination: SelectionPath::from_path(&destination),
            local_checksum: "a".repeat(64),
            source_checksum: "b".repeat(64),
        };
        db.prepare_temp_publication(&part, &proof).await.unwrap();
        drop(db);
        let db = SqliteStateDb::open(&database).await.unwrap();
        assert_eq!(
            db.get_prepared_publications(&record.library, &record.id, VersionSizeKey::Original)
                .await
                .unwrap(),
            vec![proof.clone()]
        );
        assert!(
            db.get_owned_temp_files_before(i64::MAX)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            db.retire_temp_files(std::slice::from_ref(&part))
                .await
                .unwrap(),
            0
        );
        assert!(db.claim_temp_file(&part).await.is_err());
        let mut changed = proof.clone();
        changed.source_checksum = "c".repeat(64);
        assert!(db.prepare_temp_publication(&part, &changed).await.is_err());
        assert_eq!(
            db.get_prepared_publications(&record.library, &record.id, VersionSizeKey::Original)
                .await
                .unwrap(),
            vec![proof]
        );
        assert!(!destination.exists());
        assert!(!part.exists());
    }

    #[tokio::test]
    async fn prepared_transfer_rejects_stale_provider_generation_without_retiring_owner() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        let root = tempfile::tempdir().unwrap();
        let part = root.path().join("pending.part");
        let record = crate::test_helpers::TestAssetRecord::new("child")
            .checksum("current-generation")
            .size(4)
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.claim_temp_file(&part).await.unwrap();
        let proof = PreparedPublication {
            binding: None,
            library: record.library.to_string(),
            id: record.id.to_string(),
            version: record.version_size.as_str().to_owned(),
            provider_checksum: "stale-generation".to_owned(),
            provider_size: 4,
            temporary: SelectionPath::from_path(&part),
            destination: SelectionPath::from_path(&root.path().join("photo.JPG")),
            local_checksum: "a".repeat(64),
            source_checksum: "b".repeat(64),
        };
        assert!(db.prepare_temp_publication(&part, &proof).await.is_err());
        assert_eq!(
            db.get_owned_temp_files_before(i64::MAX)
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(
            db.get_prepared_publications(&record.library, &record.id, VersionSizeKey::Original)
                .await
                .unwrap()
                .is_empty()
        );
    }
}

#[cfg(test)]
mod finalization_tests {
    use super::PreparedPublication;
    use crate::state::db::{
        DownloadStateStore, TempFileOwnershipStore, provider_selection::SelectionPath,
    };
    use crate::state::{SqliteStateDb, VersionSizeKey};

    #[tokio::test]
    async fn prepared_transfer_unscoped_receipt_commit_is_atomic_and_keeps_lineage() {
        let root = tempfile::tempdir().unwrap();
        let part = root.path().join("pending.part");
        let destination = root.path().join("photo.JPG");
        let db = SqliteStateDb::open_in_memory().unwrap();
        let record = crate::test_helpers::TestAssetRecord::new("child")
            .checksum("opaque-generation")
            .size(4)
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.claim_temp_file(&part).await.unwrap();
        let proof = PreparedPublication {
            binding: None,
            library: record.library.to_string(),
            id: record.id.to_string(),
            version: record.version_size.as_str().to_owned(),
            provider_checksum: record.checksum.to_string(),
            provider_size: 4,
            temporary: SelectionPath::from_path(&part),
            destination: SelectionPath::from_path(&destination),
            local_checksum: "a".repeat(64),
            source_checksum: "b".repeat(64),
        };
        db.prepare_temp_publication(&part, &proof).await.unwrap();
        db.acquire_lock("inject failed finalization")
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER fail_final BEFORE UPDATE OF status ON assets
             WHEN NEW.status='downloaded' BEGIN SELECT RAISE(ABORT,'injected'); END;",
            )
            .unwrap();
        assert!(
            db.mark_verified_download(
                &record.library,
                &record.id,
                "original",
                &destination,
                &proof.local_checksum,
                Some(&proof.source_checksum),
                false
            )
            .await
            .is_err()
        );
        assert_eq!(
            db.get_prepared_publications(&record.library, &record.id, VersionSizeKey::Original)
                .await
                .unwrap(),
            vec![proof.clone()]
        );
        assert_eq!(db.get_pending().await.unwrap().len(), 1);
        db.acquire_lock("clear failed finalization")
            .unwrap()
            .execute_batch("DROP TRIGGER fail_final;")
            .unwrap();
        assert!(
            db.mark_verified_download(
                &record.library,
                &record.id,
                "original",
                &destination,
                &"c".repeat(64),
                Some(&proof.source_checksum),
                false
            )
            .await
            .is_err()
        );
        assert_eq!(
            db.get_prepared_publications(&record.library, &record.id, VersionSizeKey::Original)
                .await
                .unwrap()
                .len(),
            1
        );
        db.mark_verified_download(
            &record.library,
            &record.id,
            "original",
            &destination,
            &proof.local_checksum,
            Some(&proof.source_checksum),
            false,
        )
        .await
        .unwrap();
        assert_eq!(
            db.get_prepared_publications(&record.library, &record.id, VersionSizeKey::Original)
                .await
                .unwrap(),
            vec![proof.clone()],
            "unscoped completion cannot discard its only genuine modified-byte lineage"
        );
        assert!(db.get_pending().await.unwrap().is_empty());
        let source: String = db
            .acquire_lock("final receipt source")
            .unwrap()
            .query_row(
                "SELECT source_checksum FROM asset_metadata_paths WHERE id='child'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(source, proof.source_checksum);
    }
}

#[cfg(test)]
mod binding_tests {
    use super::{PreparedPublication, PublicationBinding};
    use crate::state::MetadataRewriteStore;
    use crate::state::db::{
        DownloadStateStore, TempFileOwnershipStore, account::AccountOwner,
        provider_generations::ReuseAuthorization, provider_selection::SelectionPath,
    };
    use crate::state::{SqliteStateDb, VersionSizeKey};

    fn authorization(flags: u8, explicit: bool) -> ReuseAuthorization {
        ReuseAuthorization {
            current_metadata_flags: flags,
            explicit_refresh_authorized: explicit,
            scope: "test-scope".into(),
            pass_key: "selected-pass".into(),
            child: "child".into(),
            generation: None,
        }
    }

    fn owner() -> AccountOwner {
        AccountOwner::authenticated(
            "publication@example.invalid",
            "com",
            &serde_json::from_value(serde_json::json!({"dsInfo":{"dsid":"publication-test"}}))
                .unwrap(),
        )
        .unwrap()
    }
    async fn fixture(
        db: &SqliteStateDb,
        root: &std::path::Path,
        bound: bool,
    ) -> PreparedPublication {
        let record = crate::test_helpers::TestAssetRecord::new("child")
            .checksum("opaque-generation")
            .size(4)
            .build();
        db.upsert_seen(&record).await.unwrap();
        let temporary = root.join("owned.part");
        db.claim_temp_file(&temporary).await.unwrap();
        let binding = if bound {
            Some(db.bind_publication(owner(),PublicationBinding {
                account_key:String::new(),provider_key:String::new(),
                scope:serde_json::json!({"format":1,"realm":"com","container":"com.apple.photos.cloud",
                    "environment":"production","database":"private","zone":{"zoneName":record.library.as_ref(),"ownerRecordName":"_defaultOwner"}}).to_string(),
                library:record.library.to_string(),child:"child".into(),master:"master".into(),
                state_id:"child".into(),origin_pass:"selected-pass".into(),
            }).await.unwrap())
        } else {
            None
        };
        PreparedPublication {
            binding,
            library: record.library.to_string(),
            id: "child".into(),
            version: "original".into(),
            provider_checksum: "opaque-generation".into(),
            provider_size: 4,
            temporary: SelectionPath::from_path(&temporary),
            destination: SelectionPath::from_path(&root.join("photo.JPG")),
            local_checksum: "a".repeat(64),
            source_checksum: "b".repeat(64),
        }
    }
    async fn finish(
        db: &SqliteStateDb,
        proof: &PreparedPublication,
    ) -> Result<(), crate::state::error::StateError> {
        db.mark_verified_download(
            &proof.library,
            &proof.id,
            "original",
            &proof.destination.to_path(),
            &proof.local_checksum,
            Some(&proof.source_checksum),
            false,
        )
        .await
    }

    #[tokio::test]
    async fn v027_old_unscoped_prepared_proof_roundtrips_and_survives_completion_reopen() {
        let root = tempfile::tempdir().unwrap();
        let database = root.path().join("state.db");
        let db = SqliteStateDb::open_owned(&database, &owner())
            .await
            .unwrap();
        let proof = fixture(&db, root.path(), false).await;
        let encoded = serde_json::to_value(&proof).unwrap();
        assert!(
            encoded.get("binding").is_none(),
            "old format remains decodable"
        );
        assert_eq!(
            serde_json::from_value::<PreparedPublication>(encoded.clone()).unwrap(),
            proof
        );
        let mut unknown = encoded;
        unknown["invented_scope"] = serde_json::json!("untrusted");
        assert!(serde_json::from_value::<PreparedPublication>(unknown).is_err());
        db.prepare_temp_publication(&proof.temporary.to_path(), &proof)
            .await
            .unwrap();
        finish(&db, &proof).await.unwrap();
        drop(db);
        let db = SqliteStateDb::open_owned(&database, &owner())
            .await
            .unwrap();
        assert_eq!(
            db.get_prepared_publications(&proof.library, &proof.id, VersionSizeKey::Original)
                .await
                .unwrap(),
            vec![proof]
        );
        assert!(
            db.get_owned_temp_files_before(i64::MAX)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn v027_bound_receipt_failure_rolls_back_asset_and_keeps_prepared_proof() {
        let root = tempfile::tempdir().unwrap();
        let database = root.path().join("state.db");
        let db = SqliteStateDb::open_owned(&database, &owner())
            .await
            .unwrap();
        let proof = fixture(&db, root.path(), true).await;
        let mut tampered = proof.clone();
        tampered.binding.as_mut().unwrap().provider_key = "foreign-provider".into();
        assert!(
            db.prepare_temp_publication(&proof.temporary.to_path(), &tampered)
                .await
                .is_err()
        );
        db.prepare_temp_publication(&proof.temporary.to_path(), &proof)
            .await
            .unwrap();
        db.acquire_lock("fail successor receipt").unwrap().execute_batch(
            "CREATE TRIGGER fail_bound_receipt BEFORE INSERT ON download_receipts BEGIN SELECT RAISE(ABORT,'injected'); END;"
        ).unwrap();
        assert!(finish(&db, &proof).await.is_err());
        drop(db);
        let db = SqliteStateDb::open_owned(&database, &owner())
            .await
            .unwrap();
        assert_eq!(db.get_pending().await.unwrap().len(), 1);
        assert_eq!(
            db.get_prepared_publications(&proof.library, &proof.id, VersionSizeKey::Original)
                .await
                .unwrap(),
            vec![proof.clone()]
        );
        db.acquire_lock("allow receipt")
            .unwrap()
            .execute_batch("DROP TRIGGER fail_bound_receipt")
            .unwrap();
        finish(&db, &proof).await.unwrap();
        drop(db);
        let db = SqliteStateDb::open_owned(&database, &owner())
            .await
            .unwrap();
        assert!(
            db.get_prepared_publications(&proof.library, &proof.id, VersionSizeKey::Original)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            db.acquire_lock("receipt survives reopen")
                .unwrap()
                .query_row::<i64, _, _>("SELECT count(*) FROM download_receipts", [], |row| row
                    .get(0))
                .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn cross_rendition_original_requires_exact_source_receipt_and_fingerprint() {
        use sha2::{Digest, Sha256};
        let root = tempfile::tempdir().unwrap();
        let database = root.path().join("state.db");
        let db = SqliteStateDb::open_owned(&database, &owner())
            .await
            .unwrap();
        let mut proof = fixture(&db, root.path(), true).await;
        std::fs::write(proof.destination.to_path(), b"original bytes").unwrap();
        proof.local_checksum = format!("{:x}", Sha256::digest(b"original bytes"));
        db.prepare_temp_publication(&proof.temporary.to_path(), &proof)
            .await
            .unwrap();
        finish(&db, &proof).await.unwrap();
        drop(db);
        let db = SqliteStateDb::open_owned(&database, &owner())
            .await
            .unwrap();
        let binding = proof.binding.clone().unwrap();
        let original = || super::CrossRenditionOriginal {
            path: proof.destination.clone(),
            provider_checksum: proof.provider_checksum.clone(),
            provider_size: proof.provider_size,
            local_checksum: Some(format!(
                "{:x}",
                Sha256::digest(std::fs::read(proof.destination.to_path()).unwrap())
            )),
        };
        let find = |binding, version, original| {
            db.bound_cross_rendition_original(binding, version, original)
        };
        assert!(
            find(binding.clone(), VersionSizeKey::Adjusted, original())
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            find(binding.clone(), VersionSizeKey::Original, original())
                .await
                .unwrap()
                .is_none()
        );
        for field in ["child", "master", "state", "pass", "library"] {
            let mut other = binding.clone();
            match field {
                "child" => other.child.push_str("-other"),
                "master" => other.master.push_str("-other"),
                "state" => other.state_id.push_str("-other"),
                "pass" => other.origin_pass.push_str("-other"),
                _ => other.library.push_str("-other"),
            }
            let result = find(other, VersionSizeKey::Adjusted, original()).await;
            assert!(result.is_err() || result.unwrap().is_none(), "{field}");
        }
        let mut foreign = binding.clone();
        foreign.account_key.push_str("-other");
        assert!(
            find(foreign, VersionSizeKey::Adjusted, original())
                .await
                .is_err()
        );
        let mut wrong = original();
        wrong.provider_checksum.push_str("-other");
        assert!(
            find(binding.clone(), VersionSizeKey::Adjusted, wrong)
                .await
                .unwrap()
                .is_none()
        );
        let mut wrong = original();
        wrong.provider_size += 1;
        assert!(
            find(binding.clone(), VersionSizeKey::Adjusted, wrong)
                .await
                .unwrap()
                .is_none()
        );
        let mut wrong = original();
        wrong.path = SelectionPath::from_path(&root.path().join("other.JPG"));
        assert!(
            find(binding.clone(), VersionSizeKey::Adjusted, wrong)
                .await
                .unwrap()
                .is_none()
        );
        let mut missing = original();
        missing.local_checksum = None;
        assert!(
            find(binding.clone(), VersionSizeKey::Adjusted, missing)
                .await
                .unwrap()
                .is_none()
        );
        std::fs::write(proof.destination.to_path(), b"changed bytes").unwrap();
        assert!(
            find(binding.clone(), VersionSizeKey::Adjusted, original())
                .await
                .unwrap()
                .is_none()
        );
        std::fs::write(proof.destination.to_path(), b"original bytes").unwrap();
        assert!(
            find(binding.clone(), VersionSizeKey::Adjusted, original())
                .await
                .unwrap()
                .is_some()
        );
        // A valid receipt for a different occupant rendition is not an original receipt.
        {
            let mut adjusted = find(binding.clone(), VersionSizeKey::Adjusted, original())
                .await
                .unwrap()
                .unwrap()
                .receipt;
            let conn = db
                .acquire_lock("replace original with adjusted receipt")
                .unwrap();
            adjusted.version_size = "adjusted".into();
            adjusted.save(&conn).unwrap();
            conn.execute(
                "DELETE FROM download_receipts WHERE version_size='original'",
                [],
            )
            .unwrap();
        }
        assert!(
            find(binding.clone(), VersionSizeKey::Adjusted, original())
                .await
                .unwrap()
                .is_none()
        );
        // Restore the source-backed original so the next control changes only trust type.
        {
            let conn = db.acquire_lock("restore original receipt").unwrap();
            let id: String = conn
                .query_row(
                    "SELECT id FROM download_receipts WHERE version_size='adjusted'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            let mut receipt = crate::state::db::download_receipts::load(&conn, &id)
                .unwrap()
                .unwrap();
            receipt.version_size = "original".into();
            receipt.save(&conn).unwrap();
            conn.execute(
                "DELETE FROM download_receipts WHERE version_size='adjusted'",
                [],
            )
            .unwrap();
        }
        // A local-trust-shaped row is excluded before its proof can confer source authority.
        db.acquire_lock("replace source proof with local-trust row").unwrap().execute(
            "UPDATE download_receipts SET source_checksum=NULL, trust_receipt='local', selected_metadata_hash='metadata', generation_id='generation'", [],
        ).unwrap();
        assert!(
            find(binding, VersionSizeKey::Adjusted, original())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn historical_receipts_require_exact_binding_and_different_resource() {
        let root = tempfile::tempdir().unwrap();
        let database = root.path().join("state.db");
        let db = SqliteStateDb::open_owned(&database, &owner())
            .await
            .unwrap();
        let proof = fixture(&db, root.path(), true).await;
        db.prepare_temp_publication(&proof.temporary.to_path(), &proof)
            .await
            .unwrap();
        finish(&db, &proof).await.unwrap();
        drop(db);
        let db = SqliteStateDb::open_owned(&database, &owner())
            .await
            .unwrap();
        let binding = proof.binding.clone().unwrap();
        let find = |binding: super::PublicationBinding, checksum: &'static str, size| {
            db.bound_historical_downloads(binding, "original".into(), checksum.into(), size)
        };
        assert_eq!(find(binding.clone(), "changed", 5).await.unwrap().len(), 1);
        assert!(
            find(binding.clone(), "opaque-generation", 4)
                .await
                .unwrap()
                .is_empty()
        );
        for field in ["child", "master", "state", "pass", "library"] {
            let mut other = binding.clone();
            match field {
                "child" => other.child.push_str("-other"),
                "master" => other.master.push_str("-other"),
                "state" => other.state_id.push_str("-other"),
                "pass" => other.origin_pass.push_str("-other"),
                _ => other.library.push_str("-other"),
            }
            let result = find(other, "changed", 5).await;
            assert!(result.is_err() || result.unwrap().is_empty(), "{field}");
        }
        let mut foreign = binding;
        foreign.account_key.push_str("-other");
        assert!(find(foreign, "changed", 5).await.is_err());
    }

    #[tokio::test]
    async fn v027_bound_reuse_persists_marker_and_refuses_conflict_or_missing_row() {
        let root = tempfile::tempdir().unwrap();
        let database = root.path().join("state.db");
        let db = SqliteStateDb::open_owned(&database, &owner())
            .await
            .unwrap();
        let proof = fixture(&db, root.path(), true).await;
        db.prepare_temp_publication(&proof.temporary.to_path(), &proof)
            .await
            .unwrap();
        finish(&db, &proof).await.unwrap();
        let destination = proof.destination.to_path();
        let binding = proof.binding.clone().unwrap();
        let receipt = db
            .bound_download_receipts(
                binding.clone(),
                "original".into(),
                "opaque-generation".into(),
                4,
            )
            .await
            .unwrap()
            .remove(0);
        let record = crate::test_helpers::TestAssetRecord::new("child")
            .checksum("opaque-generation")
            .size(4)
            .build();
        db.reuse_bound_download(
            binding.clone(),
            receipt.clone(),
            record.clone(),
            Box::new(|| Ok(())),
            authorization(0, false),
        )
        .await
        .unwrap();
        let marker: String = db
            .acquire_lock("read bound reuse marker")
            .unwrap()
            .query_row(
                "SELECT preserved_path_key FROM asset_metadata_paths
                 WHERE library=?1 AND id=?2 AND version_size=?3 AND local_path=?4",
                rusqlite::params![
                    proof.library,
                    proof.id,
                    "original",
                    destination.to_str().unwrap()
                ],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            marker,
            crate::state::db::primary_layout::path_key(&destination).unwrap()
        );
        let row: (String, String, String) = db
            .acquire_lock("bound compatibility evidence")
            .unwrap()
            .query_row(
                "SELECT provider_checksum,local_checksum,source_checksum
                 FROM asset_metadata_paths
                 WHERE library=?1 AND id=?2 AND version_size='original' AND local_path=?3",
                rusqlite::params![proof.library, proof.id, destination.to_str().unwrap()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(row.0, receipt.provider_checksum);
        assert_eq!(row.1, receipt.local_checksum);
        assert_eq!(row.2, receipt.source_checksum);
        db.mark_verified_download(
            &proof.library,
            &proof.id,
            "original",
            &destination,
            &proof.local_checksum,
            Some(&proof.source_checksum),
            false,
        )
        .await
        .unwrap();
        let marker_after_receipt: String = db
            .acquire_lock("marker survives bound receipt update")
            .unwrap()
            .query_row(
                "SELECT preserved_path_key FROM asset_metadata_paths
                 WHERE library=?1 AND id=?2 AND version_size='original' AND local_path=?3",
                rusqlite::params![proof.library, proof.id, destination.to_str().unwrap()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(marker_after_receipt, marker);
        assert!(db.guard_primary_writer(&destination).await.is_err());
        assert_eq!(
            db.source_metadata_recovery(&destination).await.unwrap(),
            crate::state::db::SourceMetadataRecovery::ReadOnly
        );
        let mut sidecar = destination.as_os_str().to_os_string();
        sidecar.push(".xmp");
        let sidecar = std::path::PathBuf::from(sidecar);
        let protected = db.protected_legacy_paths().await.unwrap();
        assert!(protected.iter().any(|path| path == &destination));
        assert!(protected.iter().any(|path| path == &sidecar));
        let reopened = SqliteStateDb::open_owned(&database, &owner())
            .await
            .unwrap();
        assert!(reopened.guard_primary_writer(&destination).await.is_err());
        assert_eq!(
            reopened
                .source_metadata_recovery(&destination)
                .await
                .unwrap(),
            crate::state::db::SourceMetadataRecovery::ReadOnly
        );
        db.reuse_bound_download(
            binding.clone(),
            receipt.clone(),
            record.clone(),
            Box::new(|| Ok(())),
            authorization(0, false),
        )
        .await
        .unwrap();
        db.acquire_lock("conflicting bound marker")
            .unwrap()
            .execute(
                "UPDATE asset_metadata_paths SET preserved_path_key='conflict' WHERE id='child'",
                [],
            )
            .unwrap();
        assert!(
            db.reuse_bound_download(
                binding.clone(),
                receipt.clone(),
                record.clone(),
                Box::new(|| Ok(())),
                authorization(0, false),
            )
            .await
            .is_err()
        );
        db.acquire_lock("remove bound compatibility receipt")
            .unwrap()
            .execute(
                "DELETE FROM asset_metadata_paths WHERE library=?1 AND id=?2 AND version_size='original' AND local_path=?3",
                rusqlite::params![proof.library, proof.id, destination.to_str().unwrap()],
            )
            .unwrap();
        assert!(
            !db.reuse_bound_download(
                binding,
                receipt,
                record,
                Box::new(|| Ok(())),
                authorization(0, false)
            )
            .await
            .unwrap()
        );
    }

    #[tokio::test]
    async fn v027_bound_reuse_marker_failure_rolls_back_settlement() {
        let root = tempfile::tempdir().unwrap();
        let db = SqliteStateDb::open_owned(&root.path().join("state.db"), &owner())
            .await
            .unwrap();
        let proof = fixture(&db, root.path(), true).await;
        db.prepare_temp_publication(&proof.temporary.to_path(), &proof)
            .await
            .unwrap();
        finish(&db, &proof).await.unwrap();
        let binding = proof.binding.clone().unwrap();
        let receipt = db
            .bound_download_receipts(
                binding.clone(),
                "original".into(),
                "opaque-generation".into(),
                4,
            )
            .await
            .unwrap()
            .remove(0);
        let record = crate::test_helpers::TestAssetRecord::new("child")
            .checksum("opaque-generation")
            .size(4)
            .build();
        db.acquire_lock("make bound reuse pending")
            .unwrap()
            .execute("UPDATE assets SET status='pending' WHERE id='child'", [])
            .unwrap();
        db.acquire_lock("fail readonly marker")
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER fail_readonly_marker
                 BEFORE UPDATE OF preserved_path_key ON asset_metadata_paths
                 WHEN NEW.preserved_path_key IS NOT NULL
                 BEGIN SELECT RAISE(ABORT,'injected marker failure'); END;",
            )
            .unwrap();
        assert!(
            db.reuse_bound_download(
                binding.clone(),
                receipt.clone(),
                record.clone(),
                Box::new(|| Ok(())),
                authorization(0, false),
            )
            .await
            .is_err()
        );
        let status: String = db
            .acquire_lock("bound rollback status")
            .unwrap()
            .query_row("SELECT status FROM assets WHERE id='child'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(status, "pending");
        let marker: Option<String> = db
            .acquire_lock("bound rollback marker")
            .unwrap()
            .query_row(
                "SELECT preserved_path_key FROM asset_metadata_paths WHERE id='child'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(marker.is_none());
        db.acquire_lock("allow readonly marker")
            .unwrap()
            .execute_batch("DROP TRIGGER fail_readonly_marker")
            .unwrap();
        db.reuse_bound_download(
            binding,
            receipt,
            record,
            Box::new(|| Ok(())),
            authorization(0, false),
        )
        .await
        .unwrap();
        let marker: String = db
            .acquire_lock("bound marker after retry")
            .unwrap()
            .query_row(
                "SELECT preserved_path_key FROM asset_metadata_paths WHERE id='child'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!marker.is_empty());
    }
    #[tokio::test]
    async fn v027_bound_reuse_explicit_refresh_skips_new_marker_but_keeps_existing_marker_authoritative()
     {
        let root = tempfile::tempdir().unwrap();
        let db = SqliteStateDb::open_owned(&root.path().join("state.db"), &owner())
            .await
            .unwrap();
        let proof = fixture(&db, root.path(), true).await;
        db.prepare_temp_publication(&proof.temporary.to_path(), &proof)
            .await
            .unwrap();
        finish(&db, &proof).await.unwrap();
        let binding = proof.binding.clone().unwrap();
        let receipt = db
            .bound_download_receipts(
                binding.clone(),
                "original".into(),
                "opaque-generation".into(),
                4,
            )
            .await
            .unwrap()
            .remove(0);
        let record = crate::test_helpers::TestAssetRecord::new("child")
            .checksum("opaque-generation")
            .size(4)
            .build();
        let destination = proof.destination.to_path();
        assert!(
            db.reuse_bound_download(
                binding.clone(),
                receipt.clone(),
                record.clone(),
                Box::new(|| Ok(())),
                authorization(0, true),
            )
            .await
            .unwrap()
        );
        let marker: Option<String> = db
            .acquire_lock("explicit refresh has no new marker")
            .unwrap()
            .query_row(
                "SELECT preserved_path_key FROM asset_metadata_paths WHERE id='child'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(marker.is_none());
        assert!(db.guard_primary_writer(&destination).await.is_ok());

        let marker = crate::state::db::primary_layout::path_key(&destination).unwrap();
        db.acquire_lock("seed pre-existing compact marker")
            .unwrap()
            .execute(
                "UPDATE asset_metadata_paths SET preserved_path_key=?1 WHERE id='child'",
                [&marker],
            )
            .unwrap();
        assert!(db.guard_primary_writer(&destination).await.is_err());
        assert!(
            db.reuse_bound_download(
                binding,
                receipt,
                record,
                Box::new(|| Ok(())),
                authorization(0, true),
            )
            .await
            .unwrap()
        );
        assert!(db.guard_primary_writer(&destination).await.is_err());
    }
}
