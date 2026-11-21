//! Reuse a completed native writer only after measuring both installed files.
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use super::{
    ActiveDecision, ActiveGeneration, SelectionDestination, SqliteStateDb, StateError,
    WriterOutput, account, digest, encode, finish_metadata_receipt, load_decision, load_generation,
    metadata_receipt,
};
use crate::state::db::PendingMetadataRewrite;

// Selection basis and generation can change without changing this physical writer.
fn proof(
    pending: &PendingMetadataRewrite,
    root: &ActiveGeneration,
    manifest: &ActiveDecision,
    dest: &SelectionDestination,
    media: &str,
    packet: WriterOutput,
) -> Result<String, StateError> {
    let receipt = pending
        .selection_receipt
        .as_ref()
        .ok_or(StateError::ProviderSelectionInvalid)?;
    Ok(digest(&encode(&(
        "native-metadata-writer-v1",
        (
            &root.spec.scope,
            &root.spec.config_hash,
            root.spec.metadata_flags,
        ),
        (
            &receipt.pass_key,
            &receipt.child,
            &manifest.state_id,
            &manifest.decision.master,
            manifest.media_zone(root),
        ),
        dest,
        &receipt.grouping_hash,
        pending.asset.created_at,
        pending.asset.added_at,
        media,
        &pending.source_checksum,
        packet,
    ))?))
}

pub(super) fn record(
    conn: &Connection,
    pending: &PendingMetadataRewrite,
    root: &ActiveGeneration,
    manifest: &ActiveDecision,
    dest: &SelectionDestination,
    media: &str,
    packet: WriterOutput,
) -> Result<(), StateError> {
    let checked = proof(pending, root, manifest, dest, media, packet)?;
    conn.execute("UPDATE provider_active_destinations SET writer_output_proof=?6 WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5 AND verified_metadata=1",
        params![root.id,manifest.decision.pass_key,manifest.decision.child,dest.version_size,dest.path.key()?,checked])?;
    Ok(())
}

#[derive(Clone, Default)]
pub(crate) struct WriterKey {
    pass: String,
    child: String,
    version: String,
    path: String,
}

impl WriterKey {
    pub(crate) fn native_path(&self) -> Result<std::path::PathBuf, StateError> {
        serde_json::from_str::<super::super::provider_selection::SelectionPath>(&self.path)
            .map(|path| path.to_path())
            .map_err(|_invalid| StateError::ProviderSelectionInvalid)
    }
}

impl SqliteStateDb {
    pub(crate) async fn writer_output_candidates(
        &self,
        owner: account::AccountOwner,
        generation: String,
        after: WriterKey,
    ) -> Result<Vec<WriterKey>, StateError> {
        self.with_conn("finding previous writer outputs", move |conn| {
            account::validate_authenticated(conn, &owner)?;
            Ok(conn.prepare("SELECT pass_key,child,version_size,path FROM provider_active_destinations w
                WHERE generation=?1 AND (pass_key,child,version_size,path)>(?2,?3,?4,?5)
                AND admitted=1 AND verified_media=1 AND verified_metadata=0 AND grouping_hash IS NOT NULL AND prepared_hash IS NULL AND preserved_local IS NULL
                AND EXISTS (SELECT 1 FROM assets a WHERE a.library=w.library AND a.id=w.asset_id AND a.version_size=w.version_size AND a.checksum=w.checksum AND a.metadata_hash IS w.metadata_hash AND a.created_at IS w.created_at AND a.added_at IS w.added_at AND a.is_deleted=0)
                AND EXISTS (SELECT 1 FROM provider_active_destinations old WHERE old.library=w.library AND old.asset_id=w.asset_id AND old.path=w.path AND old.writer_output_proof IS NOT NULL AND old.verified_metadata=1)
                ORDER BY pass_key,child,version_size,path LIMIT 64")?
                .query_map(params![generation,after.pass,after.child,after.version,after.path], |r| Ok(WriterKey {pass:r.get(0)?,child:r.get(1)?,version:r.get(2)?,path:r.get(3)?}))?.collect::<Result<_,_>>()?)
        }).await
    }

    pub(crate) async fn reuse_writer_output(
        &self,
        owner: account::AccountOwner,
        generation: String,
        key: WriterKey,
        media: String,
        packet: WriterOutput,
    ) -> Result<(), StateError> {
        self.with_conn_mut("confirming unchanged writer output", move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            account::validate_authenticated(&tx, &owner)?;
            // A preceding physical writer can have settled this alias already.
            let eligible: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM provider_active_destinations w
                 WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5
                 AND local_checksum=?6 AND admitted=1 AND verified_media=1 AND verified_metadata=0
                 AND preserved_local IS NULL AND NOT EXISTS(SELECT 1 FROM provider_active_destinations p
                     WHERE p.library=w.library AND p.asset_id=w.asset_id AND p.path=w.path AND p.prepared_hash IS NOT NULL))",
                params![generation,key.pass,key.child,key.version,key.path,media], |r| r.get(0),
            )?;
            if !eligible {
                return Ok(());
            }
            let root = load_generation(&tx, &generation)?;
            let manifest = load_decision(&tx, &root, &key.pass, &key.child)?;
            let dest = manifest.decision.destinations.iter()
                .find(|d| d.version_size == key.version && d.path.key().as_deref().ok() == Some(key.path.as_str()))
                .ok_or(StateError::ProviderSelectionInvalid)?;
            let pending = match metadata_receipt(&tx, &generation, &key.pass, &key.child, &key.version, &key.path) {
                Ok(pending) => pending,
                Err(StateError::ProviderWorkConflict(_)) => return Ok(()),
                Err(error) => return Err(error),
            };
            let checked = proof(&pending, &root, &manifest, dest, &media, packet)?;
            let prior: Option<(String,String,String)> = tx.query_row(
                "SELECT generation,pass_key,child FROM provider_active_destinations
                 WHERE library=?1 AND asset_id=?2 AND path=?3 AND writer_output_proof=?4
                 AND verified_media=1 AND verified_metadata=1 AND prepared_hash IS NULL AND preserved_local IS NULL LIMIT 1",
                params![pending.asset.library,manifest.state_id,key.path,checked], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)),
            ).optional()?;
            if let Some((gen_id, pass, child)) = prior {
                load_decision(&tx, &load_generation(&tx, &gen_id)?, &pass, &child)?;
                // Passing no publication checksum retains the original publication owner.
                finish_metadata_receipt(&tx,&pending,None,true)?;
            }
            tx.commit()?;
            Ok(())
        }).await
    }
}
