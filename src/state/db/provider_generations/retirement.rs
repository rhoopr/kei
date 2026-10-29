//! Completion handoff. Unfinished owners retain their original dependencies.
use super::{
    ActiveDecision, ActiveGeneration, digest, encode, guard_library_scope, invalid, load_decision,
    load_generation, owner_keys, pass, refresh_admission_hash, refresh_progress_hash, seal_hash,
    seal_header,
};
use crate::state::db::download_receipts::{self, DownloadReceipt};
use crate::state::db::provider_selection::{SelectionDestination, SelectionOutcome};
use crate::state::error::StateError;
use rusqlite::{Connection, params};
use serde_json::Value;
use std::collections::HashSet;

pub(super) fn receipt(
    conn: &Connection,
    root: &ActiveGeneration,
    manifest: &ActiveDecision,
    destination: &SelectionDestination,
) -> Result<DownloadReceipt, StateError> {
    let (source, local): (String, String) = conn.query_row(
        "SELECT source_checksum,local_checksum FROM provider_active_destinations
         WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5
         AND admitted=1 AND verified_media=1",
        params![
            root.id,
            manifest.decision.pass_key,
            manifest.decision.child,
            destination.version_size,
            destination.path.key()?
        ],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let (account_key, provider_key) = owner_keys(conn)?;
    Ok(DownloadReceipt {
        account_key,
        provider_key,
        scope: root.spec.scope.clone(),
        library: root
            .spec
            .zone
            .get("zoneName")
            .and_then(Value::as_str)
            .ok_or(StateError::ProviderSelectionInvalid)?
            .to_owned(),
        child: manifest.decision.child.clone(),
        master: manifest
            .decision
            .master
            .clone()
            .ok_or(StateError::ProviderSelectionInvalid)?,
        state_id: manifest.state_id.clone(),
        origin_pass: manifest.decision.pass_key.clone(),
        version_size: destination.version_size.clone(),
        provider_checksum: destination.checksum.clone(),
        provider_size: destination.size,
        native_path: destination.path.clone(),
        source_checksum: source,
        local_checksum: local,
    })
}

fn local_completion(
    conn: &Connection,
    root: &ActiveGeneration,
    manifest: &ActiveDecision,
    destination: &SelectionDestination,
) -> Result<Option<download_receipts::LocalTrustCompletion>, StateError> {
    let (source, local, progress, intent, metadata): (Option<String>, Option<String>, Option<String>, Option<String>, bool) =
        conn.query_row("SELECT source_checksum,local_checksum,progress_hash,intent_hash,verified_metadata FROM provider_active_destinations WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5",
            params![root.id,manifest.decision.pass_key,manifest.decision.child,destination.version_size,destination.path.key()?],
            |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?;
    if source.is_some() {
        return Ok(None);
    }
    let local = local.ok_or(StateError::ProviderSelectionInvalid)?;
    let progress = progress.ok_or(StateError::ProviderSelectionInvalid)?;
    if !super::validate_local_trust_progress(
        conn,
        root,
        manifest,
        destination,
        intent.as_deref(),
        metadata,
        &local,
        &progress,
    )? {
        return invalid();
    }
    let receipt =
        super::local_trust_receipt(&progress).ok_or(StateError::ProviderSelectionInvalid)?;
    let (account_key, provider_key) = owner_keys(conn)?;
    Ok(Some(download_receipts::LocalTrustCompletion {
        version: 1,
        account_key,
        provider_key,
        scope: root.spec.scope.clone(),
        library: root
            .spec
            .zone
            .get("zoneName")
            .and_then(Value::as_str)
            .ok_or(StateError::ProviderSelectionInvalid)?
            .to_owned(),
        child: manifest.decision.child.clone(),
        master: manifest
            .decision
            .master
            .clone()
            .ok_or(StateError::ProviderSelectionInvalid)?,
        state_id: manifest.state_id.clone(),
        origin_pass: manifest.decision.pass_key.clone(),
        version_size: destination.version_size.clone(),
        provider_checksum: destination.checksum.clone(),
        provider_size: destination.size,
        native_path: destination.path.clone(),
        local_checksum: local,
        trust_receipt: receipt.to_owned(),
        selected_metadata_hash: destination.metadata_hash.clone(),
        generation_id: root.id.clone(),
    }))
}

fn same_receipt(left: &DownloadReceipt, right: &DownloadReceipt) -> bool {
    let mut left = left.clone();
    left.origin_pass = right.origin_pass.clone();
    left == *right
}

// Bind the remaining normalized accounting rows as well as the compact manifest.
// The marker is not content proof: its destinations must independently resolve
// to validated download receipts below.
fn marker_hash(
    conn: &Connection,
    root: &ActiveGeneration,
    manifest: &ActiveDecision,
) -> Result<String, StateError> {
    let accounting: String = conn.query_row(
        "SELECT json_array(outcome,admission,reason,admission_hash,attempts,next_retry_at)
        FROM provider_active_decisions WHERE generation=?1 AND pass_key=?2 AND child=?3",
        params![root.id, manifest.decision.pass_key, manifest.decision.child],
        |r| r.get(0),
    )?;
    let rows:Vec<String>=conn.prepare("SELECT json_array(library,asset_id,master,version_size,path,compat_path,checksum,size_bytes,created_at,added_at,metadata_hash,admitted,verified_media,verified_metadata,local_checksum,source_checksum,grouping_hash,intent_hash,progress_hash,prepared_checksum,prepared_size,prepared_hash,frozen_metadata,frozen_metadata_hash)
        FROM provider_active_destinations WHERE generation=?1 AND pass_key=?2 AND child=?3 ORDER BY version_size,path")?
        .query_map(params![root.id,manifest.decision.pass_key,manifest.decision.child],|r|r.get(0))?.collect::<Result<_,_>>()?;
    Ok(digest(&encode(&(&root.id, manifest, accounting, rows))?))
}

pub(super) fn validate_compacted(
    conn: &Connection,
    root: &ActiveGeneration,
    manifest: &ActiveDecision,
    stored: &str,
) -> Result<(), StateError> {
    if !matches!(root.spec.format, 1 | 2)
        || (root.spec.format == 1 && !root.sealed)
        || manifest.decision.confirmation.is_some()
        || !manifest.sources.is_empty()
        || manifest.decision.outcome == SelectionOutcome::Deferred
        || marker_hash(conn, root, manifest)? != stored
    {
        return invalid();
    }
    pass(root, &manifest.decision.pass_key)?;
    let sources:i64=conn.query_row("SELECT count(*) FROM provider_active_sources WHERE generation=?1 AND pass_key=?2 AND child=?3",
        params![root.id,manifest.decision.pass_key,manifest.decision.child],|r|r.get(0))?;
    let count:i64=conn.query_row("SELECT count(*) FROM provider_active_destinations WHERE generation=?1 AND pass_key=?2 AND child=?3",
        params![root.id,manifest.decision.pass_key,manifest.decision.child],|r|r.get(0))?;
    if sources != 0 || usize::try_from(count).ok() != Some(manifest.decision.destinations.len()) {
        return invalid();
    }
    for destination in &manifest.decision.destinations {
        let complete:bool=conn.query_row("SELECT admitted=1 AND verified_media=1 AND verified_metadata=1 AND prepared_hash IS NULL AND frozen_metadata IS NULL FROM provider_active_destinations WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5",
            params![root.id,manifest.decision.pass_key,manifest.decision.child,destination.version_size,destination.path.key()?],|row|row.get(0))?;
        if !complete {
            return invalid();
        }
        if let Some(expected) = local_completion(conn, root, manifest, destination)? {
            if download_receipts::load_local_trust(conn, &expected.id()?)?.as_ref()
                != Some(&expected)
            {
                return invalid();
            }
        } else {
            let expected = receipt(conn, root, manifest, destination)?;
            let actual = download_receipts::load(conn, &expected.id()?)?
                .ok_or(StateError::ProviderSelectionInvalid)?;
            if !same_receipt(&expected, &actual) {
                return invalid();
            }
        }
    }
    Ok(())
}

/// A proven prepared transfer is consumed in the same transaction as its
/// finished path receipt. No caller boolean can assert this provenance.
pub(in crate::state::db) fn record_prepared(
    conn: &Connection,
    proof: &super::super::PreparedPublication,
) -> Result<(), StateError> {
    let path = proof.destination.key()?;
    let keys:Vec<(String,String,String)>=conn.prepare("SELECT w.generation,w.pass_key,w.child FROM provider_active_destinations w
        JOIN provider_active_generations g ON g.id=w.generation
        WHERE w.library=?1 AND w.asset_id=?2 AND w.version_size=?3 AND w.path=?4
          AND w.checksum=?5 AND w.size_bytes=?6 AND w.local_checksum=?7 AND w.source_checksum=?8
          AND w.admitted=1 AND w.verified_media=1 AND json_extract(CAST(g.specification AS TEXT),'$.format')=2")?
        .query_map(params![proof.library,proof.id,proof.version,path,proof.provider_checksum,i64::try_from(proof.provider_size).map_err(|_invalid|StateError::ProviderSelectionInvalid)?,proof.local_checksum,proof.source_checksum],
            |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?.collect::<Result<_,_>>()?;
    for (generation, key, child) in keys {
        let root = load_generation(conn, &generation)?;
        let manifest = load_decision(conn, &root, &key, &child)?;
        let destination = manifest
            .decision
            .destinations
            .iter()
            .find(|d| d.version_size == proof.version && d.path == proof.destination)
            .ok_or(StateError::ProviderSelectionInvalid)?;
        receipt(conn, &root, &manifest, destination)?.save(conn)?;
    }
    Ok(())
}

/// One bounded batch, usable before a charge while a baseline is still open.
pub(super) fn compact_batch(
    conn: &Connection,
    only: Option<&str>,
    format: u8,
) -> Result<usize, StateError> {
    let keys:Vec<(String,String,String)>=conn.prepare("SELECT d.generation,d.pass_key,d.child FROM provider_active_decisions d
        JOIN provider_active_generations g ON g.id=d.generation
        WHERE d.compacted_hash IS NULL AND (?1 IS NULL OR d.generation=?1)
          AND json_extract(CAST(g.specification AS TEXT),'$.format')=?2
          AND ((json_extract(CAST(g.specification AS TEXT),'$.format')=2)
            OR (json_extract(CAST(g.specification AS TEXT),'$.format')=1 AND g.sealed=1))
          AND (d.admission='excluded' OR (d.admission='admitted'
            AND NOT EXISTS(SELECT 1 FROM provider_active_destinations w WHERE w.generation=d.generation AND w.pass_key=d.pass_key AND w.child=d.child
                AND (w.admitted=0 OR w.verified_media=0 OR (g.metadata_enabled=1 AND w.verified_metadata=0) OR w.prepared_hash IS NOT NULL OR w.frozen_metadata IS NOT NULL))))
          AND NOT EXISTS(SELECT 1 FROM provider_active_destinations w
            WHERE w.generation=d.generation AND w.pass_key=d.pass_key AND w.child=d.child
              AND ((w.source_checksum<>w.local_checksum OR EXISTS(SELECT 1 FROM primary_layout_bindings b WHERE b.library=w.library AND b.child=w.asset_id))
                AND NOT EXISTS(SELECT 1 FROM download_receipts r WHERE r.scope=g.scope AND r.child=w.child AND r.master=w.master
                  AND r.state_id=w.asset_id AND r.version_size=w.version_size AND r.provider_checksum=w.checksum
                  AND r.provider_size=w.size_bytes AND r.native_path=w.path AND r.local_checksum=w.local_checksum AND r.source_checksum=w.source_checksum)))
          AND NOT EXISTS(SELECT 1 FROM provider_active_destinations w JOIN owned_temp_files t
            ON json_extract(t.publication,'$.library')=w.library AND json_extract(t.publication,'$.id')=w.asset_id
            AND json_extract(t.publication,'$.version')=w.version_size
            WHERE w.generation=d.generation AND w.pass_key=d.pass_key AND w.child=d.child AND t.publication IS NOT NULL)
        ORDER BY g.created_at,d.generation,d.pass_key,d.child LIMIT 64")?
        .query_map(params![only,format],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?.collect::<Result<_,_>>()?;
    let mut compacted = 0;
    let mut changed_roots = HashSet::new();
    let mut validated_legacy_roots = HashSet::new();
    for (generation, key, child) in keys {
        let root = load_generation(conn, &generation)?;
        if root.spec.format == 1 && validated_legacy_roots.insert(generation.clone()) {
            #[cfg(test)]
            count_legacy_validation(&generation);
            let saved: String = conn.query_row(
                "SELECT seal_hash FROM provider_active_generations WHERE id=?1",
                [&generation],
                |row| row.get(0),
            )?;
            if !root.sealed
                || seal_hash(
                    conn,
                    &root,
                    root.checkpoint_ready,
                    root.checkpoint_veto.as_deref(),
                )? != saved
            {
                return invalid();
            }
        }
        let mut manifest = load_decision(conn, &root, &key, &child)?;
        let mut receipts = Vec::new();
        let mut local_receipts = Vec::new();
        let mut eligible = true;
        for destination in &manifest.decision.destinations {
            if let Some(local) = local_completion(conn, &root, &manifest, destination)? {
                local_receipts.push(local);
                continue;
            }
            let expected = receipt(conn, &root, &manifest, destination)?;
            let existing = download_receipts::load(conn, &expected.id()?)?;
            if let Some(existing) = existing {
                if !same_receipt(&expected, &existing) {
                    return Err(StateError::ProviderWorkConflict);
                }
            } else {
                // Only unchanged source bytes can bootstrap old progress.
                // Modified historic output has no trustworthy origin here.
                let layout:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM primary_layout_bindings WHERE library=?1 AND child=?2)",
                    params![expected.library,expected.state_id],|r|r.get(0))?;
                if expected.source_checksum != expected.local_checksum || layout {
                    eligible = false;
                    break;
                }
            }
            let unfinished: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM owned_temp_files WHERE publication IS NOT NULL
                AND json_extract(publication,'$.library')=?1 AND json_extract(publication,'$.id')=?2
                AND json_extract(publication,'$.version')=?3)",
                params![expected.library, expected.state_id, expected.version_size],
                |r| r.get(0),
            )?;
            if unfinished {
                eligible = false;
                break;
            }
            receipts.push(expected);
        }
        if !eligible {
            continue;
        }
        for receipt in receipts {
            receipt.save(conn)?;
        }
        for receipt in local_receipts {
            receipt.save(conn)?;
        }
        #[cfg(all(test, target_os = "linux"))]
        crate::test_helpers::process_death_point("completion-receipts-saved");
        manifest.decision.confirmation = None;
        manifest.sources.clear();
        conn.execute(
            "DELETE FROM provider_active_sources WHERE generation=?1 AND pass_key=?2 AND child=?3",
            params![generation, key, child],
        )?;
        let bytes = encode(&manifest)?;
        conn.execute(
            "UPDATE provider_active_decisions SET manifest=?4,manifest_hash=?5,charged_bytes=?6
            WHERE generation=?1 AND pass_key=?2 AND child=?3",
            params![
                generation,
                key,
                child,
                bytes,
                digest(&bytes),
                i64::try_from(bytes.len() + 256)
                    .map_err(|_invalid| StateError::ProviderSelectionFull)?
            ],
        )?;
        refresh_admission_hash(conn, &root, &key, &child)?;
        conn.execute(
            "UPDATE provider_active_destinations SET verified_metadata=1
            WHERE generation=?1 AND pass_key=?2 AND child=?3",
            params![generation, key, child],
        )?;
        for destination in &manifest.decision.destinations {
            refresh_progress_hash(conn, &root, &manifest, destination)?;
        }
        let hash = marker_hash(conn, &root, &manifest)?;
        conn.execute("UPDATE provider_active_decisions SET compacted_hash=?4 WHERE generation=?1 AND pass_key=?2 AND child=?3",
            params![generation,key,child,hash])?;
        #[cfg(all(test, target_os = "linux"))]
        crate::test_helpers::process_death_point("completion-manifest-retired");
        compacted += 1;
        if root.sealed {
            changed_roots.insert(generation);
        }
    }
    for id in changed_roots {
        let root = load_generation(conn, &id)?;
        let hash = seal_hash(
            conn,
            &root,
            root.checkpoint_ready,
            root.checkpoint_veto.as_deref(),
        )?;
        let header = seal_header(
            &root,
            root.checkpoint_ready,
            root.checkpoint_veto.as_deref(),
            &hash,
        )?;
        conn.execute(
            "UPDATE provider_active_generations SET seal_hash=?2,seal_header_hash=?3 WHERE id=?1",
            params![id, hash, header],
        )?;
    }
    Ok(compacted)
}

/// Called only after a successor root exists, before that root is handed to
/// the producer. Coverage cursors live in checkpoint metadata, not these rows.
pub(super) fn retire_completed_roots(
    conn: &Connection,
    keep: &str,
    format: u8,
) -> Result<(), StateError> {
    let ids:Vec<String>=conn.prepare(&format!("SELECT g.id FROM provider_active_generations g
        WHERE g.id<>?1 AND g.scope=(SELECT scope FROM provider_active_generations WHERE id=?1)
          AND json_extract(CAST(g.specification AS TEXT),'$.format')=?2
          AND ((json_extract(CAST(g.specification AS TEXT),'$.format')=2)
            OR (json_extract(CAST(g.specification AS TEXT),'$.format')=1 AND g.sealed=1))
          AND (EXISTS(SELECT 1 FROM provider_active_decisions d WHERE d.generation=g.id AND d.compacted_hash IS NOT NULL)
               OR NOT EXISTS(SELECT 1 FROM provider_active_decisions d WHERE d.generation=g.id))
          AND (json_extract(CAST(g.specification AS TEXT),'$.format')=1
            OR NOT EXISTS(SELECT 1 FROM provider_selection_rank_pages p WHERE p.generation=g.id))
          AND (json_extract(CAST(g.specification AS TEXT),'$.format')=2 OR ({LEGACY_RANK_ACCOUNTED}))
        ORDER BY g.created_at,g.id LIMIT 64"))?.query_map(params![keep,format],|r|r.get(0))?.collect::<Result<_,_>>()?;
    for id in ids {
        let root = load_generation(conn, &id)?;
        let actual = seal_hash(
            conn,
            &root,
            root.checkpoint_ready,
            root.checkpoint_veto.as_deref(),
        )?;
        if root.sealed {
            let saved: String = conn.query_row(
                "SELECT seal_hash FROM provider_active_generations WHERE id=?1",
                [&id],
                |r| r.get(0),
            )?;
            if saved != actual {
                return invalid();
            }
        }
        if root.spec.format == 1 && !legacy_rank_fully_accounted(conn, &root)? {
            // Master reconstruction can depend on pages not listed in a
            // decision's active_sources. Mixed roots keep every rank page and
            // compact marker until all observed work is positively accounted.
            continue;
        }
        let library = root
            .spec
            .zone
            .get("zoneName")
            .and_then(Value::as_str)
            .ok_or(StateError::ProviderSelectionInvalid)?;
        guard_library_scope(conn, &root.spec.scope, library)?;
        conn.execute(
            "INSERT INTO metadata(key,value) VALUES(?1,?2) ON CONFLICT(key) DO NOTHING",
            params![format!("download_ledger_scope:{library}"), root.spec.scope],
        )?;
        #[cfg(all(test, target_os = "linux"))]
        crate::test_helpers::process_death_point("completion-before-root-retirement");
        // A retired completed row cannot remain the restart cursor for a
        // mixed root. Resetting replay only revisits independently owned work.
        conn.execute("UPDATE provider_active_generations SET replay_after=NULL WHERE id=?1 AND EXISTS(
            SELECT 1 FROM provider_active_decisions d WHERE d.generation=?1 AND d.compacted_hash IS NOT NULL
            AND d.pass_key=json_extract(provider_active_generations.replay_after,'$[0]')
            AND d.child=json_extract(provider_active_generations.replay_after,'$[1]'))",[&id])?;
        conn.execute("DELETE FROM provider_active_destinations WHERE generation=?1 AND EXISTS(
            SELECT 1 FROM provider_active_decisions d WHERE d.generation=provider_active_destinations.generation
            AND d.pass_key=provider_active_destinations.pass_key AND d.child=provider_active_destinations.child
            AND d.compacted_hash IS NOT NULL)",[&id])?;
        conn.execute("DELETE FROM provider_active_decisions WHERE generation=?1 AND compacted_hash IS NOT NULL",[&id])?;
        let remaining: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM provider_active_decisions WHERE generation=?1)",
            [&id],
            |r| r.get(0),
        )?;
        if !remaining {
            if root.spec.format == 1 {
                conn.execute("DELETE FROM provider_selection_rank_records WHERE page_id IN (SELECT id FROM provider_selection_rank_pages WHERE generation=?1)",[&id])?;
                conn.execute(
                    "DELETE FROM provider_selection_rank_pages WHERE generation=?1",
                    [&id],
                )?;
            }
            conn.execute("DELETE FROM provider_active_generations WHERE id=?1", [&id])?;
        } else if root.sealed {
            let hash = seal_hash(
                conn,
                &root,
                root.checkpoint_ready,
                root.checkpoint_veto.as_deref(),
            )?;
            let header = seal_header(
                &root,
                root.checkpoint_ready,
                root.checkpoint_veto.as_deref(),
                &hash,
            )?;
            conn.execute("UPDATE provider_active_generations SET seal_hash=?2,seal_header_hash=?3 WHERE id=?1",
                params![id,hash,header])?;
        }
    }
    Ok(())
}

/// A seal proves the retained representation, not that every rank item was
/// admitted. Require a positive terminal owner for assets and their masters;
/// unfamiliar records and interrupted partial admission keep the whole root.
fn legacy_rank_fully_accounted(
    conn: &Connection,
    root: &ActiveGeneration,
) -> Result<bool, StateError> {
    if !root.sealed {
        return Ok(false);
    }
    conn.query_row(
        &format!("SELECT {LEGACY_RANK_ACCOUNTED} FROM provider_active_generations g WHERE g.id=?1"),
        [&root.id],
        |row| row.get(0),
    )
    .map_err(StateError::from)
}

const LEGACY_RANK_ACCOUNTED: &str = r"
        NOT EXISTS(SELECT 1 FROM provider_active_decisions WHERE generation=g.id AND compacted_hash IS NULL)
        AND NOT EXISTS(SELECT 1 FROM provider_active_sources WHERE generation=g.id)
        AND EXISTS(SELECT 1 FROM provider_selection_rank_pages WHERE generation=g.id)
        AND NOT EXISTS(
            SELECT 1 FROM provider_selection_rank_records r JOIN provider_selection_rank_pages p ON p.id=r.page_id
            WHERE p.generation=g.id AND NOT COALESCE((
                (r.record_type='CPLAsset' AND EXISTS(
                    SELECT 1 FROM provider_active_decisions d WHERE d.generation=g.id AND d.pass_key=p.pass_key AND d.child=r.record_name AND d.compacted_hash IS NOT NULL))
                OR (r.record_type='CPLMaster' AND EXISTS(
                    SELECT 1 FROM provider_active_decisions d WHERE d.generation=g.id AND d.pass_key=p.pass_key AND d.compacted_hash IS NOT NULL
                    AND json_extract(CAST(d.manifest AS TEXT),'$.decision.master')=r.record_name))
            ),0)
        )";

// Legacy completion reclamation is opportunistic. It must never become a
// prerequisite for current admission or undo required unfinished-owner freezing.
pub(super) fn optional_legacy_cleanup(conn: &Connection, keep: &str) -> Result<(), StateError> {
    conn.execute_batch("SAVEPOINT v27_optional_legacy_cleanup")?;
    let outcome = (|| {
        compact_batch(conn, None, 1)?;
        retire_completed_roots(conn, keep, 1)
    })();
    if let Err(error) = outcome {
        conn.execute_batch(
            "ROLLBACK TO v27_optional_legacy_cleanup; RELEASE v27_optional_legacy_cleanup",
        )?;
        tracing::warn!(%error,"Optional legacy completion cleanup deferred; original owners retained");
    } else {
        conn.execute_batch("RELEASE v27_optional_legacy_cleanup")?;
    }
    Ok(())
}

#[cfg(test)]
static LEGACY_VALIDATIONS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, usize>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

#[cfg(test)]
fn count_legacy_validation(generation: &str) {
    if let Ok(mut counts) = LEGACY_VALIDATIONS.lock() {
        *counts.entry(generation.to_owned()).or_default() += 1;
    }
}

#[cfg(test)]
pub(crate) fn take_legacy_compaction_validation_count(generation: &str) -> usize {
    LEGACY_VALIDATIONS
        .lock()
        .ok()
        .and_then(|mut counts| counts.remove(generation))
        .unwrap_or_default()
}

/// A validated prepared rewrite advances the same physical resource's completion
/// proof. Retained compact owners must move in the same transaction as its receipt.
pub(in crate::state::db) fn advance_completed_receipt(
    conn: &Connection,
    previous: &DownloadReceipt,
    completed: &DownloadReceipt,
) -> Result<(), StateError> {
    if conn.is_autocommit() {
        return invalid();
    }
    let keys: Vec<(String,String,String)> = conn.prepare(
        "SELECT w.generation,w.pass_key,w.child FROM provider_active_destinations w
         JOIN provider_active_generations g ON g.id=w.generation
         JOIN provider_active_decisions d ON d.generation=w.generation AND d.pass_key=w.pass_key AND d.child=w.child
         WHERE g.scope=?1 AND w.child=?2 AND w.master=?3 AND w.version_size=?4
           AND w.checksum=?5 AND w.size_bytes=?6 AND w.path=?7
           AND w.admitted=1 AND w.verified_media=1 AND w.verified_metadata=1
           AND w.prepared_hash IS NULL AND w.frozen_metadata IS NULL"
    )?.query_map(params![previous.scope,previous.child,previous.master,previous.version_size,
        previous.provider_checksum,previous.provider_size as i64,previous.native_path.key()?],
        |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?.collect::<Result<_,_>>()?;
    // Validate all owners against the old receipt before changing any proof.
    let mut owners = Vec::with_capacity(keys.len());
    let mut seals = std::collections::BTreeMap::new();
    for (id, key, child) in keys {
        let root = load_generation(conn, &id)?;
        if root.sealed && !seals.contains_key(&id) {
            let saved: String = conn.query_row(
                "SELECT seal_hash FROM provider_active_generations WHERE id=?1",
                [&id],
                |r| r.get(0),
            )?;
            if seal_hash(
                conn,
                &root,
                root.checkpoint_ready,
                root.checkpoint_veto.as_deref(),
            )? != saved
            {
                return invalid();
            }
            seals.insert(id.clone(), saved);
        }
        let manifest = load_decision(conn, &root, &key, &child)?;
        let destination = manifest
            .decision
            .destinations
            .iter()
            .find(|d| d.version_size == previous.version_size && d.path == previous.native_path)
            .ok_or(StateError::ProviderSelectionInvalid)?;
        let owner_receipt = receipt(conn, &root, &manifest, destination)?;
        if !same_receipt(&owner_receipt, previous) && !same_receipt(&owner_receipt, completed) {
            return invalid();
        }
        let compacted: bool = conn.query_row(
            "SELECT compacted_hash IS NOT NULL FROM provider_active_decisions WHERE generation=?1 AND pass_key=?2 AND child=?3",
            params![id,key,child], |r|r.get(0))?;
        owners.push((root, manifest, compacted));
    }
    for (root, manifest, compacted) in &owners {
        let destination = manifest
            .decision
            .destinations
            .iter()
            .find(|d| d.version_size == previous.version_size && d.path == previous.native_path)
            .ok_or(StateError::ProviderSelectionInvalid)?;
        conn.execute(
            "UPDATE provider_active_destinations SET local_checksum=?6
            WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5",
            params![
                root.id,
                manifest.decision.pass_key,
                manifest.decision.child,
                previous.version_size,
                previous.native_path.key()?,
                completed.local_checksum
            ],
        )?;
        refresh_progress_hash(conn, root, manifest, destination)?;
        if *compacted {
            conn.execute(
                "UPDATE provider_active_decisions SET compacted_hash=?4
            WHERE generation=?1 AND pass_key=?2 AND child=?3",
                params![
                    root.id,
                    manifest.decision.pass_key,
                    manifest.decision.child,
                    marker_hash(conn, root, manifest)?
                ],
            )?;
        }
    }
    download_receipts::replace_validated_local(conn, previous, &completed.local_checksum)?;
    // Only completion hashes changed, not coverage or frozen groupings.
    // Preserve the original seal; never re-seal potentially damaged inventory.
    for (id, saved) in seals {
        let root = load_generation(conn, &id)?;
        if seal_hash(
            conn,
            &root,
            root.checkpoint_ready,
            root.checkpoint_veto.as_deref(),
        )? != saved
        {
            return invalid();
        }
    }
    Ok(())
}
