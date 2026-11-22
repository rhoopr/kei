//! Retire original rank responses after their generation no longer needs
//! observed assets for recovery. The original seal still commits to page IDs.

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{
    ActiveGeneration, MAX_CHUNK_BYTES, SelectionSourceProof, StateError, account, digest, encode,
    invalid, load_generation, load_rank, seal_hash_with_pages, validate_rank_request,
};

// Bind the validated projection to the exact original page and ordered record
// index. Equal child names from different passes cannot exchange receipts.
fn receipt_hash(
    conn: &Connection,
    page: &str,
    payload: &[u8],
    child_count: usize,
) -> Result<String, StateError> {
    let mut hash = Sha256::new();
    hash.update(encode(&("rank-retirement-v1", page, digest(payload)))?);
    let mut stmt = conn.prepare("SELECT ordinal,record_name,record_type,deleted FROM provider_selection_rank_records WHERE page_id=?1 ORDER BY ordinal")?;
    let mut rows = stmt.query([page])?;
    let mut observed_relation = false;
    for ordinal in 0..child_count {
        let row = rows.next()?.ok_or(StateError::ProviderSelectionInvalid)?;
        if usize::try_from(row.get::<_, i64>(0)?).ok() != Some(ordinal) {
            return invalid();
        }
        let kind = row.get::<_, Option<String>>(2)?;
        observed_relation |= kind.as_deref() == Some("CPLContainerRelation");
        hash.update(encode(&(
            ordinal,
            row.get::<_, String>(1)?,
            kind,
            row.get::<_, bool>(3)?,
        ))?);
    }
    let has_relation: bool = conn.query_row(
        "SELECT has_relation FROM provider_selection_rank_pages WHERE id=?1",
        [page],
        |row| row.get(0),
    )?;
    if rows.next()?.is_some() || observed_relation != has_relation {
        return invalid();
    }
    Ok(format!("{:x}", hash.finalize()))
}

pub(super) fn load_rank_proof(
    conn: &Connection,
    root: &ActiveGeneration,
    page: &str,
) -> Result<SelectionSourceProof, StateError> {
    let receipt: Option<String> = if root.sealed && root.checkpoint_ready {
        conn.query_row(
            "SELECT retired_proof_hash FROM provider_selection_rank_pages WHERE id=?1",
            [page],
            |row| row.get(0),
        )?
    } else {
        None
    };
    let Some(receipt) = receipt else {
        let raw = load_rank(conn, root, page)?;
        return Ok(SelectionSourceProof {
            pass_key: raw.pass_key,
            selected_children: raw.selected_children,
            membership: raw.membership,
        });
    };
    let (key, request, request_hash, body_hash, proof): (String, Vec<u8>, String, String, Vec<u8>) =
        conn.query_row(
            "SELECT pass_key,request,request_hash,body_hash,body FROM provider_selection_rank_pages
            WHERE id=?1 AND generation=?2 AND length(request)<=?3 AND length(body)<=?3",
            params![
                page,
                root.id,
                i64::try_from(MAX_CHUNK_BYTES)
                    .map_err(|_overflow| StateError::ProviderSelectionInvalid)?
            ],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )?;
    if digest(&request) != request_hash
        || digest(&encode(&(&root.id, &key, &request_hash, &body_hash))?) != page
    {
        return invalid();
    }
    let request: Value = serde_json::from_slice(&request)
        .map_err(|_invalid| StateError::ProviderSelectionInvalid)?;
    validate_rank_request(root, &key, &request)?;
    let selected_children: Vec<Option<String>> =
        serde_json::from_slice(&proof).map_err(|_invalid| StateError::ProviderSelectionInvalid)?;
    if receipt_hash(conn, page, &proof, selected_children.len())? != receipt {
        return invalid();
    }
    Ok(SelectionSourceProof {
        pass_key: key,
        selected_children,
        membership: request.get("zones").is_some(),
    })
}

pub(super) fn retire_completed(
    conn: &mut Connection,
    owner: &account::AccountOwner,
    scope: &str,
) -> Result<(), StateError> {
    // One generation per commit: interruption during an upgrade backlog cannot
    // roll back already retired generations or require a WAL for the whole store.
    loop {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        account::validate_authenticated(&tx, owner)?;
        let id: Option<String> = tx.query_row("SELECT g.id FROM provider_active_generations g
        WHERE g.scope=?1 AND g.sealed=1 AND g.checkpoint_ready=1 AND g.checkpoint_veto IS NULL
        AND EXISTS(SELECT 1 FROM provider_selection_rank_pages p WHERE p.generation=g.id AND p.retired_proof_hash IS NULL)
        AND NOT EXISTS(SELECT 1 FROM provider_active_decisions d WHERE d.generation=g.id AND d.admission='deferred')
        AND NOT EXISTS(SELECT 1 FROM provider_active_destinations w WHERE w.generation=g.id
          AND (w.prepared_hash IS NOT NULL OR w.confirmed_repair IS NOT NULL OR
            (w.admitted=1 AND ((w.verified_media=0 AND w.preserved_local IS NULL) OR
              (g.metadata_enabled=1 AND w.verified_metadata=0))))) LIMIT 1", [scope], |row| row.get(0)).optional()?;
        let Some(id) = id else { return Ok(()) };
        let root = load_generation(&tx, &id)?;
        let expected: String = tx.query_row(
            "SELECT seal_hash FROM provider_active_generations WHERE id=?1",
            [&id],
            |row| row.get(0),
        )?;
        let checked = seal_hash_with_pages(&tx, &root, true, None, |page| {
            let raw = load_rank(&tx, &root, page)?;
            let proof = encode(&raw.selected_children)?;
            let receipt = receipt_hash(&tx, page, &proof, raw.selected_children.len())?;
            let changed = tx.execute("UPDATE provider_selection_rank_pages SET body=?2,retired_proof_hash=?3 WHERE id=?1 AND retired_proof_hash IS NULL",
                params![page, proof, receipt])?;
            if changed != 1 {
                return invalid();
            }
            Ok(())
        })?;
        if checked != expected {
            return invalid();
        }
        tx.commit()?;
    }
}
