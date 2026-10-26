//! Receipt-backed read-only adoption into the ordinary download ledger.

use rusqlite::{OptionalExtension, params};

use super::{BaselineReceipt, SqliteStateDb};
use crate::state::AssetRecord;
use crate::state::error::StateError;

impl SqliteStateDb {
    pub(crate) async fn defer_baseline_match(
        &self,
        record: &AssetRecord,
    ) -> Result<(), StateError> {
        let record = record.clone();
        self.with_conn_mut("defer_baseline_match", move |conn| {
            let tx = conn.transaction()?;
            let now = chrono::Utc::now().timestamp();
            let target: Option<(String, i64, bool)> = tx
                .query_row(
                    "SELECT checksum,size_bytes,is_deleted FROM assets
                 WHERE library=?1 AND id=?2 AND version_size=?3",
                    params![
                        record.library.as_ref(),
                        record.id.as_ref(),
                        record.version_size.as_str()
                    ],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?;
            if let Some((checksum, size, deleted)) = &target
                && (checksum != record.checksum.as_ref()
                    || u64::try_from(*size).ok() != Some(record.size_bytes)
                    || *deleted)
            {
                return Err(StateError::Invariant {
                    operation: "defer_baseline_match",
                    detail: "current observation changed before uncertain work admission".into(),
                });
            }
            if target.is_none() {
                super::asset_writes::upsert_asset_row(&tx, &record, now)?;
            }
            let prior: Option<String> = tx.query_row(
                "SELECT last_error FROM assets WHERE library=?1 AND id=?2 AND version_size=?3",
                params![
                    record.library.as_ref(),
                    record.id.as_ref(),
                    record.version_size.as_str()
                ],
                |row| row.get(0),
            )?;
            let diagnostic = record
                .last_error
                .as_deref()
                .and_then(|new| {
                    super::match_diagnostics::MatchDiagnostics::merge(prior.as_deref(), new)
                })
                .or(prior.filter(|old| {
                    super::match_diagnostics::MatchDiagnostics::decode(old).is_some()
                }))
                .unwrap_or_else(|| {
                    "Existing file match remains uncertain; no detailed attempt recorded".into()
                });
            tx.execute(
                "UPDATE assets SET status='pending',last_error=?4
                 WHERE library=?1 AND id=?2 AND version_size=?3",
                params![
                    record.library.as_ref(),
                    record.id.as_ref(),
                    record.version_size.as_str(),
                    diagnostic
                ],
            )?;
            tx.execute(
                "INSERT INTO asset_verifications(library,id,version_size,state,reason,checked_at)
                 VALUES(?1,?2,?3,'unknown','baseline_match_uncertain',?4)
                 ON CONFLICT(library,id,version_size) DO UPDATE SET
                 state=excluded.state,reason=excluded.reason,checked_at=excluded.checked_at",
                params![
                    record.library.as_ref(),
                    record.id.as_ref(),
                    record.version_size.as_str(),
                    now
                ],
            )?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn adopt_baseline_receipt(
        &self,
        record: &AssetRecord,
        receipt: &BaselineReceipt,
        validate_file: Box<dyn FnOnce() -> Result<(), StateError> + Send>,
    ) -> Result<(), StateError> {
        self.adopt_existing_receipt(record, receipt, validate_file, true)
            .await
    }

    /// The callback rechecks the explicit provider signature on the same held
    /// file inside this transaction. This proof origin is never read from state.
    pub(crate) async fn adopt_provider_signature(
        &self,
        record: &AssetRecord,
        receipt: &BaselineReceipt,
        validate_file: Box<dyn FnOnce() -> Result<(), StateError> + Send>,
    ) -> Result<(), StateError> {
        self.adopt_existing_receipt(record, receipt, validate_file, false)
            .await
    }

    async fn adopt_existing_receipt(
        &self,
        record: &AssetRecord,
        receipt: &BaselineReceipt,
        validate_file: Box<dyn FnOnce() -> Result<(), StateError> + Send>,
        historical: bool,
    ) -> Result<(), StateError> {
        let record = record.clone();
        let receipt = receipt.clone();
        self.with_conn_mut("adopt_baseline_receipt", move |conn| {
            let tx = conn.transaction()?;
            let refuse = || StateError::Invariant {
                operation: "adopt_baseline_receipt",
                detail: "existing-file receipt changed or has no source provenance".into(),
            };
            let Some(source) = receipt.source_checksum.as_deref() else {
                return Err(refuse());
            };
            let Some(local) = receipt.local_checksum.as_deref() else {
                return Err(refuse());
            };
            if record.checksum.as_ref() != receipt.provider_checksum
                || record.version_size != receipt.version_size
                || record.size_bytes != receipt.provider_size
                || record.checksum.trim().is_empty()
                || (!historical && (record.library.as_ref() != receipt.library
                    || record.id.as_ref() != receipt.id))
                || source != local
                || source.len() != 64
                || local.len() != 64
                || !source.bytes().chain(local.bytes()).all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            {
                return Err(refuse());
            }
            let Some(path_text) = receipt.local_path.to_str() else {
                return Err(refuse());
            };
            let unchanged: bool = !historical || tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM asset_metadata_paths p JOIN assets a
                 ON a.library=p.library AND a.id=p.id AND a.version_size=p.version_size
                 WHERE p.library=?1 AND p.id=?2 AND p.version_size=?3
                   AND p.local_path=?4 AND p.provider_checksum=?5
                   AND a.checksum=p.provider_checksum AND a.size_bytes=?6
                   AND a.is_deleted=0 AND a.downloaded_at IS NOT NULL
                   AND NOT EXISTS(SELECT 1 FROM primary_layout_bindings b
                       WHERE b.library=p.library AND b.child=p.id)
                   AND p.local_checksum=?7 AND p.source_checksum=?8)",
                params![receipt.library,receipt.id,receipt.version_size.as_str(),
                    path_text,receipt.provider_checksum,
                    i64::try_from(receipt.provider_size).map_err(|_error| refuse())?,local,source],
                |row| row.get(0),
            )?;
            if !unchanged {
                return Err(refuse());
            }
            super::primary_layout::guard_slot(&tx, &record.library, &record.id,
                record.version_size.as_str(), &record.checksum, &receipt.local_path)?;
            super::primary_layout::guard_downloaded_checksum(&tx, &receipt.local_path, local)?;

            // A newer observation or an existing completed destination cannot
            // be replaced by a stale matching decision.
            let target: Option<(String, i64, bool, String, Option<String>)> = tx.query_row(
                "SELECT checksum,size_bytes,is_deleted,status,local_path FROM assets
                 WHERE library=?1 AND id=?2 AND version_size=?3",
                params![record.library.as_ref(),record.id.as_ref(),record.version_size.as_str()],
                |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?)),
            ).optional()?;
            if let Some((checksum,size,deleted,status,path)) = &target
                && (checksum != record.checksum.as_ref()
                    || u64::try_from(*size).ok() != Some(record.size_bytes)
                    || *deleted
                    || (status == "downloaded" && path.as_deref().is_some_and(|path| path != path_text)))
            {
                return Err(refuse());
            }

            // A current-generation reservation is an independent destination
            // obligation. Historical generations stay retained and untouched.
            let chosen_key = super::primary_layout::path_key(&receipt.local_path)?;
            {
                let mut statement = tx.prepare_cached(
                    "SELECT destination_path,provider_checksum,provider_size FROM reconciliation_paths
                     WHERE library=?1 AND id=?2 AND version_size=?3
                       AND ((provider_checksum=?4 AND provider_size=?5)
                            OR (provider_checksum='' AND provider_size=-1))",
                )?;
                let reservations = statement.query_map(
                    params![record.library.as_ref(),record.id.as_ref(),record.version_size.as_str(),
                        record.checksum.as_ref(),i64::try_from(record.size_bytes).map_err(|_error| refuse())?],
                    |row| Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,i64>(2)?)),
                )?;
                for reservation in reservations {
                    let (path,checksum,size) = reservation?;
                    if checksum.is_empty() || size < 0
                        || super::primary_layout::path_key(std::path::Path::new(&path))? != chosen_key
                    {
                        return Err(refuse());
                    }
                }
            }

            // Already recorded exact identities need no metadata or timestamp update.
            let already_present: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM assets WHERE library=?1 AND id=?2
                 AND version_size=?3 AND checksum=?4 AND status='downloaded'
                 AND is_deleted=0 AND size_bytes=?7
                 AND local_path=?5 AND local_checksum=?6)",
                params![record.library.as_ref(),record.id.as_ref(),record.version_size.as_str(),
                    record.checksum.as_ref(),path_text,local,
                    i64::try_from(record.size_bytes).map_err(|_error| refuse())?],
                |row| row.get(0),
            )?;
            if !already_present {
                let now = chrono::Utc::now().timestamp();
                if target.is_none() {
                    super::asset_writes::upsert_asset_row(&tx, &record, now)?;
                }
                super::asset_writes::update_status_to_downloaded(
                    &tx,&record.library,&record.id,record.version_size.as_str(),
                    &receipt.local_path,local,Some(source),false,now)?;
                tx.execute(
                    "UPDATE asset_metadata_paths SET source_checksum=?5
                     WHERE library=?1 AND id=?2 AND version_size=?3 AND local_path=?4",
                    params![record.library.as_ref(),record.id.as_ref(),record.version_size.as_str(),
                        receipt.local_path.to_string_lossy(),source],
                )?;
                tx.execute(
                    "DELETE FROM asset_verifications WHERE library=?1 AND id=?2 AND version_size=?3",
                    params![record.library.as_ref(),record.id.as_ref(),record.version_size.as_str()],
                )?;
            }
            if !historical {
                // A current already-downloaded row may predate path receipts.
                // Establish only the checked raw-byte binding; preserve old
                // optional metadata debt and all other recorded paths.
                super::asset_writes::record_metadata_path(
                    &tx, &record.library, &record.id, record.version_size.as_str(),
                    super::asset_writes::MetadataPathWrite::Published,
                )?;
                tx.execute(
                    "UPDATE asset_metadata_paths SET source_checksum=?5
                     WHERE library=?1 AND id=?2 AND version_size=?3 AND local_path=?4
                       AND provider_checksum=?6 AND local_checksum=?5",
                    params![record.library.as_ref(), record.id.as_ref(),
                        record.version_size.as_str(), path_text, source,
                        record.checksum.as_ref()],
                )?;
            }
            tx.execute(
                "UPDATE asset_metadata_paths SET preserved_path_key=?5
                 WHERE library=?1 AND id=?2 AND version_size=?3 AND local_path=?4",
                params![record.library.as_ref(),record.id.as_ref(),record.version_size.as_str(),
                    receipt.local_path.to_string_lossy(),super::primary_layout::path_key(&receipt.local_path)?],
            )?;
            // Validate held bytes and the confined namespace immediately before commit.
            validate_file()?;
            super::provider_generations::record_readonly_destination(
                &tx, &record.library, &record.id, record.version_size.as_str(),
                &receipt.local_path, local, source,
            )?;
            tx.commit()?;
            Ok(())
        }).await
    }
}
