//! The metadata writer owns a physical path under its existing destination lock.
//! Transfer only the metadata obligation. A predecessor's media receipt, frozen
//! intent, and any missing-media debt remain independent.

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use super::{
    ActiveGeneration, digest, encode, grouping_names, invalid, load_decision, load_generation,
    metadata_receipt,
};
use crate::state::db::PendingMetadataRewrite;
use crate::state::db::provider_selection::SelectionPath;
use crate::state::error::StateError;

// Both ends retain their independently validated manifests and immutable writer
// intents. A changed or missing target must never hide checkpoint debt.
fn binding(
    conn: &Connection,
    old: &str,
    new: &str,
    key: &str,
    child: &str,
    version: &str,
    path: &str,
) -> Result<Option<String>, StateError> {
    let intents: Option<(String,String)> = conn.query_row(
        "SELECT o.intent_hash,n.intent_hash FROM provider_active_destinations o
         JOIN provider_active_destinations n USING(pass_key,child,version_size,path,library,asset_id,master,checksum,size_bytes,metadata_hash)
         JOIN provider_active_generations a ON a.id=o.generation
         JOIN provider_active_generations b ON b.id=n.generation
         WHERE o.generation=?1 AND n.generation=?2 AND o.pass_key=?3 AND o.child=?4 AND o.version_size=?5 AND o.path=?6
           AND a.scope=b.scope AND a.config_hash=b.config_hash AND a.created_at<b.created_at
           AND a.metadata_enabled=1 AND b.metadata_enabled=1 AND b.sealed=1 AND b.checkpoint_ready=1
           AND json_extract(a.specification,'$.metadata_flags')=json_extract(b.specification,'$.metadata_flags')
           AND o.created_at IS n.created_at AND o.added_at IS n.added_at
           AND o.intent_hash IS NOT NULL AND n.intent_hash IS NOT NULL AND o.grouping_hash<>n.grouping_hash
           AND o.admitted=1 AND n.admitted=1 AND o.verified_metadata=0 AND o.prepared_hash IS NULL",
        params![old,new,key,child,version,path],|r|Ok((r.get(0)?,r.get(1)?))
    ).optional()?;
    intents
        .map(|pair| {
            Ok(digest(&encode(&(
                "grouping-successor-v1",
                old,
                new,
                key,
                child,
                version,
                path,
                pair,
            ))?))
        })
        .transpose()
}

pub(super) fn validate_link(
    conn: &Connection,
    old: &str,
    key: &str,
    child: &str,
    version: &str,
    path: &str,
    link: &str,
) -> Result<(), StateError> {
    let (new, proof): (String, String) =
        serde_json::from_str(link).map_err(|_invalid| StateError::ProviderSelectionInvalid)?;
    if binding(conn, old, &new, key, child, version, path)?.as_ref() != Some(&proof) {
        return invalid();
    }
    Ok(())
}

// The old receipt stores a hash, not a list of names. Prove the entire
// difference by removing one positively observed album from today's inputs.
// This cannot explain a removal, a change to people, or an unrelated rename.
fn observed_addition(
    conn: &Connection,
    root: &ActiveGeneration,
    child: &str,
    asset: &str,
    master: &str,
    library: &str,
    old_group: &str,
) -> Result<bool, StateError> {
    let groups = grouping_names(conn, library, asset)?;
    let mut rows=conn.prepare("SELECT DISTINCT pass_key FROM provider_active_destinations WHERE generation=?1 AND child=?2 AND asset_id=?3 AND master=?4 AND library=?5 AND admitted=1")?;
    let keys = rows.query_map(params![root.id, child, asset, master, library], |r| {
        r.get::<_, String>(0)
    })?;
    for key in keys {
        let key = key?;
        let pass = super::pass(root, &key)?;
        if pass.get("kind").and_then(serde_json::Value::as_str) != Some("Album") {
            continue;
        }
        let Some(name) = pass
            .get("scope")
            .and_then(crate::icloud::photos::selected_album_name)
        else {
            continue;
        };
        let mut previous = groups.clone();
        let albums = previous
            .first_mut()
            .ok_or(StateError::ProviderSelectionInvalid)?;
        let Ok(index) = albums.binary_search_by(|s| s.as_str().cmp(name)) else {
            continue;
        };
        albums.remove(index);
        if digest(&encode(&previous)?) != old_group {
            continue;
        }
        let observed = load_decision(conn, root, &key, child)?;
        if observed.state_id == asset
            && observed.decision.master.as_deref() == Some(master)
            && observed.decision.outcome == super::SelectionOutcome::Selected
            && observed
                .media_zone(root)
                .get("zoneName")
                .and_then(serde_json::Value::as_str)
                == Some(library)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The caller holds the path lock until it finishes publishing. A false result
/// rolls back the entire claim, including any earlier predecessor in this set.
pub(in crate::state::db) fn claim_writer(
    tx: &Transaction<'_>,
    pending: &PendingMetadataRewrite,
) -> Result<bool, StateError> {
    let Some(receipt) = &pending.selection_receipt else {
        return Ok(true);
    };
    let root = load_generation(tx, &receipt.generation)?;
    let version = pending.asset.version_size.as_str();
    let path = SelectionPath::from_path(
        pending
            .asset
            .local_path
            .as_deref()
            .ok_or(StateError::ProviderSelectionInvalid)?,
    )
    .key()?;
    let eligible:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM provider_active_destinations WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5 AND admitted=1 AND verified_metadata=0 AND metadata_successor IS NULL AND (verified_media=1 OR preserved_local IS NOT NULL))",params![root.id,receipt.pass_key,receipt.child,version,path],|r|r.get(0))?;
    if !eligible {
        return Ok(false);
    }
    let current = match metadata_receipt(
        tx,
        &root.id,
        &receipt.pass_key,
        &receipt.child,
        version,
        &path,
    ) {
        Ok(p) => p,
        Err(StateError::ProviderWorkConflict(_)) => return Ok(false),
        Err(e) => return Err(e),
    };
    if current.asset.local_checksum != pending.asset.local_checksum
        || current.source_checksum != pending.source_checksum
        || current.selection_receipt.as_ref().is_none_or(|r| {
            r.grouping_hash != receipt.grouping_hash
                || r.metadata_flags != receipt.metadata_flags
                || r.prepared != receipt.prepared
                || r.preserved_local != receipt.preserved_local
        })
    {
        return Ok(false);
    }
    if receipt.prepared.is_some() {
        return Ok(true);
    }
    let mut stmt=tx.prepare("SELECT w.generation,w.pass_key,w.child,w.master,w.grouping_hash,w.prepared_hash IS NOT NULL OR w.confirmed_repair IS NOT NULL OR w.preserved_local IS NOT NULL
        FROM provider_active_destinations w JOIN provider_active_generations g ON g.id=w.generation
        WHERE w.library=?1 AND w.asset_id=?2 AND w.version_size=?3 AND w.path=?4 AND w.admitted=1 AND w.verified_metadata=0
          AND g.metadata_enabled=1 AND w.grouping_hash<>?5 AND w.metadata_successor IS NULL")?;
    let rows = stmt
        .query_map(
            params![
                pending.asset.library,
                pending.asset.id,
                version,
                path,
                receipt.grouping_hash
            ],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, bool>(5)?,
                ))
            },
        )?
        .collect::<Result<Vec<_>, _>>()?;
    if !rows.is_empty() {
        let latest:String=tx.query_row("SELECT id FROM provider_active_generations WHERE scope=?1 AND config_hash=?2 ORDER BY created_at DESC,id DESC LIMIT 1",params![root.spec.scope,root.spec.config_hash],|r|r.get(0))?;
        if latest != root.id || super::basis(tx, &root.spec.scope)? != root.spec.basis {
            return Ok(false);
        }
    }
    for (old, key, child, master, group, protected) in rows {
        if protected || !root.sealed || !root.checkpoint_ready || child != receipt.child {
            return Ok(false);
        }
        let Some(proof) = binding(tx, &old, &root.id, &key, &child, version, &path)? else {
            return Ok(false);
        };
        // The queue elects one writer per physical input, not one per pass.
        // Keep the successor link pass-specific, and prove that this pass's
        // successor can be settled by the representative's publication.
        if key != receipt.pass_key {
            let compatible: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM provider_active_destinations w
                 JOIN provider_active_destinations n USING(generation,child,version_size,path,library,
                     asset_id,master,checksum,size_bytes,metadata_hash,local_checksum,grouping_hash)
                 WHERE w.generation=?1 AND w.child=?2 AND w.version_size=?3 AND w.path=?4
                   AND w.pass_key=?5 AND n.pass_key=?6 AND n.admitted=1 AND n.verified_metadata=0
                   AND (n.verified_media=1 OR n.preserved_local IS NOT NULL)
                   AND n.prepared_hash IS NULL AND n.metadata_successor IS NULL
                   AND n.created_at IS w.created_at AND n.added_at IS w.added_at
                   AND n.source_checksum IS w.source_checksum
                   AND (n.preserved_local IS NULL)=(w.preserved_local IS NULL)
                   AND json_extract(n.preserved_local,'$.size') IS json_extract(w.preserved_local,'$.size')
                   AND json_extract(n.preserved_local,'$.modified') IS json_extract(w.preserved_local,'$.modified'))",
                params![root.id,child,version,path,receipt.pass_key,key], |r|r.get(0)
            )?;
            if !compatible {
                return Ok(false);
            }
            load_decision(tx, &root, &key, &child)?;
        }
        load_decision(tx, &load_generation(tx, &old)?, &key, &child)?;
        if !observed_addition(
            tx,
            &root,
            &child,
            &pending.asset.id,
            &master,
            &pending.asset.library,
            &group,
        )? {
            return Ok(false);
        }
        let link = serde_json::to_string(&(&root.id, proof))
            .map_err(|_invalid| StateError::ProviderSelectionInvalid)?;
        let changed=tx.execute("UPDATE provider_active_destinations SET metadata_successor=?6 WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5 AND prepared_hash IS NULL AND metadata_successor IS NULL",params![old,key,child,version,path,link])?;
        if changed != 1 {
            return invalid();
        }
    }
    Ok(true)
}
