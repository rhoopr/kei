//! Bounded scoped selection roots and independently verified destinations.
//!
//! Rank coverage is observational. Neither these roots nor their page IDs own a
//! provider cursor, determine a current version, prove absence or retire debt.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::asset_writes::upsert_asset_row;
use super::contracts::PreservedLocal;
use super::membership::refresh_asset_album_groupings_tx;
use super::provider_inbox::{CapturedPageId, load_page};
use super::provider_selection::{
    SelectionDecision, SelectionDestination, SelectionMediaSource, SelectionOutcome,
};
use super::rows::{decode_asset_date, encode_asset_date};
use super::{SqliteStateDb, account};
use crate::state::{AssetRecord, error::StateError};

mod grouping_handoff;
mod rank_retirement;
pub(super) use grouping_handoff::claim_writer;
mod writer_output;
pub(crate) use super::contracts::WriterOutput;
use rank_retirement::load_rank_proof;
pub(crate) use writer_output::WriterKey;

// Limit one generation evidence write; retained history never spends this allowance.
pub(crate) const MAX_GENERATION_BYTES: u64 = 512 * 1024 * 1024;
pub(crate) const INDEPENDENT_ALBUM_COPIES: &str = "independent_album_copies";
const ACCEPTED_REPLAY_COVERAGE: &str = "accepted_unfiled_replay";
const ACCEPTED_ALBUM_REPLAY_COVERAGE: &str = "accepted_named_album_replay";
const ACCEPTED_FAVORITES_REPLAY_COVERAGE: &str = "accepted_favorites_replay";
const MIXED_FAVORITES_REPLAY: &str = "accepted_favorites_unfiled_replay";
const ACCEPTED_FOREIGN_REPLAY_COVERAGE: &str = "accepted_foreign_album_replay";
const MAX_CHUNK_BYTES: usize = 16 * 1024 * 1024;
// Original serialized inputs per validation operation, not an allocator or disk bound.
const MAX_VALIDATION_BYTES: usize = 512 * 1024 * 1024;
const MAX_OBSERVED_BYTES: usize = 64 * 1024 * 1024;
const RETAINED_REPLAY_DECISION_LIMIT: usize = 64;
const RETAINED_REPLAY_CANDIDATE_LIMIT: i64 = 128;

const MANAGED_SUCCESSOR_PREFIX: &str = "managed_successor:";

fn album_pass_scope<'a>(root: &'a ActiveGeneration, key: &str) -> Option<&'a Value> {
    root.spec
        .profile
        .get("passes")?
        .as_array()?
        .iter()
        .find(|pass| pass.get("key").and_then(Value::as_str) == Some(key))?
        .get("scope")
}

fn preservation_intent(
    root: &ActiveGeneration,
    manifest: &ActiveDecision,
    destination: &SelectionDestination,
) -> Result<String, StateError> {
    Ok(digest(&encode(&(
        "preserved-local-v1",
        &root.id,
        &root.spec,
        &manifest.decision.pass_key,
        &manifest.decision.child,
        &manifest.decision.master,
        destination,
    ))?))
}

fn preserved_local(
    conn: &Connection,
    root: &ActiveGeneration,
    manifest: &ActiveDecision,
    destination: &SelectionDestination,
) -> Result<Option<PreservedLocal>, StateError> {
    let bytes: Option<Vec<u8>> = conn.query_row(
        "SELECT preserved_local FROM provider_active_destinations WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5 AND (preserved_local IS NULL OR length(preserved_local)<=1024)",
        params![root.id, manifest.decision.pass_key, manifest.decision.child, destination.version_size, destination.path.key()?],
        |row| row.get(0),
    )?;
    let Some(bytes) = bytes else { return Ok(None) };
    if bytes.len() > 1024 {
        return invalid();
    }
    let evidence: PreservedLocal =
        serde_json::from_slice(&bytes).map_err(|_invalid| StateError::ProviderSelectionInvalid)?;
    if evidence.intent != preservation_intent(root, manifest, destination)?
        || evidence.checksum.len() != 64
        || !evidence
            .checksum
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
        || evidence.size > i64::MAX as u64
    {
        return invalid();
    }
    Ok(Some(evidence))
}

fn preserved_progress_hash(
    root: &ActiveGeneration,
    manifest: &ActiveDecision,
    destination: &SelectionDestination,
    intent: Option<&str>,
    metadata: bool,
    evidence: &PreservedLocal,
) -> Result<String, StateError> {
    Ok(digest(&encode(&(
        preservation_intent(root, manifest, destination)?,
        intent,
        metadata,
        evidence,
    ))?))
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManagedSuccessor {
    generation: String,
    manifest_hash: String,
    operation: String,
    operation_hash: String,
    prior_reason: String,
}

fn cancelled_layout(
    conn: &Connection,
    operation: &str,
) -> Result<super::primary_layout::LayoutOperation, StateError> {
    let header: Vec<u8> = conn.query_row(
        "SELECT header FROM primary_layout_operations WHERE operation=?1 AND phase='cancelled' AND length(header)<=1048576",
        [operation], |row| row.get(0),
    )?;
    let mut op: super::primary_layout::LayoutOperation = super::primary_layout::decode(&header)?;
    if op.operation != operation
        || !op.members.is_empty()
        || !matches!(op.phase.as_str(), "planned" | "prepared" | "preserved")
    {
        return invalid();
    }
    let matches: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM primary_layout_operations WHERE operation=?1 AND family=?2 AND library=?3 AND source_library=?4 AND prior_generation=?5 AND decision=?6 AND policy=?7 AND phase='cancelled')",
        params![operation,op.family,op.library,op.source_library,op.generation,op.decision,op.policy], |row| row.get(0),
    )?;
    if !matches {
        return invalid();
    }
    op.members = conn.prepare("SELECT evidence FROM primary_layout_members WHERE operation=?1 ORDER BY member LIMIT 17")?
        .query_map([operation], |row| row.get::<_, Vec<u8>>(0))?
        .map(|row| super::primary_layout::decode(&row?)).collect::<Result<_, StateError>>()?;
    if op.members.is_empty()
        || op.members.len() > 16
        || op.members.iter().any(|member| member.installed.is_some())
    {
        return invalid();
    }
    Ok(op)
}

#[must_use]
fn matches_managed_successor(
    old_root: &ActiveGeneration,
    old: &ActiveDecision,
    current_root: &ActiveGeneration,
    current: &ActiveDecision,
    op: &super::primary_layout::LayoutOperation,
) -> bool {
    if old_root.id == current_root.id
        || old_root.spec.scope != current_root.spec.scope
        || old_root.spec.zone != current_root.spec.zone
        || old_root.spec.config_hash != current_root.spec.config_hash
        || old.decision.outcome != SelectionOutcome::Selected
        || current.decision.outcome != SelectionOutcome::Selected
        || old.decision.pass_key != current.decision.pass_key
        || old.decision.child != current.decision.child
        || old.decision.master.is_none()
        || old.decision.master != current.decision.master
        || old.state_id != current.state_id
        || old.decision.media_source != current.decision.media_source
        || old.media_zone(old_root) != current.media_zone(current_root)
        || Some(op.library.as_str())
            != old
                .media_zone(old_root)
                .get("zoneName")
                .and_then(Value::as_str)
        || op.child != old.state_id
        || op.asset_record_name != old.decision.child
        || op.pass != old.decision.pass_key
        || Some(op.source_library.as_str())
            != old_root.spec.zone.get("zoneName").and_then(Value::as_str)
    {
        return false;
    }
    let mut changed = false;
    for destination in &old.decision.destinations {
        let Some(next) = current
            .decision
            .destinations
            .iter()
            .find(|next| next.version_size == destination.version_size)
        else {
            return false;
        };
        let Some(member) = op
            .members
            .iter()
            .find(|member| member.destination.as_ref() == Some(&destination.path))
        else {
            return false;
        };
        let Some(record) = &member.record else {
            return false;
        };
        let Some(prepared) = &member.prepared else {
            return false;
        };
        if record.library.as_ref() != op.library
            || record.id.as_ref() != old.state_id
            || record.version_size.as_str() != destination.version_size
            || record.checksum.as_ref() != destination.checksum
            || record.size_bytes != destination.size
            || record.metadata.metadata_hash.as_deref() != Some(destination.metadata_hash.as_str())
            || prepared.version != destination.version_size
            || prepared.provider_checksum != destination.checksum
            || member.stage.as_ref() != Some(&prepared.path)
            || prepared.source_checksum.as_ref().is_none_or(|checksum| {
                checksum.len() != 64 || !checksum.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
        {
            return false;
        }
        changed |= next.checksum != destination.checksum || next.size != destination.size;
    }
    changed
}

/// This is the sole inactive-successor predicate. Scheduling, guards, grouping
/// and metadata already require admitted destinations; only this proof permits
/// an old selected manifest to retain receipts with admission withdrawn.
fn validate_managed_successor(
    conn: &Connection,
    root: &ActiveGeneration,
    manifest: &ActiveDecision,
    reason: &str,
) -> Result<bool, StateError> {
    let Some(proof) = reason.strip_prefix(MANAGED_SUCCESSOR_PREFIX) else {
        return Ok(false);
    };
    let proof: ManagedSuccessor =
        serde_json::from_str(proof).map_err(|_invalid| StateError::ProviderSelectionInvalid)?;
    let (attempts, deadline): (i64, i64) = conn.query_row(
        "SELECT attempts,next_retry_at FROM provider_active_decisions WHERE generation=?1 AND pass_key=?2 AND child=?3",
        params![root.id,manifest.decision.pass_key,manifest.decision.child], |row| Ok((row.get(0)?,row.get(1)?)),
    )?;
    if !((proof.prior_reason.is_empty() && attempts == 0 && deadline == 0)
        || (proof.prior_reason == "current_confirmation_unavailable"
            && (1..=32).contains(&attempts)
            && deadline > 0))
    {
        return invalid();
    }
    let successor_root = load_generation(conn, &proof.generation)?;
    let (bytes, hash, admission): (Vec<u8>, String, String) = conn.query_row(
        "SELECT manifest,manifest_hash,admission FROM provider_active_decisions WHERE generation=?1 AND pass_key=?2 AND child=?3 AND length(manifest)<=?4",
        params![proof.generation,manifest.decision.pass_key,manifest.decision.child,i64::try_from(MAX_CHUNK_BYTES).map_err(|_invalid| StateError::ProviderSelectionFull)?], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
    )?;
    if hash != proof.manifest_hash
        || digest(&bytes) != hash
        || !matches!(admission.as_str(), "admitted" | "excluded")
    {
        return invalid();
    }
    let current: ActiveDecision =
        serde_json::from_slice(&bytes).map_err(|_invalid| StateError::ProviderSelectionInvalid)?;
    let stored_admission: Option<String> = conn.query_row("SELECT admission_hash FROM provider_active_decisions WHERE generation=?1 AND pass_key=?2 AND child=?3", params![proof.generation,current.decision.pass_key,current.decision.child], |row| row.get(0))?;
    if stored_admission.as_deref()
        != Some(
            admission_hash(
                conn,
                &successor_root,
                &current.decision.pass_key,
                &current.decision.child,
            )?
            .as_str(),
        )
    {
        return invalid();
    }
    validate_decision(conn, &successor_root, &current, true)?;
    let op = cancelled_layout(conn, &proof.operation)?;
    if digest(&encode(&op)?) != proof.operation_hash
        || !matches_managed_successor(root, manifest, &successor_root, &current, &op)
    {
        return invalid();
    }
    Ok(true)
}

fn managed_predecessors(
    conn: &Connection,
    root: &ActiveGeneration,
    manifest: &ActiveDecision,
    layouts: &[super::primary_layout::LayoutOperation],
) -> Result<
    Vec<(
        ActiveGeneration,
        ActiveDecision,
        super::primary_layout::LayoutOperation,
    )>,
    StateError,
> {
    if layouts.len() > 16 {
        return Err(StateError::ProviderSelectionFull);
    }
    for supplied in layouts {
        if encode(&cancelled_layout(conn, &supplied.operation)?)? != encode(supplied)? {
            return Err(StateError::ProviderWorkConflict("cancelled_layout_changed"));
        }
    }
    let keys: Vec<String> = conn.prepare("SELECT DISTINCT d.generation FROM provider_active_decisions d JOIN provider_active_generations g ON g.id=d.generation WHERE g.scope=?1 AND g.config_hash=?2 AND d.pass_key=?3 AND d.child=?4 AND d.admission='admitted' AND d.generation<>?5 AND EXISTS(SELECT 1 FROM provider_active_destinations w WHERE w.generation=d.generation AND w.pass_key=d.pass_key AND w.child=d.child AND w.admitted=1 AND ((w.verified_media=0 AND w.preserved_local IS NULL) OR (g.metadata_enabled=1 AND w.verified_metadata=0))) ORDER BY g.created_at LIMIT 17")?
        .query_map(params![root.spec.scope,root.spec.config_hash,manifest.decision.pass_key,manifest.decision.child,root.id], |row| row.get(0))?.collect::<Result<_, _>>()?;
    if keys.len() > 16 {
        return Err(StateError::ProviderSelectionFull);
    }
    let mut result = Vec::new();
    for generation in keys {
        let old_root = load_generation(conn, &generation)?;
        let old = load_decision(
            conn,
            &old_root,
            &manifest.decision.pass_key,
            &manifest.decision.child,
        )?;
        let prepared: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM provider_active_destinations WHERE generation=?1 AND pass_key=?2 AND child=?3 AND prepared_hash IS NOT NULL)", params![generation,old.decision.pass_key,old.decision.child], |row| row.get(0))?;
        if prepared {
            continue;
        }
        let mut matching = layouts
            .iter()
            .filter(|op| matches_managed_successor(&old_root, &old, root, manifest, op));
        if let Some(op) = matching.next()
            && matching.next().is_none()
        {
            result.push((old_root, old, op.clone()));
        }
    }
    Ok(result)
}

fn insert_selection_rows(
    conn: &Connection,
    root: &ActiveGeneration,
    manifest: &ActiveDecision,
    disposition: (&str, &str),
    capacity: u64,
) -> Result<(), StateError> {
    let generation = &root.id;
    let d = &manifest.decision;
    let (admission, reason) = disposition;
    let admitted = admission == "admitted";
    let bytes = encode(manifest)?;
    let hash = digest(&bytes);
    let charged = u64::try_from(
        bytes.len()
            + manifest
                .sources
                .iter()
                .map(|s| s.page_id.len() + generation.len() + d.pass_key.len() + d.child.len() + 16)
                .sum::<usize>()
            + d.destinations
                .iter()
                .map(|destination| {
                    destination.path.key().map(|path| {
                        path.len()
                            + destination.path.to_path().to_string_lossy().len()
                            + destination.checksum.len()
                            + destination.metadata_hash.len()
                            + generation.len()
                            + d.pass_key.len()
                            + d.child.len()
                            + manifest.state_id.len()
                            + destination.version_size.len()
                            + d.master.as_ref().map_or(0, String::len)
                            + manifest
                                .media_zone(root)
                                .get("zoneName")
                                .and_then(Value::as_str)
                                .map_or(0, str::len)
                            + 1024
                    })
                })
                .collect::<Result<Vec<_>, _>>()?
                .iter()
                .sum::<usize>()
            + encode(&(&d.pass_key, &d.child))?.len()
            + 256,
    )
    .map_err(|_invalid| StateError::ProviderSelectionFull)?;
    check_write_charge(charged, capacity)?;
    conn.execute("INSERT INTO provider_active_decisions(generation,pass_key,child,manifest,manifest_hash,outcome,admission,reason,charged_bytes) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9) ON CONFLICT(generation,pass_key,child) DO UPDATE SET manifest=excluded.manifest,manifest_hash=excluded.manifest_hash,outcome=excluded.outcome,admission=excluded.admission,reason=excluded.reason,attempts=CASE WHEN excluded.admission='deferred' THEN provider_active_decisions.attempts ELSE 0 END,next_retry_at=CASE WHEN excluded.admission='deferred' THEN provider_active_decisions.next_retry_at ELSE 0 END,charged_bytes=provider_active_decisions.charged_bytes+excluded.charged_bytes",params![generation,d.pass_key,d.child,bytes,hash,outcome_name(d.outcome),admission,reason,i64::try_from(charged).map_err(|_invalid|StateError::ProviderSelectionFull)?])?;
    for source in &manifest.sources {
        conn.execute("INSERT OR IGNORE INTO provider_active_sources(generation,pass_key,child,page_id,ordinal) VALUES(?1,?2,?3,?4,?5)",params![generation,d.pass_key,d.child,source.page_id,source.ordinal])?;
    }
    let photo = match (&d.confirmation, &d.master) {
        (Some(body), Some(master)) => Some(
            crate::icloud::photos::current_asset(body, &d.child, master, manifest.media_zone(root))
                .map_err(|_invalid| StateError::ProviderSelectionInvalid)?,
        ),
        _ => None,
    };
    for destination in &d.destinations {
        let photo = photo.as_ref().ok_or(StateError::ProviderSelectionInvalid)?;
        conn.execute("INSERT INTO provider_active_destinations(generation,pass_key,child,library,asset_id,master,version_size,path,checksum,size_bytes,created_at,added_at,metadata_hash,admitted,compat_path) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",params![generation,d.pass_key,d.child,manifest.media_zone(root).get("zoneName").and_then(Value::as_str),manifest.state_id,d.master,destination.version_size,destination.path.key()?,destination.checksum,i64::try_from(destination.size).map_err(|_invalid|StateError::ProviderSelectionInvalid)?,encode_asset_date(photo.created()),Some(encode_asset_date(photo.added_date())),destination.metadata_hash,admitted,destination.path.to_path().to_string_lossy()])?;
    }
    Ok(())
}

impl SqliteStateDb {
    pub(crate) async fn cancelled_selection_layouts(
        &self,
        owner: account::AccountOwner,
        generation: String,
        manifest: ActiveDecision,
    ) -> Result<Vec<super::primary_layout::LayoutOperation>, StateError> {
        self.with_conn("reading bounded cancelled selection journals", move |conn| {
            account::validate_authenticated(conn, &owner)?;
            let root = load_generation(conn, &generation)?;
            validate_decision(conn, &root, &manifest, true)?;
            let library = manifest.media_zone(&root).get("zoneName").and_then(Value::as_str).ok_or(StateError::ProviderSelectionInvalid)?;
            let child = &manifest.state_id;
            let ids: Vec<String> = conn.prepare("SELECT operation FROM primary_layout_operations WHERE library=?1 AND json_extract(CAST(header AS TEXT),'$.child')=?2 AND phase='cancelled' ORDER BY rowid DESC LIMIT 16")?.query_map(params![library,child], |row| row.get(0))?.collect::<Result<_, _>>()?;
            let layouts = ids.iter().map(|id| cancelled_layout(conn, id)).collect::<Result<Vec<_>, _>>()?;
            let mut result = Vec::new();
            for (_, _, op) in managed_predecessors(conn, &root, &manifest, &layouts)? {
                if !result.iter().any(|prior: &super::primary_layout::LayoutOperation| prior.operation == op.operation) { result.push(op); }
            }
            Ok(result)
        }).await
    }

    pub(crate) async fn has_selected_layout_obligation(
        &self,
        owner: account::AccountOwner,
        library: String,
        child: String,
    ) -> Result<bool, StateError> {
        self.with_conn("checking native managed recovery ownership", move |conn| {
            account::validate_authenticated(conn, &owner)?;
            validate_identity_decisions(conn, &library, &child)?;
            Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM provider_active_destinations w JOIN provider_active_generations g ON g.id=w.generation WHERE w.library=?1 AND w.asset_id=?2 AND w.admitted=1 AND ((w.verified_media=0 AND w.preserved_local IS NULL) OR (g.metadata_enabled=1 AND w.verified_metadata=0)))", params![library,child], |row| row.get(0))?)
        }).await
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct GenerationSpec {
    pub(crate) format: u8,
    pub(crate) scope: String,
    pub(crate) zone: Value,
    pub(crate) config_hash: String,
    pub(crate) basis: String,
    pub(crate) profile: Value,
    pub(crate) metadata_enabled: bool,
    pub(crate) metadata_flags: u16,
}

impl GenerationSpec {
    pub(crate) fn is_accepted_replay(&self) -> bool {
        matches!(
            self.profile.get("coverage").and_then(Value::as_str),
            Some(
                ACCEPTED_REPLAY_COVERAGE
                    | ACCEPTED_ALBUM_REPLAY_COVERAGE
                    | ACCEPTED_FAVORITES_REPLAY_COVERAGE
                    | MIXED_FAVORITES_REPLAY
                    | ACCEPTED_FOREIGN_REPLAY_COVERAGE
            )
        )
    }

    pub(crate) fn mixed_favorites_replay(&self) -> bool {
        self.profile.get("coverage").and_then(Value::as_str) == Some(MIXED_FAVORITES_REPLAY)
    }

    pub(crate) fn is_accepted_favorites(&self, key: &str) -> bool {
        self.is_accepted_replay()
            && pass_from_profile(&self.profile, key).is_some_and(is_replay_favorites_pass)
    }

    pub(crate) fn accepted_foreign_replay(&self, key: &str) -> Option<SelectionMediaSource> {
        if self.profile.get("coverage").and_then(Value::as_str)
            != Some(ACCEPTED_FOREIGN_REPLAY_COVERAGE)
        {
            return None;
        }
        foreign_album_source(pass_from_profile(&self.profile, key)?)
    }

    pub(crate) fn accepted_album_container(&self, key: &str) -> Option<&str> {
        if self.profile.get("coverage").and_then(Value::as_str)
            != Some(ACCEPTED_ALBUM_REPLAY_COVERAGE)
        {
            return None;
        }
        accepted_album_container(pass_from_profile(&self.profile, key)?)
    }

    pub(crate) fn accepted_album_requirement(
        &self,
        key: &str,
    ) -> Option<(Vec<&str>, crate::icloud::photos::ReplayAlbumLink)> {
        use crate::icloud::photos::ReplayAlbumLink;
        if !self.is_accepted_replay() {
            return None;
        }
        if self.accepted_foreign_replay(key).is_some() {
            let container = pass_from_profile(&self.profile, key)?
                .pointer("/scope/container_id")?
                .as_str()?;
            return Some((vec![container], ReplayAlbumLink::Foreign));
        }
        if let Some(container) = self.accepted_album_container(key) {
            return Some((vec![container], ReplayAlbumLink::Live));
        }
        if !is_replay_unfiled_pass(pass_from_profile(&self.profile, key)?) {
            return None;
        }
        mixed_unfiled_albums(self.profile.get("passes")?.as_array()?)
            .map(|containers| (containers, ReplayAlbumLink::Removed))
    }
}

pub(crate) fn accepted_replay_coverage(profile: &Value) -> Option<&'static str> {
    if !has_no_replay_excludes(profile) {
        return None;
    }
    let passes = profile.get("passes")?.as_array()?;
    passes
        .iter()
        .find_map(|pass| replay_pass_coverage(pass, passes))
}

pub(crate) fn accepted_replay_pass(profile: &Value, key: &str) -> bool {
    profile
        .get("passes")
        .and_then(Value::as_array)
        .zip(pass_from_profile(profile, key))
        .is_some_and(|(passes, pass)| replay_pass_coverage(pass, passes).is_some())
}

fn replay_pass_coverage(pass: &Value, passes: &[Value]) -> Option<&'static str> {
    if let [a, b] = passes
        && ((is_replay_favorites_pass(a) && is_replay_unfiled_pass(b))
            || (is_replay_favorites_pass(b) && is_replay_unfiled_pass(a)))
    {
        return Some(MIXED_FAVORITES_REPLAY);
    }
    if accepted_album_container(pass).is_some() {
        return Some(ACCEPTED_ALBUM_REPLAY_COVERAGE);
    }
    if passes.len() == 1 && foreign_album_source(pass).is_some() {
        return Some(ACCEPTED_FOREIGN_REPLAY_COVERAGE);
    }
    if passes.len() == 1 && is_replay_favorites_pass(pass) {
        return Some(ACCEPTED_FAVORITES_REPLAY_COVERAGE);
    }
    if !is_replay_unfiled_pass(pass) {
        return None;
    }
    if passes.len() == 1 {
        return Some(ACCEPTED_REPLAY_COVERAGE);
    }
    mixed_unfiled_albums(passes).map(|_| ACCEPTED_ALBUM_REPLAY_COVERAGE)
}

fn is_replay_favorites_pass(pass: &Value) -> bool {
    pass.get("kind").and_then(Value::as_str) == Some("SmartFolder")
        && has_no_replay_excludes(pass)
        && pass
            .get("scope")
            .is_some_and(crate::icloud::photos::smart_folders::is_favorites_selection)
}

fn is_replay_unfiled_pass(pass: &Value) -> bool {
    pass.get("kind").and_then(Value::as_str) == Some("Unfiled")
        && has_no_replay_excludes(pass)
        && pass
            .get("scope")
            .is_some_and(crate::icloud::photos::is_library_wide_selection)
}

// Every selected named album must be covered. Smart folders do not contribute
// exclusions. Keep the complete check within one targeted lookup.
fn mixed_unfiled_albums(passes: &[Value]) -> Option<Vec<&str>> {
    let mut albums = passes
        .iter()
        .filter(|pass| pass.get("kind").and_then(Value::as_str) == Some("Album"))
        .map(accepted_album_container)
        .collect::<Option<Vec<_>>>()?;
    albums.sort_unstable();
    albums.dedup();
    (1..=crate::icloud::photos::REPLAY_ALBUM_LINK_LIMIT)
        .contains(&albums.len())
        .then_some(albums)
}

fn has_no_replay_excludes(profile: &Value) -> bool {
    profile
        .get("excludes")
        .and_then(Value::as_array)
        .is_some_and(Vec::is_empty)
}

fn accepted_album_container(pass: &Value) -> Option<&str> {
    let scope = pass.get("scope")?;
    let container = scope.get("container_id")?.as_str()?;
    (pass.get("kind").and_then(Value::as_str) == Some("Album")
        && has_no_replay_excludes(pass)
        && scope.get("cross_zone_sources")?.as_array()?.is_empty()
        && crate::icloud::photos::selected_album_name(scope).is_some())
    .then_some(container)
}

fn foreign_album_source(pass: &Value) -> Option<SelectionMediaSource> {
    let scope = pass.get("scope")?;
    let [source] = pass.get("media_sources")?.as_array()?.as_slice() else {
        return None;
    };
    let [declared] = scope.get("cross_zone_sources")?.as_array()?.as_slice() else {
        return None;
    };
    if pass["kind"] != "Album"
        || !has_no_replay_excludes(pass)
        || source.get("zone") != declared.get("zone")
        || source.pointer("/zone/zoneName") == scope.pointer("/zone/zoneName")
    {
        return None;
    }
    crate::icloud::photos::selected_album_name(scope)?;
    serde_json::from_value(source.clone()).ok()
}

#[derive(Clone, Debug)]
pub(crate) struct ActiveGeneration {
    pub(crate) id: String,
    pub(crate) spec: GenerationSpec,
    pub(crate) sealed: bool,
    pub(crate) checkpoint_ready: bool,
    pub(crate) checkpoint_veto: Option<String>,
}

/// Destinations admitted by a specific, freshly scanned selection. Re-read
/// their rank and publication receipts in the source checkpoint transaction.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct SparseSelectedCopy {
    pub(crate) created: chrono::DateTime<Utc>,
    pub(crate) added: Option<chrono::DateTime<Utc>>,
    pub(crate) version: String,
    pub(crate) path: std::path::PathBuf,
    pub(crate) provider_checksum: String,
    pub(crate) metadata_hash: String,
    pub(crate) local_checksum: String,
    source_checksum: String,
}

pub(super) fn sparse_selected_copies(
    conn: &Connection,
    generation: &str,
    library: &str,
    child: &str,
    master: &str,
) -> Result<Result<Vec<SparseSelectedCopy>, super::SparseTargetHold>, StateError> {
    use super::SparseTargetHold;
    let root = load_generation(conn, generation)?;
    if !root.sealed
        || !root.checkpoint_ready
        || root.spec.zone.get("zoneName").and_then(Value::as_str) != Some(library)
        || basis(conn, &root.spec.scope)? != root.spec.basis
        || conn.query_row("SELECT id FROM provider_active_generations WHERE scope=?1 ORDER BY created_at DESC,id DESC LIMIT 1",
            [&root.spec.scope], |row| row.get::<_, String>(0))? != generation
    {
        return Ok(Err(SparseTargetHold::ReceiptUnqualified));
    }
    let mut keys = conn.prepare("SELECT pass_key FROM provider_active_decisions WHERE generation=?1 AND child=?2 ORDER BY pass_key")?;
    let keys = keys
        .query_map(params![generation, child], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    // Only a missing decision permits a direct Hidden membership lookup.
    if keys.is_empty() {
        return Ok(Ok(Vec::new()));
    }
    let mut copies = Vec::new();
    if !super::legacy_preservation::child_publications_ready(
        conn,
        library,
        child,
        root.spec.metadata_enabled,
    )? {
        return Ok(Err(SparseTargetHold::ReceiptUnqualified));
    }
    let mut pages = RankProofs::new(conn, &root);
    let grouping = root
        .spec
        .metadata_enabled
        .then(|| grouping_hash(conn, library, child))
        .transpose()?;
    for key in keys {
        let manifest = load_decision_with_pages(conn, &root, &key, child, true, &mut pages)?;
        if manifest.media_zone(&root) != &root.spec.zone {
            continue;
        }
        let decision = manifest.decision;
        if decision.master.as_deref().is_some_and(|id| id != master) {
            return Ok(Err(SparseTargetHold::PairUnresolved));
        }
        if decision.outcome == SelectionOutcome::Excluded {
            continue;
        }
        if decision.outcome != SelectionOutcome::Selected
            || decision.master.as_deref() != Some(master)
            || manifest.state_id != child
            || decision.destinations.is_empty()
        {
            return Ok(Err(SparseTargetHold::PairUnresolved));
        }
        for dest in decision.destinations {
            let receipt = conn.query_row("SELECT w.local_checksum,w.source_checksum,w.created_at,w.added_at FROM provider_active_destinations w
                JOIN assets a ON a.library=w.library AND a.id=w.asset_id AND a.version_size=w.version_size
                  AND a.checksum=w.checksum AND a.metadata_hash=w.metadata_hash AND a.size_bytes=w.size_bytes
                  AND a.created_at IS w.created_at AND a.added_at IS w.added_at
                WHERE w.generation=?1 AND w.pass_key=?2 AND w.child=?3 AND w.path=?4 AND w.version_size=?5
                  AND w.admitted=1 AND w.verified_media=1 AND w.preserved_local IS NULL
                  AND w.prepared_hash IS NULL AND (?6 IS NULL OR (w.verified_metadata=1 AND w.grouping_hash=?6))
                  AND EXISTS (SELECT 1 FROM provider_active_destinations p
                    WHERE p.current_publication=1 AND p.library=w.library AND p.asset_id=w.asset_id
                      AND p.master=w.master AND p.path=w.path AND p.version_size=w.version_size
                      AND p.checksum=w.checksum AND p.local_checksum=w.local_checksum
                      AND p.source_checksum=w.source_checksum AND p.verified_media=1
                      AND p.preserved_local IS NULL AND p.prepared_hash IS NULL)",
                params![generation,key,child,dest.path.key()?,dest.version_size,grouping], |row| Ok((row.get(0)?,row.get(1)?, decode_asset_date(row.get(2)?,2)?, row.get::<_,Option<f64>>(3)?.map(|v| decode_asset_date(v,3)).transpose()?))).optional()?;
            let Some((local_checksum, source_checksum, created, added)) = receipt else {
                return Ok(Err(SparseTargetHold::ReceiptUnqualified));
            };
            copies.push(SparseSelectedCopy {
                created,
                added,
                version: dest.version_size,
                path: dest.path.to_path(),
                provider_checksum: dest.checksum,
                metadata_hash: dest.metadata_hash,
                local_checksum,
                source_checksum,
            });
        }
    }
    if copies.is_empty() {
        return Ok(Err(SparseTargetHold::ReceiptUnqualified));
    }
    Ok(Ok(copies))
}

/// Shared selector handoff: exact published selector-to-container evidence.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct BoundAlbumSelection {
    pub(crate) generation: String,
    pub(crate) container_id: String,
    pub(crate) components: Vec<String>,
    pub(crate) deleted: bool,
}

impl SqliteStateDb {
    pub(crate) async fn bound_album_selections(
        &self,
        owner: account::AccountOwner,
        zone: Value,
        database: String,
    ) -> Result<Vec<BoundAlbumSelection>, StateError> {
        const PRIOR_ALBUM_GENERATION_LIMIT: i64 = 128;
        self.with_conn("reading prior exact album selections", move |conn| {
            account::validate_authenticated(conn, &owner)?;
            let library = zone.get("zoneName").and_then(Value::as_str).ok_or(StateError::ProviderSelectionInvalid)?;
            // Historical selection evidence is not current file retirement authority.
            let ids: Vec<String> = conn.prepare("SELECT DISTINCT g.id,g.created_at FROM provider_active_generations g JOIN provider_active_destinations d ON d.generation=g.id WHERE json_extract(CAST(g.specification AS TEXT),'$.zone.zoneName')=?1 AND d.verified_media=1 ORDER BY g.created_at DESC,g.id DESC LIMIT ?2")?
                .query_map(params![library,PRIOR_ALBUM_GENERATION_LIMIT], |row| row.get(0))?.collect::<Result<_, _>>()?;
            let mut result = Vec::new();
            let mut seen = HashSet::new();
            for id in ids {
                let root = load_generation(conn, &id)?;
                if root.spec.zone != zone { continue; }
                let scope: Value = serde_json::from_str(&root.spec.scope).map_err(|_invalid| StateError::ProviderSelectionInvalid)?;
                if scope.get("database").and_then(Value::as_str) != Some(database.as_str()) { continue; }
                let passes = root.spec.profile.get("passes").and_then(Value::as_array).ok_or(StateError::ProviderSelectionInvalid)?;
                for pass in passes {
                    if pass.get("kind").and_then(Value::as_str) != Some("Album") { continue; }
                    let Some(container) = pass.pointer("/scope/container_id").and_then(Value::as_str) else { continue; };
                    if !seen.insert(container.to_owned()) { continue; }
                    let Some(raw) = pass.get("selector_components") else { continue; };
                    let components: Vec<String> = serde_json::from_value(raw.clone()).map_err(|_invalid| StateError::ProviderSelectionInvalid)?;
                    if components.is_empty() { return invalid(); }
                    let key = pass.get("key").and_then(Value::as_str).ok_or(StateError::ProviderSelectionInvalid)?;
                    let published: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM provider_active_destinations WHERE generation=?1 AND pass_key=?2 AND verified_media=1)",params![id,key], |row| row.get(0))?;
                    if !published { continue; }
                    let deleted: Option<bool> = conn.query_row("SELECT is_deleted FROM album_containers WHERE library=?1 AND container_id=?2 AND pass_kind='album'", params![library,container], |row| row.get(0)).optional()?;
                    let Some(deleted) = deleted else { continue; };
                    result.push(BoundAlbumSelection { generation:id.clone(),container_id:container.to_owned(),components,deleted });
                }
            }
            Ok(result)
        }).await
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RankSource {
    pub(crate) page_id: String,
    pub(crate) ordinal: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ActiveDecision {
    pub(crate) decision: SelectionDecision,
    pub(crate) sources: Vec<RankSource>,
    /// Existing sparse/legacy identity selection, independently checked at admission.
    pub(crate) state_id: String,
}

impl ActiveDecision {
    pub(crate) fn media_zone<'a>(&'a self, root: &'a ActiveGeneration) -> &'a Value {
        self.decision
            .media_source
            .as_ref()
            .map_or(&root.spec.zone, |source| &source.zone)
    }
}

pub(crate) enum SelectionAttempt {
    Due(Option<Box<ActiveDecision>>),
    Deferred,
}

pub(crate) enum ReplayPageCapture {
    Captured(Vec<String>),
    Released(String),
}

fn validate_media_source(
    root: &ActiveGeneration,
    manifest: &ActiveDecision,
) -> Result<(), StateError> {
    if let Some(binding) = root
        .spec
        .accepted_foreign_replay(&manifest.decision.pass_key)
        && manifest.decision.media_source.as_ref() != Some(&binding)
    {
        return invalid();
    }
    let Some(source) = &manifest.decision.media_source else {
        return Ok(());
    };
    if source.scope == root.spec.scope && source.zone == root.spec.zone {
        return Ok(());
    }
    let allowed = pass(root, &manifest.decision.pass_key)?
        .get("media_sources")
        .and_then(Value::as_array)
        .ok_or(StateError::ProviderSelectionInvalid)?;
    if !allowed.contains(
        &serde_json::to_value(source).map_err(|_invalid| StateError::ProviderSelectionInvalid)?,
    ) {
        return invalid();
    }
    let scope: Value = serde_json::from_str(&source.scope)
        .map_err(|_invalid| StateError::ProviderSelectionInvalid)?;
    let owner: Value = serde_json::from_str(&root.spec.scope)
        .map_err(|_invalid| StateError::ProviderSelectionInvalid)?;
    if ["format", "realm", "container", "environment"]
        .iter()
        .any(|key| scope.get(key) != owner.get(key))
        || !matches!(
            scope.get("database").and_then(Value::as_str),
            Some("private" | "shared")
        )
        || !crate::icloud::photos::has_explicit_owner(source.zone.get("ownerRecordName"))
        || source
            .zone
            .get("zoneName")
            .and_then(Value::as_str)
            .is_none_or(|name| name.trim().is_empty())
        || source.zone.get("zoneName") == root.spec.zone.get("zoneName")
        || scope.pointer("/zone/zoneName") != source.zone.get("zoneName")
        || scope.pointer("/zone/ownerRecordName") != source.zone.get("ownerRecordName")
    {
        return invalid();
    }
    Ok(())
}

#[derive(Debug)]
pub(crate) struct GenerationProjection {
    pub(crate) admitted: bool,
    pub(crate) reason: String,
}

pub(crate) struct AdmissionChecks {
    /// Live rank observation, supplied only by full inventory after a paired lookup.
    pub(crate) inventory_observation: Option<Arc<crate::icloud::photos::PhotoAsset>>,
    pub(crate) capacity: u64,
    pub(crate) layouts: Vec<super::primary_layout::LayoutOperation>,
    /// Occupied unsuffixed paths for newly proposed unfiled Shared collisions.
    pub(crate) archive_paths: Vec<(crate::state::VersionSizeKey, std::path::PathBuf)>,
}

fn ambiguous_primary_archive_path(
    conn: &Connection,
    records: &[AssetRecord],
    checks: &[(crate::state::VersionSizeKey, std::path::PathBuf)],
) -> Result<bool, StateError> {
    for (version, path) in checks {
        let record = records
            .iter()
            .find(|r| r.version_size == *version)
            .ok_or(StateError::ProviderSelectionInvalid)?;
        // A scoped current destination identifies an independently owned Primary
        // copy. A pre-baseline catalog row alone does not. Never infer a Shared
        // owner from the provider checksum or from the occupied path.
        let ambiguous = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM assets a WHERE a.local_path=?1
                AND a.library='PrimarySync' AND a.version_size=?2 AND a.status='downloaded'
                AND a.checksum=?3 AND a.size_bytes=?4
                AND NOT EXISTS(SELECT 1 FROM provider_active_destinations w
                    WHERE w.library=a.library AND w.asset_id=a.id AND w.version_size=a.version_size
                    AND w.compat_path=a.local_path AND w.checksum=a.checksum AND w.size_bytes=a.size_bytes
                    AND w.admitted=1 AND w.verified_media=1))",
            params![
                path.to_string_lossy(), version.as_str(), record.checksum,
                i64::try_from(record.size_bytes).map_err(|_overflow| StateError::ProviderSelectionInvalid)?
            ],
            |row| row.get(0),
        )?;
        if ambiguous {
            return Ok(true);
        }
    }
    Ok(false)
}

fn invalid<T>() -> Result<T, StateError> {
    Err(StateError::ProviderSelectionInvalid)
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, StateError> {
    let bytes =
        serde_json::to_vec(value).map_err(|_invalid| StateError::ProviderSelectionInvalid)?;
    if bytes.len() > MAX_CHUNK_BYTES {
        return Err(StateError::ProviderSelectionFull);
    }
    Ok(bytes)
}

fn owner_keys(conn: &Connection) -> Result<(String, String), StateError> {
    Ok(conn.query_row(
        "SELECT account_key,provider_key FROM account_owner WHERE singleton=1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?)
}

/// A change detector over retained complete nonempty observations. Ordering is
/// hashing/scheduling only; it never selects a resource version. Empty polling
/// pages and exact held-delta replays cannot repeatedly dirty healthy work.
fn basis(conn: &Connection, scope: &str) -> Result<String, StateError> {
    dependency_basis(conn, scope, EmptySelectionDeletes::Include)
}

enum EmptySelectionDeletes {
    Include,
    IgnoreAppliedAssetDeletes,
}

/// Verify the downstream owner independently retains this exact live source.
/// A filename, a matching local checksum, or a historical downloaded row is
/// not proof that the selected destinations consumed this provider revision.
pub(super) fn published_change_matches(
    conn: &Connection,
    scope: &str,
    name: &str,
    record: &Value,
) -> Result<bool, StateError> {
    let candidate: Option<(String, String, String)> = conn
        .query_row(
            "SELECT w.generation,w.pass_key,w.child FROM provider_active_destinations w
         JOIN provider_active_generations g ON g.id=w.generation
         WHERE g.scope=?1 AND (w.child=?2 OR w.master=?2) AND w.current_publication=1
         AND w.admitted=1 AND w.verified_media=1 AND w.prepared_hash IS NULL
         AND (g.metadata_enabled=0 OR w.verified_metadata=1)
         ORDER BY g.created_at DESC LIMIT 1",
            params![scope, name],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((generation, key, child)) = candidate else {
        return Ok(false);
    };
    let root = load_generation(conn, &generation)?;
    let manifest = load_decision(conn, &root, &key, &child)?;
    if manifest
        .decision
        .media_source
        .as_ref()
        .is_some_and(|source| source.scope != scope)
    {
        return Ok(false);
    }
    Ok(manifest
        .decision
        .confirmation
        .as_deref()
        .is_some_and(|body| crate::icloud::photos::confirmation_contains_record(body, record)))
}

fn dependency_basis(
    conn: &Connection,
    scope: &str,
    deletes: EmptySelectionDeletes,
) -> Result<String, StateError> {
    let mut hash = Sha256::new();
    // Only a known asset's applied deletion is irrelevant to an empty
    // selection. Unknown tombstones may be membership changes and must retain
    // their dependency. Mixed pages retain the complete original body hash.
    let mut statement=conn.prepare("SELECT p.request_cursor,p.body_hash FROM provider_shadow_pages p JOIN provider_catalog_pages c ON c.page_id=p.id AND c.body_hash=p.body_hash AND c.projector_version=1 WHERE p.scope=?1 AND EXISTS(SELECT 1 FROM provider_catalog_records r WHERE r.page_id=p.id AND (?2=0 OR NOT (r.deleted=1 AND (r.record_type IS NULL OR r.record_type IN ('CPLAsset','CPLMaster')) AND EXISTS(SELECT 1 FROM assets a WHERE a.library=json_extract(?1,'$.zone.zoneName') AND a.id=r.record_name AND a.is_deleted=1)))) ORDER BY p.request_cursor,p.body_hash")?;
    let mut rows = statement.query(params![
        scope,
        matches!(deletes, EmptySelectionDeletes::IgnoreAppliedAssetDeletes)
    ])?;
    while let Some(row) = rows.next()? {
        for value in [row.get::<_, String>(0)?, row.get::<_, String>(1)?] {
            hash.update((value.len() as u64).to_le_bytes());
            hash.update(value.as_bytes());
        }
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn validate_spec(spec: &GenerationSpec) -> Result<(), StateError> {
    // Durable MetadataFlags encoding: the upper bits refine XMP writers only.
    const ENRICHMENT_FLAGS: u16 = 0b1100_0000;
    const XMP_WRITER_FLAGS: u16 = 0b0011_0000;
    const KNOWN_FLAGS: u16 = 0x01ff;
    let scope: Value = serde_json::from_str(&spec.scope)
        .map_err(|_invalid| StateError::ProviderSelectionInvalid)?;
    if spec.format != 1
        || spec.metadata_enabled != (spec.metadata_flags != 0)
        || spec.metadata_flags & !KNOWN_FLAGS != 0
        // Enrichment bits require an embedded-XMP or sidecar writer.
        || (spec.metadata_flags & ENRICHMENT_FLAGS != 0
            && spec.metadata_flags & XMP_WRITER_FLAGS == 0)
        || !matches!(
            scope.get("database").and_then(Value::as_str),
            Some("private" | "shared")
        )
        || !crate::icloud::photos::has_explicit_owner(spec.zone.get("ownerRecordName"))
        || spec.zone.get("zoneName") != scope.pointer("/zone/zoneName")
        || spec.zone.get("ownerRecordName") != scope.pointer("/zone/ownerRecordName")
        || [&spec.config_hash, &spec.basis]
            .iter()
            .any(|v| v.len() != 64 || !v.bytes().all(|b| b.is_ascii_hexdigit()))
        || !(spec.is_accepted_replay()
            || spec.profile.get("coverage").and_then(Value::as_str)
                == Some("observed_selection_window"))
        || spec
            .profile
            .get(INDEPENDENT_ALBUM_COPIES)
            .is_some_and(|value| value != &Value::Bool(true))
    {
        return invalid();
    }
    let passes = spec
        .profile
        .get("passes")
        .and_then(Value::as_array)
        .ok_or(StateError::ProviderSelectionInvalid)?;
    if (passes.is_empty() && spec.profile.get("deleted_album_bindings").is_none())
        || passes.iter().any(|p| {
            p.get("key")
                .and_then(Value::as_str)
                .is_none_or(|key| key.trim().is_empty())
                || p.pointer("/scope/zone") != Some(&spec.zone)
        })
    {
        return invalid();
    }
    let keys: HashSet<_> = passes
        .iter()
        .filter_map(|p| p.get("key").and_then(Value::as_str))
        .collect();
    if keys.len() != passes.len() {
        return invalid();
    }
    if spec.is_accepted_replay()
        && (spec.metadata_enabled
            || accepted_replay_coverage(&spec.profile)
                != spec.profile.get("coverage").and_then(Value::as_str))
    {
        return invalid();
    }
    Ok(())
}

#[cfg(test)]
#[test]
fn metadata_enrichment_flags_roundtrip_and_require_an_xmp_writer() {
    let zone = serde_json::json!({"zoneName":"PrimarySync", "ownerRecordName":"_defaultOwner"});
    let scope = serde_json::json!({"database":"private", "zone":zone});
    let mut spec = GenerationSpec {
        format: 1,
        scope: scope.to_string(),
        zone: zone.clone(),
        config_hash: "a".repeat(64),
        basis: "b".repeat(64),
        profile: serde_json::json!({"coverage":"observed_selection_window", "passes":[{"key":"primary", "scope":{"zone":zone}}]}),
        metadata_enabled: true,
        metadata_flags: 0,
    };
    const EMBED_XMP: u16 = 1 << 4;
    const XMP_SIDECAR: u16 = 1 << 5;
    const INCLUDE_ALBUMS: u16 = 1 << 6;
    const INCLUDE_PEOPLE: u16 = 1 << 7;
    for flags in [1, 255, 256, 511] {
        spec.metadata_flags = flags;
        let reopened: GenerationSpec =
            serde_json::from_str(&serde_json::to_string(&spec).unwrap()).unwrap();
        assert_eq!(reopened, spec);
        validate_spec(&reopened).unwrap();
    }
    spec.metadata_flags = 512;
    assert!(validate_spec(&spec).is_err());
    for enrichment in [
        INCLUDE_ALBUMS,
        INCLUDE_PEOPLE,
        INCLUDE_ALBUMS | INCLUDE_PEOPLE,
    ] {
        spec.metadata_flags = enrichment;
        assert!(validate_spec(&spec).is_err());
        for writer in [EMBED_XMP, XMP_SIDECAR] {
            spec.metadata_flags = writer | enrichment;
            let reopened: GenerationSpec =
                serde_json::from_str(&serde_json::to_string(&spec).unwrap()).unwrap();
            assert_eq!(reopened, spec);
            validate_spec(&reopened).unwrap();
        }
    }
}

type DestinationProofRow = (
    Option<String>,
    Option<String>,
    bool,
    bool,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

type GenerationRow = (
    String,
    String,
    String,
    String,
    String,
    Vec<u8>,
    String,
    bool,
    bool,
    Option<String>,
);

fn load_generation(conn: &Connection, id: &str) -> Result<ActiveGeneration, StateError> {
    let length: i64 = conn.query_row(
        "SELECT length(specification) FROM provider_active_generations WHERE id=?1",
        [id],
        |r| r.get(0),
    )?;
    if usize::try_from(length).map_or(true, |n| n > MAX_CHUNK_BYTES) {
        return invalid();
    }
    let (account,provider,scope,config_hash,source_basis,bytes,hash,sealed,ready,veto):GenerationRow=
        conn.query_row("SELECT account_key,provider_key,scope,config_hash,basis,specification,specification_hash,sealed,checkpoint_ready,checkpoint_veto FROM provider_active_generations WHERE id=?1",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?,r.get(8)?,r.get(9)?)))?;
    if bytes.len() > MAX_CHUNK_BYTES
        || owner_keys(conn)? != (account, provider)
        || digest(&bytes) != hash
        || (ready && !sealed)
    {
        return invalid();
    }
    let spec: GenerationSpec =
        serde_json::from_slice(&bytes).map_err(|_invalid| StateError::ProviderSelectionInvalid)?;
    validate_spec(&spec)?;
    if spec.is_accepted_replay() && sealed {
        return invalid();
    }
    validate_deleted_selection(conn, &spec)?;
    let metadata_enabled: bool = conn.query_row(
        "SELECT metadata_enabled FROM provider_active_generations WHERE id=?1",
        [id],
        |r| r.get(0),
    )?;
    if metadata_enabled != spec.metadata_enabled {
        return invalid();
    }
    if (scope, config_hash, source_basis)
        != (
            spec.scope.clone(),
            spec.config_hash.clone(),
            spec.basis.clone(),
        )
    {
        return invalid();
    }
    let root = ActiveGeneration {
        id: id.to_owned(),
        spec,
        sealed,
        checkpoint_ready: ready,
        checkpoint_veto: veto,
    };
    let (seal, header): (Option<String>, Option<String>) = conn.query_row(
        "SELECT seal_hash,seal_header_hash FROM provider_active_generations WHERE id=?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if sealed {
        let seal = seal.ok_or(StateError::ProviderSelectionInvalid)?;
        if header.as_deref()
            != Some(seal_header(&root, ready, root.checkpoint_veto.as_deref(), &seal)?.as_str())
        {
            return invalid();
        }
    } else if ready || root.checkpoint_veto.is_some() || seal.is_some() || header.is_some() {
        return invalid();
    }
    let (cursor_length, replayed): (Option<i64>, i64) = conn.query_row(
        "SELECT length(replay_after),last_replayed_at FROM provider_active_generations WHERE id=?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if replayed < 0
        || cursor_length.is_some_and(|length| {
            usize::try_from(length).map_or(true, |length| length > MAX_CHUNK_BYTES)
        })
    {
        return invalid();
    }
    if cursor_length.is_some() {
        let cursor: String = conn.query_row(
            "SELECT replay_after FROM provider_active_generations WHERE id=?1",
            [id],
            |r| r.get(0),
        )?;
        let (key, child): (String, String) = serde_json::from_str(&cursor)
            .map_err(|_invalid| StateError::ProviderSelectionInvalid)?;
        let exists: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM provider_active_decisions WHERE generation=?1 AND pass_key=?2 AND child=?3)", params![id,key,child], |r|r.get(0))?;
        if !exists {
            return invalid();
        }
    }
    Ok(root)
}

/// Audit immutable headers before a normalized scope/config predicate can hide
/// a corrupted historical root. This does not decode retained rank pages.
fn validate_generation_headers(conn: &Connection) -> Result<(), StateError> {
    let mut statement = conn.prepare("SELECT id FROM provider_active_generations ORDER BY id")?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        load_generation(conn, &row.get::<_, String>(0)?)?;
    }
    Ok(())
}

fn validate_scope_decisions(
    conn: &Connection,
    scope: &str,
    replacement: Option<(&str, &str)>,
) -> Result<(), StateError> {
    // Each caller holds one transaction. Check every header before trusting
    // its scope, then reuse that validated header for this generation's rows.
    let mut generations = conn.prepare(&format!(
        "SELECT g.id, ({SUPERSEDED_SELECTION}) FROM provider_active_generations g ORDER BY g.id"
    ))?;
    let mut decisions = conn.prepare(
        "SELECT d.pass_key,d.child FROM provider_active_decisions d WHERE d.generation=?1
         AND (NOT ?2 OR EXISTS(SELECT 1 FROM provider_active_destinations w
             WHERE w.generation=d.generation AND w.pass_key=d.pass_key AND w.child=d.child
               AND w.prepared_hash IS NOT NULL))
         ORDER BY d.pass_key,d.child",
    )?;
    let mut roots = generations.query([])?;
    while let Some(row) = roots.next()? {
        let root = load_generation(conn, &row.get::<_, String>(0)?)?;
        if root.spec.scope == scope {
            let historical = row.get::<_, bool>(1)?
                || replacement.is_some_and(|(config, generation)| {
                    root.spec.config_hash == config && root.id != generation
                });
            let mut rows = decisions.query(params![root.id, historical])?;
            while let Some(row) = rows.next()? {
                load_decision_with_sources(
                    conn,
                    &root,
                    &row.get::<_, String>(0)?,
                    &row.get::<_, String>(1)?,
                    false,
                )?;
            }
        }
    }
    Ok(())
}

fn seal_header(
    root: &ActiveGeneration,
    ready: bool,
    veto: Option<&str>,
    contents: &str,
) -> Result<String, StateError> {
    Ok(digest(&encode(&(
        &root.id, &root.spec, ready, veto, contents,
    ))?))
}

fn intent_hash(
    root: &ActiveGeneration,
    manifest: &ActiveDecision,
    destination: &SelectionDestination,
    group: &str,
) -> Result<String, StateError> {
    Ok(digest(&encode(&(
        &root.id,
        &root.spec,
        digest(&encode(manifest)?),
        destination,
        group,
    ))?))
}

fn progress_hash(
    root: &ActiveGeneration,
    manifest: &ActiveDecision,
    destination: &SelectionDestination,
    intent: Option<&str>,
    metadata: bool,
    local: &str,
    source: &str,
) -> Result<String, StateError> {
    Ok(digest(&encode(&(
        &root.id,
        digest(&encode(manifest)?),
        destination,
        intent,
        metadata,
        local,
        source,
    ))?))
}

/// The output belongs to this exact immutable writer intent and verified
/// input. It is retained across publication/finalization interruption only.
fn prepared_metadata_hash(
    conn: &Connection,
    root: &ActiveGeneration,
    manifest: &ActiveDecision,
    destination: &SelectionDestination,
    output: &str,
    size: u64,
) -> Result<String, StateError> {
    if output.len() != 64 || !output.bytes().all(|b| b.is_ascii_hexdigit()) {
        return invalid();
    }
    let (intent,local,source):(String,String,Option<String>)=conn.query_row(
        "SELECT intent_hash,local_checksum,source_checksum FROM provider_active_destinations WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5 AND admitted=1 AND (verified_media=1 OR preserved_local IS NOT NULL)",
        params![root.id,manifest.decision.pass_key,manifest.decision.child,destination.version_size,destination.path.key()?],
        |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)),
    )?;
    if let Some(preserved) = preserved_local(conn, root, manifest, destination)? {
        return Ok(digest(&encode(&(
            intent, local, source, output, size, preserved,
        ))?));
    }
    Ok(digest(&encode(&(intent, local, source, output, size))?))
}

fn prepared_metadata(
    conn: &Connection,
    root: &ActiveGeneration,
    manifest: &ActiveDecision,
    destination: &SelectionDestination,
) -> Result<Option<(String, u64)>, StateError> {
    let (output,size,hash):(Option<String>,Option<i64>,Option<String>)=conn.query_row(
        "SELECT prepared_checksum,prepared_size,prepared_hash FROM provider_active_destinations WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5",
        params![root.id,manifest.decision.pass_key,manifest.decision.child,destination.version_size,destination.path.key()?],
        |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)),
    )?;
    match (output, size, hash) {
        (None, None, None) => Ok(None),
        (Some(output), Some(size), Some(hash)) => {
            let size =
                u64::try_from(size).map_err(|_invalid| StateError::ProviderSelectionInvalid)?;
            if prepared_metadata_hash(conn, root, manifest, destination, &output, size)? != hash {
                return invalid();
            }
            Ok(Some((output, size)))
        }
        _ => invalid(),
    }
}

fn refresh_progress_hash(
    conn: &Connection,
    root: &ActiveGeneration,
    manifest: &ActiveDecision,
    destination: &SelectionDestination,
) -> Result<(), StateError> {
    let path = destination.path.key()?;
    let (media,metadata,local,source,intent):(bool,bool,Option<String>,Option<String>,Option<String>)=conn.query_row("SELECT verified_media,verified_metadata,local_checksum,source_checksum,intent_hash FROM provider_active_destinations WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5",params![root.id,manifest.decision.pass_key,manifest.decision.child,destination.version_size,path],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?;
    let preserved = preserved_local(conn, root, manifest, destination)?;
    let hash = if let Some(preserved) = &preserved {
        if media || source.is_some() || local.as_deref() != Some(&preserved.checksum) {
            return invalid();
        }
        Some(preserved_progress_hash(
            root,
            manifest,
            destination,
            intent.as_deref(),
            metadata,
            preserved,
        )?)
    } else if media {
        Some(progress_hash(
            root,
            manifest,
            destination,
            intent.as_deref(),
            metadata,
            local
                .as_deref()
                .ok_or(StateError::ProviderSelectionInvalid)?,
            source
                .as_deref()
                .ok_or(StateError::ProviderSelectionInvalid)?,
        )?)
    } else {
        None
    };
    conn.execute("UPDATE provider_active_destinations SET progress_hash=?6 WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5",params![root.id,manifest.decision.pass_key,manifest.decision.child,destination.version_size,path,hash])?;
    Ok(())
}

fn check_write_charge(additional: u64, capacity: u64) -> Result<(), StateError> {
    if additional > capacity {
        return Err(StateError::ProviderSelectionFull);
    }
    Ok(())
}

fn pass<'a>(root: &'a ActiveGeneration, key: &str) -> Result<&'a Value, StateError> {
    pass_from_profile(&root.spec.profile, key).ok_or(StateError::ProviderSelectionInvalid)
}

fn pass_from_profile<'a>(profile: &'a Value, key: &str) -> Option<&'a Value> {
    profile
        .get("passes")?
        .as_array()?
        .iter()
        .find(|pass| pass.get("key").and_then(Value::as_str) == Some(key))
}

#[cfg(test)]
thread_local! {
    static RANK_DECODE_METRICS: std::cell::Cell<(usize, usize)> = const { std::cell::Cell::new((0, 0)) };
}

struct SelectionSourcePage {
    pass_key: String,
    response: Value,
    selected_children: Vec<Option<String>>,
    membership: bool,
}

struct SelectionSourceProof {
    pass_key: String,
    selected_children: Vec<Option<String>>,
    membership: bool,
}

fn load_rank(
    conn: &Connection,
    root: &ActiveGeneration,
    page: &str,
) -> Result<SelectionSourcePage, StateError> {
    // Filter before materializing the blobs, including for unbudgeted callers
    // such as retirement. Read the relation flag from the same row.
    let limit =
        i64::try_from(MAX_CHUNK_BYTES).map_err(|_overflow| StateError::ProviderSelectionInvalid)?;
    let (generation, key, request, request_hash, body, body_hash, has_relation):
        (String, String, Vec<u8>, String, Vec<u8>, String, bool) = conn.query_row(
        "SELECT generation,pass_key,request,request_hash,body,body_hash,has_relation
         FROM provider_selection_rank_pages WHERE id=?1 AND length(request)<=?2 AND length(body)<=?2",
        params![page, limit],
        |row| row.try_into(),
    ).optional()?.ok_or(StateError::ProviderSelectionInvalid)?;
    if generation != root.id
        || request.len() > MAX_CHUNK_BYTES
        || body.len() > MAX_CHUNK_BYTES
        || digest(&request) != request_hash
        || digest(&body) != body_hash
        || digest(&encode(&(&generation, &key, &request_hash, &body_hash))?) != page
    {
        return invalid();
    }
    let request: Value = serde_json::from_slice(&request)
        .map_err(|_invalid| StateError::ProviderSelectionInvalid)?;
    validate_rank_request(root, &key, &request)?;
    #[cfg(test)]
    RANK_DECODE_METRICS.with(|metrics| {
        let (count, bytes) = metrics.get();
        metrics.set((count + 1, bytes + body.len()));
    });
    let response = selection_source_response(root, &request, &body)?;
    let records = response
        .get("records")
        .and_then(Value::as_array)
        .ok_or(StateError::ProviderSelectionInvalid)?;
    let mut statement = conn.prepare("SELECT ordinal,record_name,record_type,deleted FROM provider_selection_rank_records WHERE page_id=?1 ORDER BY ordinal")?;
    let mut rows = statement.query([page])?;
    let foreign = root.spec.accepted_foreign_replay(&key).is_some();
    let membership_container =
        if (foreign || !root.spec.is_accepted_replay()) && request.get("zones").is_some() {
            Some(
                pass(root, &key)?
                    .pointer("/scope/container_id")
                    .and_then(Value::as_str)
                    .ok_or(StateError::ProviderSelectionInvalid)?,
            )
        } else {
            None
        };
    let mut selected_children = Vec::with_capacity(records.len());
    let mut observed_relation = false;
    for (ordinal, record) in records.iter().enumerate() {
        let kind = record.get("recordType").and_then(Value::as_str);
        observed_relation |= kind == Some("CPLContainerRelation");
        let Some(row) = rows.next()? else {
            return invalid();
        };
        if row.get::<_, i64>(0)?
            != i64::try_from(ordinal).map_err(|_invalid| StateError::ProviderSelectionInvalid)?
            || row.get::<_, String>(1)?
                != record
                    .get("recordName")
                    .and_then(Value::as_str)
                    .ok_or(StateError::ProviderSelectionInvalid)?
            || row.get::<_, Option<String>>(2)?.as_deref() != kind
            || row.get::<_, bool>(3)?
                != record
                    .get("deleted")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
        {
            return invalid();
        }
        let child = if let Some(container) = membership_container {
            if foreign {
                crate::icloud::photos::ReplayAlbumLink::Live.child(
                    record,
                    container,
                    &root.spec.zone,
                )
            } else {
                crate::icloud::photos::selection_membership_child(
                    record,
                    container,
                    &root.spec.zone,
                )
                .map_err(|_invalid| StateError::ProviderSelectionInvalid)?
            }
        } else if root.spec.is_accepted_replay()
            && (kind != Some("CPLAsset")
                || record.get("deleted").and_then(Value::as_bool) == Some(true))
        {
            None
        } else {
            record.get("recordName").and_then(Value::as_str)
        };
        selected_children.push(child.map(str::to_owned));
    }
    if rows.next()?.is_some() || has_relation != observed_relation {
        return invalid();
    }
    Ok(SelectionSourcePage {
        pass_key: key,
        response,
        selected_children,
        membership: membership_container.is_some(),
    })
}

/// Compact raw/index proofs live only within one owning operation. Parsed pages
/// are released immediately. Validation bounds unique raw inputs per operation.
struct RankProofs<'a> {
    conn: &'a Connection,
    root: &'a ActiveGeneration,
    pages: HashMap<String, SelectionSourceProof>,
    bytes: usize,
    capacity: usize,
}

impl<'a> RankProofs<'a> {
    fn new(conn: &'a Connection, root: &'a ActiveGeneration) -> Self {
        Self {
            conn,
            root,
            pages: HashMap::new(),
            bytes: 0,
            capacity: MAX_VALIDATION_BYTES,
        }
    }

    fn verify_source(
        &mut self,
        source: &RankSource,
        key: &str,
        child: &str,
    ) -> Result<(), StateError> {
        if !self.matches_source(source, key, child)? {
            return invalid();
        }
        Ok(())
    }

    fn matches_source(
        &mut self,
        source: &RankSource,
        key: &str,
        child: &str,
    ) -> Result<bool, StateError> {
        if !self.pages.contains_key(&source.page_id) {
            let additional = rank_input_bytes(self.conn, &source.page_id)?;
            if additional > self.capacity.saturating_sub(self.bytes) {
                return Err(StateError::ProviderSelectionFull);
            }
            let proof = load_rank_proof(self.conn, self.root, &source.page_id)?;
            self.pages.insert(source.page_id.clone(), proof);
            self.bytes += additional;
        }
        let ordinal = usize::try_from(source.ordinal)
            .map_err(|_invalid| StateError::ProviderSelectionInvalid)?;
        Ok(self.pages.get(&source.page_id).is_some_and(|proof| {
            proof.pass_key == key
                && proof
                    .selected_children
                    .get(ordinal)
                    .and_then(Option::as_deref)
                    == Some(child)
        }))
    }

    fn is_membership_source(
        &mut self,
        source: &RankSource,
        key: &str,
        child: &str,
    ) -> Result<bool, StateError> {
        Ok(self.matches_source(source, key, child)?
            && self
                .pages
                .get(&source.page_id)
                .is_some_and(|proof| proof.membership))
    }
}

fn rank_input_bytes(conn: &Connection, page: &str) -> Result<usize, StateError> {
    let (request, body): (i64, i64) = conn.query_row(
        "SELECT length(request),length(body) FROM provider_selection_rank_pages WHERE id=?1",
        [page],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let mut additional = 0_usize;
    for length in [request, body] {
        let length =
            usize::try_from(length).map_err(|_invalid| StateError::ProviderSelectionInvalid)?;
        if length > MAX_CHUNK_BYTES {
            return invalid();
        }
        additional = additional
            .checked_add(length)
            .ok_or(StateError::ProviderSelectionFull)?;
    }
    Ok(additional)
}

/// Reuse only within one owning operation. Every new operation revalidates raw
/// hashes, strict scope and the complete normalized index. No ordinal is lost.
struct RankPages<'a> {
    conn: &'a Connection,
    root: &'a ActiveGeneration,
    pages: HashMap<String, (String, Arc<Value>)>,
    bytes: usize,
    capacity: usize,
}

impl<'a> RankPages<'a> {
    fn new(conn: &'a Connection, root: &'a ActiveGeneration) -> Self {
        Self {
            conn,
            root,
            pages: HashMap::new(),
            bytes: 0,
            capacity: MAX_OBSERVED_BYTES,
        }
    }

    fn load(&mut self, page: &str) -> Result<(String, Arc<Value>), StateError> {
        if let Some((key, body)) = self.pages.get(page) {
            return Ok((key.clone(), Arc::clone(body)));
        }
        let additional = rank_input_bytes(self.conn, page)?;
        if additional > self.capacity.saturating_sub(self.bytes) {
            return Err(StateError::ProviderSelectionFull);
        }
        let source = load_rank(self.conn, self.root, page)?;
        let key = source.pass_key;
        let body = Arc::new(source.response);
        self.bytes += additional;
        self.pages
            .insert(page.to_owned(), (key.clone(), Arc::clone(&body)));
        Ok((key, body))
    }
}

fn validate_rank_request(
    root: &ActiveGeneration,
    key: &str,
    request: &Value,
) -> Result<(), StateError> {
    if root.spec.is_accepted_replay() {
        if !accepted_replay_pass(&root.spec.profile, key) {
            return invalid();
        }
        let Some(cursor) = request
            .pointer("/zones/0/syncToken")
            .and_then(Value::as_str)
            .filter(|cursor| !cursor.trim().is_empty())
        else {
            return invalid();
        };
        if request
            != &crate::icloud::photos::queries::build_changes_zone_request(
                &root.spec.zone,
                Some(cursor),
                crate::icloud::photos::queries::CHANGES_REPLAY_PAGE_SIZE,
            )
        {
            return invalid();
        }
        return Ok(());
    }
    let query = if key == "global-frontier" {
        serde_json::json!({"list_type":"CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted","query_filter":null})
    } else {
        pass(root, key)?
            .get("scope")
            .cloned()
            .ok_or(StateError::ProviderSelectionInvalid)?
    };
    if let Some(zones) = request.get("zones").and_then(Value::as_array) {
        let zone = zones.first().ok_or(StateError::ProviderSelectionInvalid)?;
        if key == "global-frontier"
            || query
                .get("container_id")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
            || zones.len() != 1
            || zone.get("zoneID") != Some(&root.spec.zone)
            || zone
                .get("resultsLimit")
                .and_then(Value::as_u64)
                .is_none_or(|limit| limit == 0)
        {
            return invalid();
        }
        return Ok(());
    }
    let filters = request
        .pointer("/query/filterBy")
        .and_then(Value::as_array)
        .ok_or(StateError::ProviderSelectionInvalid)?;
    let offset = filters
        .first()
        .and_then(|f| f.pointer("/fieldValue/value"))
        .and_then(Value::as_u64)
        .ok_or(StateError::ProviderSelectionInvalid)?;
    let mut expected = vec![
        serde_json::json!({"fieldName":"startRank","fieldValue":{"type":"INT64","value":offset},"comparator":"EQUALS"}),
        serde_json::json!({"fieldName":"direction","fieldValue":{"type":"STRING","value":"ASCENDING"},"comparator":"EQUALS"}),
    ];
    if let Some(extra) = query.get("query_filter").and_then(Value::as_array) {
        expected.extend(extra.iter().cloned())
    }
    if request.get("zoneID") != Some(&root.spec.zone)
        || request.pointer("/query/recordType") != query.get("list_type")
        || *filters != expected
        || request
            .get("resultsLimit")
            .and_then(Value::as_u64)
            .is_none_or(|n| n == 0)
    {
        return invalid();
    }
    Ok(())
}

fn selection_source_response(
    root: &ActiveGeneration,
    request: &Value,
    body: &[u8],
) -> Result<Value, StateError> {
    let response = crate::icloud::photos::validated_selection_page(request, body)
        .map_err(|_invalid| StateError::ProviderSelectionInvalid)?;
    if root.spec.is_accepted_replay()
        && response
            .pointer("/zones/0/moreComing")
            .and_then(Value::as_bool)
            != Some(true)
    {
        return invalid();
    }
    Ok(response)
}

fn capture_selection_source(
    tx: &rusqlite::Transaction<'_>,
    root: &ActiveGeneration,
    key: &str,
    request: &Value,
    body: &[u8],
    capacity: u64,
) -> Result<String, StateError> {
    validate_rank_request(root, key, request)?;
    let response = selection_source_response(root, request, body)?;
    let generation = &root.id;
    let request = encode(request)?;
    let request_hash = digest(&request);
    let body_hash = digest(body);
    let id = digest(&encode(&(generation, key, &request_hash, &body_hash))?);
    let exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM provider_selection_rank_pages WHERE id=?1)",
        [&id],
        |row| row.get(0),
    )?;
    if exists {
        load_rank_proof(tx, root, &id)?;
        return Ok(id);
    }
    if root.sealed {
        return invalid();
    }
    let records = response
        .get("records")
        .and_then(Value::as_array)
        .ok_or(StateError::ProviderSelectionInvalid)?;
    let has_relation = records.iter().any(|record| {
        record.get("recordType").and_then(Value::as_str) == Some("CPLContainerRelation")
    });
    let indexed_bytes = records
        .iter()
        .map(|record| {
            record
                .get("recordName")
                .and_then(Value::as_str)
                .map_or(0, str::len)
                + record
                    .get("recordType")
                    .and_then(Value::as_str)
                    .map_or(0, str::len)
                + id.len()
                + 32
        })
        .sum::<usize>();
    let charged = u64::try_from(
        request.len() + body.len() + generation.len() + key.len() + id.len() + indexed_bytes + 256,
    )
    .map_err(|_invalid| StateError::ProviderSelectionFull)?;
    check_write_charge(charged, capacity)?;
    tx.execute("INSERT INTO provider_selection_rank_pages(id,generation,pass_key,request,request_hash,body,body_hash,charged_bytes,has_relation) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",params![id,generation,key,request,request_hash,body,body_hash,i64::try_from(charged).map_err(|_invalid|StateError::ProviderSelectionFull)?,has_relation])?;
    for (ordinal, record) in records.iter().enumerate() {
        tx.execute("INSERT INTO provider_selection_rank_records(page_id,ordinal,record_name,record_type,deleted) VALUES(?1,?2,?3,?4,?5)",params![id,i64::try_from(ordinal).map_err(|_invalid|StateError::ProviderSelectionFull)?,record.get("recordName").and_then(Value::as_str),record.get("recordType").and_then(Value::as_str),record.get("deleted").and_then(Value::as_bool).unwrap_or(false)])?;
    }
    Ok(id)
}

fn outcome_name(outcome: SelectionOutcome) -> &'static str {
    match outcome {
        SelectionOutcome::Selected => "selected",
        SelectionOutcome::Excluded => "excluded",
        SelectionOutcome::Deferred => "deferred",
    }
}

fn validate_decision(
    conn: &Connection,
    root: &ActiveGeneration,
    manifest: &ActiveDecision,
    verify_body: bool,
) -> Result<(), StateError> {
    if conn.is_autocommit() {
        let tx = conn.unchecked_transaction()?;
        validate_decision_with_pages(
            &tx,
            root,
            manifest,
            verify_body,
            &mut RankProofs::new(&tx, root),
        )?;
        tx.commit()?;
        Ok(())
    } else {
        validate_decision_with_pages(
            conn,
            root,
            manifest,
            verify_body,
            &mut RankProofs::new(conn, root),
        )
        .map(drop)
    }
}

fn confirmed_decision_asset(
    manifest: &ActiveDecision,
    root: &ActiveGeneration,
) -> Result<Option<crate::icloud::photos::PhotoAsset>, StateError> {
    let decision = &manifest.decision;
    match (&decision.confirmation, &decision.master) {
        (Some(bytes), Some(master)) if bytes.len() <= MAX_CHUNK_BYTES => {
            crate::icloud::photos::current_asset(
                bytes,
                &decision.child,
                master,
                manifest.media_zone(root),
            )
            .map(Some)
            .map_err(|_invalid| StateError::ProviderSelectionInvalid)
        }
        (None, None) if decision.outcome == SelectionOutcome::Deferred => Ok(None),
        _ => invalid(),
    }
}

fn validate_decision_with_pages(
    conn: &Connection,
    root: &ActiveGeneration,
    manifest: &ActiveDecision,
    verify_body: bool,
    pages: &mut RankProofs<'_>,
) -> Result<Option<crate::icloud::photos::PhotoAsset>, StateError> {
    let decision = &manifest.decision;
    pass(root, &decision.pass_key)?;
    validate_media_source(root, manifest)?;
    if decision.child.trim().is_empty()
        || manifest.sources.is_empty()
        || decision.destinations.len() > 32
        || (decision.outcome == SelectionOutcome::Selected && !decision.reason.is_empty())
        || (decision.outcome != SelectionOutcome::Selected
            && (decision.reason.is_empty() || !decision.destinations.is_empty()))
    {
        return invalid();
    }
    let mut sources = HashSet::new();
    for source in &manifest.sources {
        if !sources.insert((&source.page_id, source.ordinal)) {
            return invalid();
        }
        if decision
            .media_source
            .as_ref()
            .is_some_and(|media| media.scope != root.spec.scope)
            && !pages.is_membership_source(source, &decision.pass_key, &decision.child)?
        {
            return invalid();
        }
        if !verify_body && !root.spec.is_accepted_replay() {
            let valid:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM provider_selection_rank_records r JOIN provider_selection_rank_pages p ON p.id=r.page_id WHERE p.generation=?1 AND p.pass_key=?2 AND r.page_id=?3 AND r.ordinal=?4 AND r.record_name=?5 AND r.record_type IS NOT 'CPLContainerRelation')",params![root.id,decision.pass_key,source.page_id,source.ordinal,decision.child],|r|r.get(0))?;
            if valid {
                continue;
            }
        }
        pages.verify_source(source, &decision.pass_key, &decision.child)?;
    }
    let photo = confirmed_decision_asset(manifest, root)?;
    if root.spec.is_accepted_favorites(&decision.pass_key)
        && decision.outcome == SelectionOutcome::Selected
        && !photo
            .as_ref()
            .is_some_and(crate::icloud::photos::PhotoAsset::confirms_favorite_membership)
    {
        return invalid();
    }
    if root
        .spec
        .accepted_album_requirement(&decision.pass_key)
        .is_some()
        && decision.outcome == SelectionOutcome::Selected
        && !accepted_album_proof_matches(conn, root, decision, decision.confirmation.as_deref())?
    {
        return invalid();
    }
    if manifest.state_id != decision.child {
        let Some(master) = decision.master.as_deref() else {
            return invalid();
        };
        let valid:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM legacy_master_state_owners WHERE library=?1 AND master_record_name=?2 AND asset_record_name=?3)",params![manifest.media_zone(root).get("zoneName").and_then(Value::as_str),manifest.state_id,decision.child],|r|r.get(0))?;
        if manifest.state_id != master || !valid {
            return invalid();
        }
    }
    let mut destinations = HashSet::new();
    for destination in &decision.destinations {
        let path = destination.path.to_path();
        if super::provider_selection::SelectionPath::from_path(&path) != destination.path
            || !path.is_absolute()
            || path.file_name().is_none()
            || !destinations.insert((&destination.version_size, destination.path.key()?))
        {
            return invalid();
        }
        let photo = photo.as_ref().ok_or(StateError::ProviderSelectionInvalid)?;
        let logical = crate::state::VersionSizeKey::from_str(&destination.version_size)
            .ok_or(StateError::ProviderSelectionInvalid)?;
        if !photo.versions().iter().any(|(key, value)| {
            let provider = crate::state::VersionSizeKey::from(*key);
            let matches = provider == logical
                || matches!(
                    (provider, logical),
                    (
                        crate::state::VersionSizeKey::Original,
                        crate::state::VersionSizeKey::Alternative
                    ) | (
                        crate::state::VersionSizeKey::Alternative,
                        crate::state::VersionSizeKey::Original
                    )
                );
            matches
                && value.checksum.as_ref() == destination.checksum
                && value.size == destination.size
                && photo.metadata_arc(provider).metadata_hash.as_deref()
                    == Some(destination.metadata_hash.as_str())
        }) {
            return invalid();
        }
    }
    Ok(photo)
}

impl SqliteStateDb {
    pub(crate) async fn freeze_interrupted_selection(
        &self,
        owner: account::AccountOwner,
        scope: String,
    ) -> Result<(), StateError> {
        self.with_conn_mut("freezing interrupted metadata intent", move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            account::validate_authenticated(&tx, &owner)?;
            validate_generation_headers(&tx)?;
            freeze_interrupted_groupings(&tx, &scope)?;
            tx.commit()?;
            rank_retirement::retire_completed(conn, &owner, &scope)
        })
        .await
    }

    pub(crate) async fn selection_metadata_retry_offset(
        &self,
        owner: account::AccountOwner,
    ) -> Result<usize, StateError> {
        self.with_conn("reading selected metadata retry schedule", move |conn| {
            account::validate_authenticated(conn, &owner)?;
            let length: Option<i64> = conn.query_row("SELECT length(value) FROM metadata WHERE key='active_selection_metadata_retry_offset'", [], |r|r.get(0)).optional()?;
            let Some(length) = length else { return Ok(0); };
            if !(1..=19).contains(&length) { return invalid(); }
            let value: String = conn.query_row("SELECT value FROM metadata WHERE key='active_selection_metadata_retry_offset'", [], |r|r.get(0))?;
            let offset = value.parse::<i64>().map_err(|_invalid|StateError::ProviderSelectionInvalid)?;
            usize::try_from(offset).map_err(|_invalid|StateError::ProviderSelectionInvalid)
        }).await
    }

    pub(crate) async fn set_selection_metadata_retry_offset(
        &self,
        owner: account::AccountOwner,
        offset: usize,
    ) -> Result<(), StateError> {
        let offset =
            i64::try_from(offset).map_err(|_invalid| StateError::ProviderSelectionInvalid)?;
        self.with_conn("retaining selected metadata retry schedule", move |conn| {
            account::validate_authenticated(conn, &owner)?;
            conn.execute("INSERT INTO metadata(key,value) VALUES('active_selection_metadata_retry_offset',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [offset.to_string()])?;
            Ok(())
        }).await
    }

    pub(crate) async fn selection_basis(
        &self,
        owner: account::AccountOwner,
        scope: String,
    ) -> Result<String, StateError> {
        self.with_conn("inspecting selection dependencies", move |conn| {
            account::validate_authenticated(conn, &owner)?;
            basis(conn, &scope)
        })
        .await
    }

    pub(crate) async fn current_selection_generation(
        &self,
        owner: account::AccountOwner,
        scope: String,
        config_hash: String,
    ) -> Result<Option<ActiveGeneration>, StateError> {
        self.with_conn("reading current selection generation",move|conn|{
            let tx=conn.unchecked_transaction()?;
            account::validate_authenticated(&tx,&owner)?;
            let conn=&tx;
            validate_generation_headers(conn)?;
            let current=basis(conn,&scope)?;
            let mut id:Option<String>=conn.query_row(&format!("SELECT g.id FROM provider_active_generations g WHERE {CURRENT_SELECTION_CONFIGURATION} AND NOT ({SUPERSEDED_SELECTION}) AND g.basis=?3 ORDER BY g.created_at DESC,g.id DESC LIMIT 1"),params![scope,config_hash,current],|r|r.get(0)).optional()?;
            if id.is_none() {
                let without_deletes=dependency_basis(conn,&scope,EmptySelectionDeletes::IgnoreAppliedAssetDeletes)?;
                // Reuse only a sealed, checkpoint-ready empty query proof.
                // Keep its original basis and seal unchanged. Deleting an
                // already-known asset cannot introduce membership into it.
                id=conn.query_row(&format!("SELECT g.id FROM provider_active_generations g WHERE {CURRENT_SELECTION_CONFIGURATION} AND NOT ({SUPERSEDED_SELECTION}) AND g.basis=?3 AND g.sealed=1 AND g.checkpoint_ready=1 AND NOT EXISTS(SELECT 1 FROM provider_active_decisions d WHERE d.generation=g.id) ORDER BY g.created_at DESC,g.id DESC LIMIT 1"),params![scope,config_hash,without_deletes],|r|r.get(0)).optional()?;
            }
            let root=id.map(|id|load_generation(conn,&id)).transpose()?;
            if let Some(root)=&root && root.sealed {
                let expected:String=conn.query_row("SELECT seal_hash FROM provider_active_generations WHERE id=?1",[&root.id],|r|r.get(0))?;
                if seal_hash(conn,root,root.checkpoint_ready,root.checkpoint_veto.as_deref())?!=expected {return invalid()}
            }
            if let Some(root)=&root && root.spec.metadata_enabled {
                // Grouping projection during admission does not change the
                // selection configuration. A later actual writer dependency
                // change requires a fresh root and leaves old intents intact.
                let mut statement=conn.prepare("SELECT DISTINCT library,asset_id,grouping_hash FROM provider_active_destinations WHERE generation=?1 AND grouping_hash IS NOT NULL ORDER BY library,asset_id,grouping_hash")?;
                let mut rows=statement.query([&root.id])?;
                while let Some(row)=rows.next()? {
                    let library:String=row.get(0)?;let asset:String=row.get(1)?;let expected:String=row.get(2)?;
                    if grouping_hash(conn,&library,&asset)?!=expected {return Ok(None)}
                }
            }
            Ok(root)
        }).await
    }

    pub(crate) async fn begin_selection_generation(
        &self,
        owner: account::AccountOwner,
        spec: GenerationSpec,
        capacity: u64,
    ) -> Result<ActiveGeneration, StateError> {
        self.with_conn_mut("starting selection generation",move|conn|{
            let tx=conn.transaction_with_behavior(TransactionBehavior::Immediate)?;account::validate_authenticated(&tx,&owner)?;validate_spec(&spec)?;validate_current_deleted_selection(&tx,&spec)?;
            if basis(&tx,&spec.scope)?!=spec.basis {return Err(StateError::ProviderWorkConflict("selection_basis_changed"))}
            if !spec.is_accepted_replay() { freeze_interrupted_groupings(&tx, &spec.scope)?; }
            let bytes=encode(&spec)?;let (account,provider)=owner_keys(&tx)?;
            let id=format!("{:032x}",rand::random::<u128>());
            let charged=u64::try_from(bytes.len()+id.len()+account.len()+provider.len()+spec.scope.len()+spec.config_hash.len()+spec.basis.len()+128).map_err(|_invalid|StateError::ProviderSelectionFull)?;
            check_write_charge(charged,capacity)?;
            tx.execute("INSERT INTO provider_active_generations(id,account_key,provider_key,scope,config_hash,basis,specification,specification_hash,created_at,charged_bytes,metadata_enabled) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,max(?9,coalesce((SELECT max(created_at)+1 FROM provider_active_generations WHERE scope=?4 AND config_hash=?5),?9)),?10,?11)",params![id,account,provider,spec.scope,spec.config_hash,spec.basis,bytes,digest(&bytes),Utc::now().timestamp_millis(),i64::try_from(charged).map_err(|_invalid|StateError::ProviderSelectionFull)?,spec.metadata_enabled])?;
            let out=load_generation(&tx,&id)?;tx.commit()?;Ok(out)
        }).await
    }

    pub(crate) async fn capture_selection_rank_page(
        &self,
        owner: account::AccountOwner,
        generation: String,
        key: String,
        request: Value,
        body: Vec<u8>,
        capacity: u64,
    ) -> Result<(), StateError> {
        self.with_conn_mut("capturing scoped selection rank page", move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            account::validate_authenticated(&tx, &owner)?;
            let root = load_generation(&tx, &generation)?;
            if root.spec.is_accepted_replay() {
                return invalid();
            }
            capture_selection_source(&tx, &root, &key, &request, &body, capacity)?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    /// Attach only an accepted, account-owned inbox page, never synthesized records.
    pub(crate) async fn capture_selection_replay_page(
        &self,
        owner: account::AccountOwner,
        generation: String,
        key: String,
        id: CapturedPageId,
        refreshed: Option<Vec<u8>>,
    ) -> Result<ReplayPageCapture, StateError> {
        self.with_conn_mut("linking accepted replay evidence", move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            account::validate_authenticated(&tx, &owner)?;
            let root = load_generation(&tx, &generation)?;
            if !root.spec.is_accepted_replay() {
                return invalid();
            }
            let page = if super::provider_inbox::released(&tx, id.0)? {
                let cursor: String = tx.query_row(
                    "SELECT p.request_cursor FROM provider_shadow_pages p
                     JOIN account_owner a USING(account_key,provider_key)
                     WHERE a.singleton=1 AND p.id=?1 AND p.scope=?2 AND p.more_coming=1",
                    params![id.0, root.spec.scope],
                    |row| row.get(0),
                )?;
                // A sibling pass or a previous replay can already have kept
                // this exact original. Reuse its bytes, not its admission.
                let body = match refreshed {
                    Some(body) => body,
                    None => {
                        let original: Option<Vec<u8>> = tx
                            .query_row(
                                "SELECT p.body FROM provider_selection_rank_pages p
                             JOIN provider_shadow_pages s ON s.id=?1 AND s.body_hash=p.body_hash
                             WHERE p.generation=?2 AND length(p.body)<=?3 LIMIT 1",
                                params![id.0, root.id, MAX_CHUNK_BYTES as i64],
                                |row| row.get(0),
                            )
                            .optional()?;
                        let Some(body) = original else {
                            return Ok(ReplayPageCapture::Released(cursor));
                        };
                        body
                    }
                };
                let Ok(page) =
                    crate::icloud::photos::catalog_observed_page(body, &root.spec.scope, &cursor)
                else {
                    return Ok(ReplayPageCapture::Captured(Vec::new()));
                };
                if !super::provider_inbox::matches_released_body(&tx, id, &page)? {
                    return Ok(ReplayPageCapture::Captured(Vec::new()));
                }
                page
            } else {
                load_page(&tx, id, MAX_CHUNK_BYTES)?.page
            };
            if page.scope != root.spec.scope
                || !page.more_coming
                || page.request_cursor == page.successor
                || crate::icloud::photos::catalog_observed_page(
                    page.body.clone(),
                    &page.scope,
                    &page.request_cursor,
                )
                .map_err(|_invalid| StateError::ProviderSelectionInvalid)?
                    != page
            {
                return invalid();
            }
            let request = crate::icloud::photos::queries::build_changes_zone_request(
                &root.spec.zone,
                Some(&page.request_cursor),
                crate::icloud::photos::queries::CHANGES_REPLAY_PAGE_SIZE,
            );
            let rank = capture_selection_source(
                &tx,
                &root,
                &key,
                &request,
                &page.body,
                MAX_GENERATION_BYTES,
            )?;
            let children = if root.spec.accepted_foreign_replay(&key).is_some() {
                load_rank_proof(&tx, &root, &rank)?
                    .selected_children
                    .into_iter()
                    .flatten()
                    .collect()
            } else {
                page.identities
                    .into_iter()
                    .filter(|identity| {
                        identity.record_type.as_deref() == Some("CPLAsset") && !identity.deleted
                    })
                    .map(|identity| identity.name)
                    .collect()
            };
            tx.commit()?;
            Ok(ReplayPageCapture::Captured(children))
        })
        .await
    }

    pub(crate) async fn selection_rank_sources(
        &self,
        owner: account::AccountOwner,
        generation: String,
        key: String,
        child: String,
    ) -> Result<Vec<RankSource>, StateError> {
        self.selection_rank_sources_with_budget(owner, generation, key, child, MAX_VALIDATION_BYTES)
            .await
    }

    pub(crate) async fn accepted_album_links(
        &self,
        owner: account::AccountOwner,
        generation: String,
        key: String,
        child: String,
    ) -> Result<Option<Vec<crate::icloud::photos::ReplayAlbumLinkLookup>>, StateError> {
        self.with_conn("reading accepted album links", move |conn| {
            let tx = conn.unchecked_transaction()?;
            account::validate_authenticated(&tx, &owner)?;
            let root = load_generation(&tx, &generation)?;
            let links = accepted_album_links(&tx, &root, &key, &child)?;
            tx.commit()?;
            Ok(match links {
                AcceptedAlbumLinks::Exact(links) => Some(links),
                AcceptedAlbumLinks::Missing | AcceptedAlbumLinks::Ambiguous => None,
            })
        })
        .await
    }

    pub(crate) async fn selection_rank_sources_with_budget(
        &self,
        owner: account::AccountOwner,
        generation: String,
        key: String,
        child: String,
        capacity: usize,
    ) -> Result<Vec<RankSource>, StateError> {
        self.with_conn("reading selection source links", move |conn| {
            let tx = conn.unchecked_transaction()?;
            account::validate_authenticated(&tx, &owner)?;
            let root = load_generation(&tx, &generation)?;
            pass(&root, &key)?;
            // Resolve the two source kinds independently so each branch can
            // use its own index. The name branch probes the child index; the
            // relation branch reads only pages marked during capture or
            // backfilled from retained records. Neither branch walks unrelated
            // records or the pages of an unfiled scan without relations.
            let mut stmt = tx.prepare(
                "SELECT r.page_id FROM provider_selection_rank_records r JOIN provider_selection_rank_pages p ON p.id=r.page_id WHERE p.generation=?1 AND p.pass_key=?2 AND r.record_name=?3
                 UNION
                 SELECT p.id FROM provider_selection_rank_pages p WHERE p.generation=?1 AND p.pass_key=?2 AND p.has_relation=1
                 ORDER BY page_id",
            )?;
            let pages = stmt.query_map(params![generation, key, child], |r| r.get::<_, String>(0))?;
            let mut selected = Vec::new();
            for page in pages {
                let page = page?;
                // Discover sources one page at a time: unrelated relations
                // do not spend the next page's validation allowance.
                if rank_input_bytes(&tx, &page)? > capacity {
                    return Err(StateError::ProviderSelectionFull);
                }
                let proof = load_rank_proof(&tx, &root, &page)?;
                if proof.pass_key != key {
                    return invalid();
                }
                for (ordinal, candidate) in proof.selected_children.iter().enumerate() {
                    if candidate.as_deref() == Some(child.as_str()) {
                        selected.push(RankSource {
                            page_id: page.clone(),
                            ordinal: ordinal.try_into().map_err(|_overflow| StateError::ProviderSelectionInvalid)?,
                        });
                    }
                }
            }
            Ok(selected)
        }).await
    }
}

fn load_decision(
    conn: &Connection,
    root: &ActiveGeneration,
    key: &str,
    child: &str,
) -> Result<ActiveDecision, StateError> {
    load_decision_with_sources(conn, root, key, child, true)
}

fn load_decision_with_sources(
    conn: &Connection,
    root: &ActiveGeneration,
    key: &str,
    child: &str,
    verify_body: bool,
) -> Result<ActiveDecision, StateError> {
    if conn.is_autocommit() {
        let tx = conn.unchecked_transaction()?;
        let manifest = load_decision_with_pages(
            &tx,
            root,
            key,
            child,
            verify_body,
            &mut RankProofs::new(&tx, root),
        )?;
        tx.commit()?;
        Ok(manifest)
    } else {
        load_decision_with_pages(
            conn,
            root,
            key,
            child,
            verify_body,
            &mut RankProofs::new(conn, root),
        )
    }
}

fn load_decision_with_pages(
    conn: &Connection,
    root: &ActiveGeneration,
    key: &str,
    child: &str,
    verify_body: bool,
    pages: &mut RankProofs<'_>,
) -> Result<ActiveDecision, StateError> {
    let (bytes, hash, outcome): (Vec<u8>, String, String) = conn
        .prepare_cached(
            "SELECT manifest,manifest_hash,outcome FROM provider_active_decisions
         WHERE generation=?1 AND pass_key=?2 AND child=?3 AND length(manifest)<=?4",
        )?
        .query_row(
            params![
                root.id,
                key,
                child,
                i64::try_from(MAX_CHUNK_BYTES)
                    .map_err(|_overflow| StateError::ProviderSelectionInvalid)?
            ],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?
        .ok_or(StateError::ProviderSelectionInvalid)?;
    if bytes.len() > MAX_CHUNK_BYTES || digest(&bytes) != hash {
        return invalid();
    }
    let manifest: ActiveDecision =
        serde_json::from_slice(&bytes).map_err(|_invalid| StateError::ProviderSelectionInvalid)?;
    if manifest.decision.pass_key != key
        || manifest.decision.child != child
        || outcome != outcome_name(manifest.decision.outcome)
    {
        return invalid();
    }
    let photo = validate_decision_with_pages(conn, root, &manifest, verify_body, pages)?;
    validate_decision_rows(conn, root, &manifest, photo.as_ref())?;
    Ok(manifest)
}

fn admission_hash(
    conn: &Connection,
    root: &ActiveGeneration,
    key: &str,
    child: &str,
) -> Result<String, StateError> {
    AdmissionRow::load(conn, root, key, child)?.hash(root, key, child)
}

struct AdmissionRow {
    manifest: String,
    admission: String,
    reason: String,
    attempts: i64,
    deadline: i64,
    stored_hash: Option<String>,
}

impl AdmissionRow {
    fn load(
        conn: &Connection,
        root: &ActiveGeneration,
        key: &str,
        child: &str,
    ) -> Result<Self, StateError> {
        Ok(conn
            .prepare_cached(
                "SELECT manifest_hash,admission,reason,attempts,next_retry_at,admission_hash
             FROM provider_active_decisions WHERE generation=?1 AND pass_key=?2 AND child=?3",
            )?
            .query_row(params![root.id, key, child], |r| {
                Ok(Self {
                    manifest: r.get(0)?,
                    admission: r.get(1)?,
                    reason: r.get(2)?,
                    attempts: r.get(3)?,
                    deadline: r.get(4)?,
                    stored_hash: r.get(5)?,
                })
            })?)
    }

    fn hash(&self, root: &ActiveGeneration, key: &str, child: &str) -> Result<String, StateError> {
        Ok(digest(&encode(&(
            &root.id,
            key,
            child,
            &self.manifest,
            &self.admission,
            &self.reason,
            self.attempts,
            self.deadline,
        ))?))
    }
}

fn defer_attempt(
    conn: &Connection,
    root: &ActiveGeneration,
    key: &str,
    child: &str,
) -> Result<(), StateError> {
    let (admission,attempts):(String,u32)=conn.query_row("SELECT admission,attempts FROM provider_active_decisions WHERE generation=?1 AND pass_key=?2 AND child=?3",params![root.id,key,child],|r|Ok((r.get(0)?,r.get(1)?)))?;
    if admission == "excluded" {
        return invalid();
    }
    let delay = (3600_i64 * (1_i64 << attempts.min(5))).min(86400);
    conn.execute("UPDATE provider_active_decisions SET attempts=?4,next_retry_at=?5,reason=CASE WHEN admission='admitted' THEN 'current_confirmation_unavailable' ELSE reason END WHERE generation=?1 AND pass_key=?2 AND child=?3",params![root.id,key,child,attempts.saturating_add(1).min(32),Utc::now().timestamp()+delay])?;
    refresh_admission_hash(conn, root, key, child)
}

fn refresh_admission_hash(
    conn: &Connection,
    root: &ActiveGeneration,
    key: &str,
    child: &str,
) -> Result<(), StateError> {
    let hash = admission_hash(conn, root, key, child)?;
    conn.execute("UPDATE provider_active_decisions SET admission_hash=?4 WHERE generation=?1 AND pass_key=?2 AND child=?3",params![root.id,key,child,hash])?;
    Ok(())
}

fn validate_decision_rows(
    conn: &Connection,
    root: &ActiveGeneration,
    manifest: &ActiveDecision,
    photo: Option<&crate::icloud::photos::PhotoAsset>,
) -> Result<(), StateError> {
    let d = &manifest.decision;
    let row = AdmissionRow::load(conn, root, &d.pass_key, &d.child)?;
    if row.stored_hash.as_deref() != Some(row.hash(root, &d.pass_key, &d.child)?.as_str()) {
        return invalid();
    }
    let AdmissionRow {
        admission,
        reason,
        attempts,
        deadline,
        ..
    } = row;
    let superseded =
        admission == "excluded" && validate_managed_successor(conn, root, manifest, &reason)?;
    let legal = match admission.as_str() {
        "admitted" => {
            d.outcome == SelectionOutcome::Selected
                && ((reason.is_empty() && attempts == 0 && deadline == 0)
                    || (reason == "current_confirmation_unavailable"
                        && (1..=32).contains(&attempts)
                        && deadline > 0))
        }
        "excluded" => {
            superseded
                || (d.outcome == SelectionOutcome::Excluded
                    && reason == d.reason
                    && attempts == 0
                    && deadline == 0)
        }
        "deferred" => {
            d.outcome != SelectionOutcome::Excluded
                && !reason.is_empty()
                && (1..=32).contains(&attempts)
                && deadline > 0
        }
        _ => false,
    };
    if !legal {
        return invalid();
    }
    for (table, count) in [
        ("provider_active_sources", manifest.sources.len()),
        ("provider_active_destinations", d.destinations.len()),
    ] {
        let actual: i64 = conn.query_row(
            &format!(
                "SELECT count(*) FROM {table} WHERE generation=?1 AND pass_key=?2 AND child=?3"
            ),
            params![root.id, d.pass_key, d.child],
            |r| r.get(0),
        )?;
        if usize::try_from(actual).ok() != Some(count) {
            return invalid();
        }
    }
    for source in &manifest.sources {
        let valid:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM provider_active_sources WHERE generation=?1 AND pass_key=?2 AND child=?3 AND page_id=?4 AND ordinal=?5)",params![root.id,d.pass_key,d.child,source.page_id,source.ordinal],|r|r.get(0))?;
        if !valid {
            return invalid();
        }
    }
    for destination in &d.destinations {
        let photo = photo.ok_or(StateError::ProviderSelectionInvalid)?;
        let (group, intent, media, metadata, local, source, progress, successor): DestinationProofRow = conn
            .prepare_cached(
                "SELECT grouping_hash,intent_hash,verified_media,verified_metadata,local_checksum,source_checksum,progress_hash,metadata_successor
                 FROM provider_active_destinations
                 WHERE generation=?1 AND pass_key=?2 AND child=?3 AND asset_id=?4 AND version_size=?5 AND path=?6
                   AND checksum=?7 AND size_bytes=?8 AND metadata_hash=?9 AND master=?10 AND library=?11 AND compat_path=?12
                   AND (verified_media=0 OR ((admitted=1 OR ?13) AND length(local_checksum)=64 AND length(source_checksum)=64))
                   AND admitted=?14 AND created_at IS ?15 AND added_at IS ?16",
            )?
            .query_row(
                params![root.id, d.pass_key, d.child, manifest.state_id, destination.version_size,
                    destination.path.key()?, destination.checksum,
                    i64::try_from(destination.size).map_err(|_invalid| StateError::ProviderSelectionInvalid)?,
                    destination.metadata_hash, d.master, manifest.media_zone(root).get("zoneName").and_then(Value::as_str),
                    destination.path.to_path().to_string_lossy(), superseded, admission=="admitted",
                    encode_asset_date(photo.created()), encode_asset_date(photo.added_date())],
                |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?)),
            )
            .optional()?
            .ok_or(StateError::ProviderSelectionInvalid)?;
        if let Some(link) = successor {
            grouping_handoff::validate_link(
                conn,
                &root.id,
                &d.pass_key,
                &d.child,
                &destination.version_size,
                &destination.path.key()?,
                &link,
            )?;
        }
        match (&group, &intent) {
            (None, None) if !root.sealed => {}
            (Some(group), Some(stored))
                if intent_hash(root, manifest, destination, group)? == *stored => {}
            _ => return invalid(),
        }
        let preserved = preserved_local(conn, root, manifest, destination)?;
        let resolved = media || preserved.is_some();
        if prepared_metadata(conn, root, manifest, destination)?.is_some()
            && (!resolved || metadata || !root.spec.metadata_enabled || intent.is_none())
        {
            return invalid();
        }
        if metadata && (!resolved || (root.spec.metadata_enabled && intent.is_none())) {
            return invalid();
        }
        if let Some(preserved) = &preserved {
            if media
                || source.is_some()
                || local.as_deref() != Some(&preserved.checksum)
                || progress.as_deref()
                    != Some(
                        preserved_progress_hash(
                            root,
                            manifest,
                            destination,
                            intent.as_deref(),
                            metadata,
                            preserved,
                        )?
                        .as_str(),
                    )
            {
                return invalid();
            }
        } else if media {
            let (Some(local), Some(source), Some(stored)) = (local, source, progress) else {
                return invalid();
            };
            // The progress receipt binds independent local/source SHA-256 to
            // the immutable provider generation and destination above. MMCS
            // is opaque and has no content-hash equality relationship.
            if source.len() != 64
                || local.len() != 64
                || !source
                    .bytes()
                    .chain(local.bytes())
                    .all(|b| b.is_ascii_hexdigit())
                || progress_hash(
                    root,
                    manifest,
                    destination,
                    intent.as_deref(),
                    metadata,
                    &local,
                    &source,
                )? != stored
            {
                return invalid();
            }
        } else if local.is_some() || source.is_some() || progress.is_some() {
            return invalid();
        }
    }
    Ok(())
}

/// Reconstruct original observed facts for an unresolved selected identity.
/// Ambiguous/missing bounded evidence remains debt; page order never chooses
/// a version. Every retained candidate must agree on the same selected tuple.
fn observed_asset(
    conn: &Connection,
    root: &ActiveGeneration,
    manifest: &ActiveDecision,
) -> Result<Option<crate::icloud::photos::PhotoAsset>, StateError> {
    observed_asset_matching(
        conn,
        root,
        manifest,
        crate::icloud::photos::same_selected_facts,
    )
}

/// A deferred lookup owns no destination. A later, fully verified selection
/// can account for that same observation without rewriting the old decision.
fn lookup_has_verified_successor(
    conn: &Connection,
    old_root: &ActiveGeneration,
    key: &str,
    child: &str,
) -> Result<bool, StateError> {
    // Check one deterministic candidate. If its facts differ, keep the older
    // work retryable rather than searching through later history for a match.
    // Walk generation headers first so each decision probe uses its primary key.
    let successor: Option<String> = conn
        .query_row(
            "SELECT g.id FROM provider_active_generations g CROSS JOIN provider_active_decisions d
         WHERE d.generation=g.id AND d.pass_key=?1 AND d.child=?2 AND d.admission='admitted'
           AND g.scope=?3 AND g.config_hash=?4
           AND g.created_at>(SELECT created_at FROM provider_active_generations WHERE id=?5)
           AND g.sealed=1 AND g.checkpoint_ready=1 AND g.checkpoint_veto IS NULL
           AND EXISTS(SELECT 1 FROM provider_active_destinations w
             WHERE (w.generation,w.pass_key,w.child)=(d.generation,d.pass_key,d.child))
           AND NOT EXISTS(SELECT 1 FROM provider_active_destinations w
             WHERE (w.generation,w.pass_key,w.child)=(d.generation,d.pass_key,d.child)
               AND (w.admitted=0 OR w.verified_media=0 OR w.preserved_local IS NOT NULL
                 OR w.prepared_hash IS NOT NULL OR w.confirmed_repair IS NOT NULL
                 OR (g.metadata_enabled=1 AND w.verified_metadata=0)))
         ORDER BY g.created_at,g.id LIMIT 1",
            params![
                key,
                child,
                old_root.spec.scope,
                old_root.spec.config_hash,
                old_root.id
            ],
            |row| row.get(0),
        )
        .optional()?;
    let Some(successor) = successor else {
        return Ok(false);
    };
    let verify = || {
        let old = load_decision(conn, old_root, key, child)?;
        if old.decision.outcome != SelectionOutcome::Deferred
            || old.decision.reason != "current_lookup_unresolved"
            || old.decision.confirmation.is_some()
            || !old.decision.destinations.is_empty()
            || old.decision.media_source.is_none()
            || old.media_zone(old_root) != &old_root.spec.zone
        {
            return Ok(false);
        }
        let root = load_generation(conn, &successor)?;
        if root.spec.zone != old_root.spec.zone
            || root.spec.profile != old_root.spec.profile
            || root.spec.metadata_enabled != old_root.spec.metadata_enabled
            || root.spec.metadata_flags != old_root.spec.metadata_flags
        {
            return Ok(false);
        }
        let newer = load_decision(conn, &root, key, child)?;
        if newer.state_id != old.state_id
            || newer.decision.media_source != old.decision.media_source
        {
            return Ok(false);
        }
        // URL renewal is harmless, but aliasing can change which copies the
        // planner selects even when the provider checksums are unchanged.
        let agrees = crate::icloud::photos::PhotoAsset::same_unplanned_download;
        let Some(original) = observed_asset_matching(conn, old_root, &old, agrees)? else {
            return Ok(false);
        };
        Ok(confirmed_decision_asset(&newer, &root)?
            .is_some_and(|current| agrees(&original, &current)))
    };
    match verify() {
        Err(StateError::ProviderSelectionFull) => Ok(false),
        result => result,
    }
}

fn observed_asset_matching(
    conn: &Connection,
    root: &ActiveGeneration,
    manifest: &ActiveDecision,
    agrees: impl Fn(&crate::icloud::photos::PhotoAsset, &crate::icloud::photos::PhotoAsset) -> bool,
) -> Result<Option<crate::icloud::photos::PhotoAsset>, StateError> {
    let mut observed = None;
    let mut pages = RankPages::new(conn, root);
    for source in &manifest.sources {
        let (_, page) = pages.load(&source.page_id)?;
        let Some(child) = page
            .get("records")
            .and_then(Value::as_array)
            .and_then(|records| {
                usize::try_from(source.ordinal)
                    .ok()
                    .and_then(|n| records.get(n))
            })
        else {
            return invalid();
        };
        let Some(master) = child
            .pointer("/fields/masterRef/value/recordName")
            .and_then(Value::as_str)
            .filter(|name| !name.trim().is_empty())
        else {
            return Ok(None);
        };
        let page_ids: Vec<String> = conn
            .prepare(
                "SELECT DISTINCT p.id FROM provider_selection_rank_records r
             JOIN provider_selection_rank_pages p ON p.id=r.page_id
             WHERE p.generation=?1 AND r.record_name=?2 AND (NOT ?3 OR p.pass_key=?4)
             ORDER BY p.id LIMIT 65",
            )?
            .query_map(
                params![
                    root.id,
                    master,
                    root.spec.is_accepted_replay(),
                    manifest.decision.pass_key
                ],
                |row| row.get(0),
            )?
            .collect::<Result<_, _>>()?;
        if page_ids.is_empty() || page_ids.len() > 64 {
            return Ok(None);
        }
        for page_id in page_ids {
            let (_, master_page) = pages.load(&page_id)?;
            for record in master_page
                .get("records")
                .and_then(Value::as_array)
                .ok_or(StateError::ProviderSelectionInvalid)?
                .iter()
                .filter(|record| record.get("recordName").and_then(Value::as_str) == Some(master))
            {
                let body = encode(&serde_json::json!({"records":[child,record]}))?;
                let Ok(asset) = crate::icloud::photos::current_asset(
                    &body,
                    &manifest.decision.child,
                    master,
                    &root.spec.zone,
                ) else {
                    return Ok(None);
                };
                if let Some(previous) = &observed
                    && !agrees(previous, &asset)
                {
                    return Ok(None);
                }
                observed = Some(asset);
            }
        }
    }
    Ok(observed)
}

fn accepted_child_matches_current(
    conn: &Connection,
    root: &ActiveGeneration,
    key: &str,
    fresh: &[u8],
    current: &crate::icloud::photos::PhotoAsset,
) -> Result<bool, StateError> {
    let child = current.asset_record_name();
    let master = current.id();
    if !root.spec.is_accepted_replay()
        || !accepted_replay_pass(&root.spec.profile, key)
        || current.metadata().is_hidden
        || current.metadata().is_deleted
    {
        return Ok(false);
    }
    // A lookup can supply a missing master, not replace captured master
    // evidence or undo a captured tombstone.
    let blocked: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM provider_selection_rank_records r
         JOIN provider_selection_rank_pages p ON p.id=r.page_id
         WHERE p.generation=?1 AND p.pass_key=?2
           AND (r.record_name=?3 OR (r.record_name=?4 AND r.deleted=1)))",
        params![root.id, key, master, child],
        |row| row.get(0),
    )?;
    if blocked {
        return Ok(false);
    }
    let mut statement = conn.prepare(
        "SELECT r.page_id,r.ordinal FROM provider_selection_rank_records r
         JOIN provider_selection_rank_pages p ON p.id=r.page_id
         WHERE p.generation=?1 AND p.pass_key=?2 AND r.record_name=?3",
    )?;
    let mut rows = statement.query(params![root.id, key, child])?;
    let mut pages = RankPages::new(conn, root);
    let mut matched = false;
    while let Some(row) = rows.next()? {
        let (_, page) = pages.load(&row.get::<_, String>(0)?)?;
        let ordinal = row.get::<_, u32>(1)? as usize;
        let record = page
            .get("records")
            .and_then(Value::as_array)
            .and_then(|records| records.get(ordinal))
            .ok_or(StateError::ProviderSelectionInvalid)?;
        if !crate::icloud::photos::accepted_child_matches_lookup(
            record,
            fresh,
            current,
            &root.spec.zone,
        )
        .unwrap_or(false)
        {
            return Ok(false);
        }
        matched = true;
    }
    Ok(matched)
}

impl SqliteStateDb {
    pub(crate) async fn qualify_accepted_child_lookup(
        &self,
        owner: account::AccountOwner,
        generation: String,
        key: String,
        lookup: crate::icloud::photos::CurrentCatalogAsset,
    ) -> Result<Option<crate::icloud::photos::CurrentCatalogAsset>, StateError> {
        self.with_conn("checking exact accepted child lookup", move |conn| {
            let tx = conn.unchecked_transaction()?;
            account::validate_authenticated(&tx, &owner)?;
            let root = load_generation(&tx, &generation)?;
            let qualified =
                accepted_child_matches_current(&tx, &root, &key, &lookup.body, &lookup.asset)?;
            tx.commit()?;
            Ok(qualified.then_some(lookup))
        })
        .await
    }
}

fn accepted_replay_eligibility(
    conn: &Connection,
    root: &ActiveGeneration,
    manifest: &ActiveDecision,
    fresh: Option<&[u8]>,
) -> Result<bool, StateError> {
    let decision = &manifest.decision;
    let (Some(fresh), Some(master)) = (fresh, decision.master.as_deref()) else {
        return Ok(false);
    };
    let foreign = root
        .spec
        .accepted_foreign_replay(&decision.pass_key)
        .is_some();
    if !foreign && manifest.media_zone(root) != &root.spec.zone {
        return Ok(false);
    }
    if root
        .spec
        .accepted_album_requirement(&decision.pass_key)
        .is_some()
        && decision.confirmation.as_deref() != Some(fresh)
        && !accepted_album_proof_matches(conn, root, decision, Some(fresh))?
    {
        return Ok(false);
    }
    let deleted: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM provider_selection_rank_records r
         JOIN provider_selection_rank_pages p ON p.id=r.page_id
          WHERE p.generation=?1 AND ((r.record_name IN (?2,?3) AND r.deleted=1)
             OR (?4 AND r.record_name=?2)))",
        params![root.id, decision.child, master, foreign],
        |row| row.get(0),
    )?;
    if deleted {
        return Ok(false);
    }
    let agrees = crate::icloud::photos::same_selected_facts;
    let current = crate::icloud::photos::current_asset(
        fresh,
        &decision.child,
        master,
        manifest.media_zone(root),
    )
    .map_err(|_invalid| StateError::ProviderSelectionInvalid)?;
    if current.metadata().is_hidden
        || current.metadata().is_deleted
        || (root.spec.is_accepted_favorites(&decision.pass_key)
            && !current.confirms_favorite_membership())
        || !confirmed_decision_asset(manifest, root)?
            .is_some_and(|selected| agrees(&selected, &current))
    {
        return Ok(false);
    }
    if foreign {
        // Every retained source was validated as an original membership
        // relation. The distinct media facts come only from the bound lookup.
        return Ok(true);
    }
    match observed_asset_matching(conn, root, manifest, agrees)? {
        Some(observed) => Ok(agrees(&observed, &current)),
        None => accepted_child_matches_current(conn, root, &decision.pass_key, fresh, &current),
    }
}

/// Share only unchanged catalog facts. A previous independent publication or
/// a sibling admitted in this generation supplies no proof for the new path.
fn can_share_accepted_album_asset(
    conn: &Connection,
    root: &ActiveGeneration,
    manifest: &ActiveDecision,
    records: &[AssetRecord],
) -> Result<bool, StateError> {
    let decision = &manifest.decision;
    let container = root.spec.accepted_album_container(&decision.pass_key);
    if manifest.state_id != decision.child
        || records.is_empty()
        || root.spec.profile.get(INDEPENDENT_ALBUM_COPIES) != Some(&Value::Bool(true))
    {
        return Ok(false);
    }
    let mut current = conn.prepare_cached(
        "SELECT EXISTS(
            SELECT 1 FROM assets a JOIN asset_master_mappings m
              ON m.library=a.library AND m.asset_record_name=a.id
            WHERE a.library=?1 AND a.id=?2 AND a.version_size=?3 AND m.master_record_name=?4
              AND a.checksum=?5 AND a.size_bytes=?6 AND a.metadata_hash=?7
              AND a.created_at IS ?8 AND a.added_at IS ?9 AND a.is_deleted=0
              AND a.status IN ('pending','failed','downloaded')
              AND a.capture_repair_metadata_hash IS NULL)",
    )?;
    for record in records {
        if !current.query_row(
            params![
                record.library,
                record.id,
                record.version_size.as_str(),
                decision.master,
                record.checksum,
                i64::try_from(record.size_bytes).ok(),
                record.metadata.metadata_hash,
                encode_asset_date(record.created_at),
                record.added_at.map(encode_asset_date)
            ],
            |row| row.get::<_, bool>(0),
        )? {
            return Ok(false);
        }
    }
    // A previous verified publication can identify the catalog rendition
    // without granting membership or completion to this new destination.
    // Match the bound media database and owner as well as the zone name.
    let scope = decision
        .media_source
        .as_ref()
        .map_or(root.spec.scope.as_str(), |s| &s.scope);
    let mut published = conn.prepare_cached(
        "SELECT d.generation,d.pass_key FROM provider_active_destinations d
         JOIN provider_active_generations g ON g.id=d.generation
         JOIN provider_active_decisions p USING(generation,pass_key,child)
         WHERE COALESCE(json_extract(CAST(p.manifest AS TEXT),'$.decision.media_source.scope'),g.scope)=?1
           AND d.library=?2 AND d.child=?3 AND d.asset_id=?3 AND d.master=?4
           AND d.version_size=?5 AND d.checksum=?6 AND d.size_bytes=?7
           AND d.admitted=1 AND d.verified_media=1 AND d.path<>?8 AND d.pass_key<>?9
         ORDER BY g.created_at DESC LIMIT 1",
    )?;
    let mut prior_copy = !decision.destinations.is_empty();
    for destination in &decision.destinations {
        let previous: Option<(String, String)> = published
            .query_row(
                params![
                    scope,
                    manifest.media_zone(root)["zoneName"].as_str(),
                    decision.child,
                    decision.master,
                    destination.version_size,
                    destination.checksum,
                    i64::try_from(destination.size).ok(),
                    destination.path.key()?,
                    decision.pass_key
                ],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((generation, key)) = previous else {
            prior_copy = false;
            break;
        };
        let published_root = load_generation(conn, &generation)?;
        load_decision_with_sources(conn, &published_root, &key, &decision.child, false)?;
    }
    if prior_copy {
        return Ok(true);
    }
    if container.is_none() && !root.spec.mixed_favorites_replay() {
        return Ok(false);
    }
    let mut siblings = conn.prepare_cached(
        "SELECT pass_key FROM provider_active_decisions
         WHERE generation=?1 AND child=?2 AND pass_key<>?3 AND admission='admitted'",
    )?;
    let keys = siblings.query_map(params![root.id, decision.child, decision.pass_key], |row| {
        row.get::<_, String>(0)
    })?;
    for key in keys {
        let key = key?;
        if !root.spec.mixed_favorites_replay()
            && root
                .spec
                .accepted_album_container(&key)
                .is_none_or(|other| Some(other) == container)
        {
            continue;
        }
        let prior = load_decision(conn, root, &key, &decision.child)?;
        if prior.state_id == manifest.state_id
            && prior.decision.master == decision.master
            && prior.media_zone(root) == manifest.media_zone(root)
            && decision.destinations.iter().all(|destination| {
                prior.decision.destinations.iter().any(|other| {
                    other.version_size == destination.version_size
                        && other.checksum == destination.checksum
                        && other.size == destination.size
                        && other.metadata_hash == destination.metadata_hash
                })
            })
        {
            return Ok(true);
        }
    }
    Ok(false)
}

enum AcceptedAlbumLinks {
    Exact(Vec<crate::icloud::photos::ReplayAlbumLinkLookup>),
    Missing,
    Ambiguous,
}

fn accepted_album_links(
    conn: &Connection,
    root: &ActiveGeneration,
    key: &str,
    child: &str,
) -> Result<AcceptedAlbumLinks, StateError> {
    let (containers, status) = root
        .spec
        .accepted_album_requirement(key)
        .ok_or(StateError::ProviderSelectionInvalid)?;
    let foreign = matches!(status, crate::icloud::photos::ReplayAlbumLink::Foreign);
    let mut statement = conn.prepare(
        "SELECT id FROM provider_selection_rank_pages
         WHERE generation=?1 AND pass_key=?2 AND (has_relation=1 OR ?3)",
    )?;
    let ids = statement.query_map(
        params![
            root.id,
            key,
            !matches!(status, crate::icloud::photos::ReplayAlbumLink::Live)
        ],
        |row| row.get::<_, String>(0),
    )?;
    let mut links: Vec<Option<String>> = vec![None; containers.len()];
    let mut refused_names = HashSet::new();
    for id in ids {
        let page = load_rank(conn, root, &id?)?;
        let records = page
            .response
            .get("records")
            .and_then(Value::as_array)
            .ok_or(StateError::ProviderSelectionInvalid)?;
        for record in records {
            for (container, link) in containers.iter().zip(&mut links) {
                if status.child(record, container, &root.spec.zone) != Some(child) {
                    if foreign {
                        if crate::icloud::photos::ReplayAlbumLink::Removed.child(
                            record,
                            container,
                            &root.spec.zone,
                        ) == Some(child)
                        {
                            return Ok(AcceptedAlbumLinks::Ambiguous);
                        }
                        if let Some(name) = record.get("recordName").and_then(Value::as_str) {
                            refused_names.insert(name.to_owned());
                        }
                    }
                    continue;
                }
                let name = record
                    .get("recordName")
                    .and_then(Value::as_str)
                    .ok_or(StateError::ProviderSelectionInvalid)?;
                if link.as_deref().is_some_and(|prior| prior != name) {
                    return Ok(AcceptedAlbumLinks::Ambiguous);
                }
                *link = Some(name.to_owned());
            }
        }
    }
    if links
        .iter()
        .flatten()
        .any(|name| refused_names.contains(name))
    {
        return Ok(AcceptedAlbumLinks::Ambiguous);
    }
    let exact = containers
        .into_iter()
        .zip(links)
        .map(|(container, name)| {
            Some(crate::icloud::photos::ReplayAlbumLinkLookup {
                record_name: name?,
                container: container.to_owned(),
            })
        })
        .collect::<Option<_>>();
    Ok(exact.map_or(AcceptedAlbumLinks::Missing, AcceptedAlbumLinks::Exact))
}

fn accepted_album_proof_matches(
    conn: &Connection,
    root: &ActiveGeneration,
    decision: &SelectionDecision,
    confirmation: Option<&[u8]>,
) -> Result<bool, StateError> {
    let key = &decision.pass_key;
    let child = &decision.child;
    let (Some((containers, status)), Some(body)) =
        (root.spec.accepted_album_requirement(key), confirmation)
    else {
        return Ok(false);
    };
    match accepted_album_links(conn, root, key, child)? {
        AcceptedAlbumLinks::Exact(links) => {
            crate::icloud::photos::replay_album_confirmation_matches(
                body,
                &links,
                child,
                &root.spec.zone,
                status,
            )
            .map_err(|_invalid| StateError::ProviderSelectionInvalid)
        }
        AcceptedAlbumLinks::Missing
            if matches!(status, crate::icloud::photos::ReplayAlbumLink::Live) =>
        {
            let ([container], Some(master)) = (containers.as_slice(), decision.master.as_deref())
            else {
                return invalid();
            };
            let Some(name) = crate::icloud::photos::replay_current_album_link(
                body,
                container,
                child,
                master,
                &root.spec.zone,
            )
            .map_err(|_invalid| StateError::ProviderSelectionInvalid)?
            else {
                return Ok(false);
            };
            // The lookup fills a gap. It cannot replace a captured record,
            // including an opaque tombstone that lacks relation fields.
            Ok(conn.query_row(
                "SELECT NOT EXISTS(SELECT 1 FROM provider_selection_rank_records r
                 JOIN provider_selection_rank_pages p ON p.id=r.page_id
                 WHERE p.generation=?1 AND p.pass_key=?2 AND r.record_name=?3)",
                params![root.id, key, name],
                |row| row.get(0),
            )?)
        }
        AcceptedAlbumLinks::Missing | AcceptedAlbumLinks::Ambiguous => Ok(false),
    }
}

/// Revisions may differ during first inventory, but all retained observations
/// must still identify the exact child and master in the original query zone.
fn first_inventory_identity_matches(
    conn: &Connection,
    root: &ActiveGeneration,
    old: &ActiveDecision,
    current: &crate::icloud::photos::PhotoAsset,
) -> Result<bool, StateError> {
    if old.decision.media_source.is_none() || old.media_zone(root) != &root.spec.zone {
        return Ok(false);
    }
    let zone = root
        .spec
        .zone
        .get("zoneName")
        .and_then(Value::as_str)
        .ok_or(StateError::ProviderSelectionInvalid)?;
    let blocked: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM metadata WHERE key IN (?1,?2))",
        params![
            crate::sync_cycle::sync_token_key(zone),
            crate::sync_cycle::inventory_reconciliation_key(zone)
        ],
        |row| row.get(0),
    )?;
    if blocked {
        return Ok(false);
    }
    let same_identity = |a: &crate::icloud::photos::PhotoAsset,
                         b: &crate::icloud::photos::PhotoAsset| {
        a.asset_record_name() == b.asset_record_name()
            && a.id() == b.id()
            && a.source_zone() == b.source_zone()
    };
    Ok(observed_asset_matching(conn, root, old, same_identity)?
        .is_some_and(|prior| same_identity(&prior, current)))
}

fn validate_records(
    root: &ActiveGeneration,
    manifest: &ActiveDecision,
    records: &[AssetRecord],
) -> Result<(), StateError> {
    let d = &manifest.decision;
    if records.len() != d.destinations.len() {
        return invalid();
    }
    let photo = match (&d.confirmation, &d.master) {
        (Some(body), Some(master)) => Some(
            crate::icloud::photos::current_asset(body, &d.child, master, manifest.media_zone(root))
                .map_err(|_invalid| StateError::ProviderSelectionInvalid)?,
        ),
        _ => None,
    };
    let mut versions = HashSet::new();
    for record in records {
        let photo = photo.as_ref().ok_or(StateError::ProviderSelectionInvalid)?;
        if record.library.as_ref()
            != manifest
                .media_zone(root)
                .get("zoneName")
                .and_then(Value::as_str)
                .ok_or(StateError::ProviderSelectionInvalid)?
            || record.id.as_ref() != manifest.state_id
            || record.created_at != photo.created()
            || record.added_at != Some(photo.added_date())
            || !versions.insert(record.version_size)
            || record.metadata.is_deleted
            || record.metadata.metadata_hash.as_deref()
                != Some(record.metadata.compute_hash().as_str())
        {
            return invalid();
        }
        if !d.destinations.iter().any(|destination| {
            destination.version_size == record.version_size.as_str()
                && destination.checksum == record.checksum.as_ref()
                && destination.size == record.size_bytes
                && record.metadata.metadata_hash.as_deref()
                    == Some(destination.metadata_hash.as_str())
                && destination
                    .path
                    .to_path()
                    .file_name()
                    .and_then(|name| name.to_str())
                    == Some(record.filename.as_ref())
        }) {
            return invalid();
        }
        if !photo.versions().iter().any(|(version, resource)| {
            let metadata = photo.metadata_arc(crate::state::VersionSizeKey::from(*version));
            resource.checksum.as_ref() == record.checksum.as_ref()
                && resource.size == record.size_bytes
                && metadata.metadata_hash == record.metadata.metadata_hash
                && metadata.source == record.metadata.source
                && metadata.provider_data == record.metadata.provider_data
        }) {
            return invalid();
        }
    }
    Ok(())
}

impl SqliteStateDb {
    /// Read-ahead hint only. The producer still validates the decision and its
    /// retry deadline when consuming each observation.
    pub(crate) async fn first_selection_observations(
        &self,
        owner: account::AccountOwner,
        generation: String,
        key: String,
        children: Vec<String>,
    ) -> Result<HashSet<String>, StateError> {
        self.with_conn("finding new selection observations", move |conn| {
            account::validate_authenticated(conn, &owner)?;
            let mut recorded = conn.prepare_cached("SELECT EXISTS(SELECT 1 FROM provider_active_decisions WHERE generation=?1 AND pass_key=?2 AND child=?3)")?;
            let mut new = HashSet::new();
            for child in children {
                if !recorded.query_row(params![generation, key, child], |row| row.get::<_, bool>(0))? {
                    new.insert(child);
                }
            }
            Ok(new)
        }).await
    }

    pub(crate) async fn selection_attempt(
        &self,
        owner: account::AccountOwner,
        generation: String,
        key: String,
        child: String,
    ) -> Result<SelectionAttempt, StateError> {
        self.with_conn("reading selection attempt", move |conn| {
            let tx = conn.unchecked_transaction()?;
            account::validate_authenticated(&tx, &owner)?;
            let root = load_generation(&tx, &generation)?;
            let due: Option<bool> = tx.query_row(
                "SELECT admission='excluded' OR next_retry_at<=?4 FROM provider_active_decisions WHERE generation=?1 AND pass_key=?2 AND child=?3",
                params![generation, key, child, Utc::now().timestamp()], |row| row.get(0),
            ).optional()?;
            let attempt = match due {
                None => SelectionAttempt::Due(None),
                Some(due) => {
                    let previous = load_decision(&tx, &root, &key, &child)?;
                    if due { SelectionAttempt::Due(Some(Box::new(previous))) } else { SelectionAttempt::Deferred }
                }
            };
            tx.commit()?;
            Ok(attempt)
        }).await
    }

    pub(crate) async fn observed_selection_asset(
        &self,
        owner: account::AccountOwner,
        generation: String,
        key: String,
        child: String,
    ) -> Result<Option<crate::icloud::photos::PhotoAsset>, StateError> {
        self.with_conn("reconstructing unresolved selected facts", move |conn| {
            let tx = conn.unchecked_transaction()?;
            account::validate_authenticated(&tx, &owner)?;
            let root = load_generation(&tx, &generation)?;
            let manifest = load_decision(&tx, &root, &key, &child)?;
            observed_asset(&tx, &root, &manifest)
        })
        .await
    }

    /// One historical root per cycle; its bounded decision window and retry
    /// deadlines provide reachable continuation without a bulk history upgrade.
    pub(crate) async fn retained_selection_root(
        &self,
        owner: account::AccountOwner,
        scope: String,
        config_hash: String,
        current: String,
    ) -> Result<Option<ActiveGeneration>, StateError> {
        self.with_conn("scheduling retained selected work",move|conn|{
            let tx=conn.unchecked_transaction()?;
            let conn=&tx;
            account::validate_authenticated(conn,&owner)?;
            // Discovery is not a checkpoint decision. Validate the selected
            // root here and the due manifests when claiming the replay window.
            let mut candidates=conn.prepare(&format!("SELECT g.id, EXISTS(SELECT 1 FROM provider_active_destinations w WHERE w.generation=g.id AND {UNFINISHED} AND EXISTS(SELECT 1 FROM provider_active_decisions d WHERE d.generation=w.generation AND d.pass_key=w.pass_key AND d.child=w.child AND d.next_retry_at<=?4)) AS destination_due
                FROM provider_active_generations g WHERE {CURRENT_SELECTION_CONFIGURATION} AND g.id<>?3 AND NOT ({SUPERSEDED_SELECTION})
                AND (destination_due OR EXISTS(SELECT 1 FROM provider_active_decisions d WHERE d.generation=g.id AND d.admission='deferred' AND d.next_retry_at<=?4))
                ORDER BY g.last_replayed_at,g.created_at,g.id"))?;
            let now=Utc::now().timestamp();
            let mut candidates=candidates.query(params![scope,config_hash,current,now])?;
            let mut deferred=conn.prepare("SELECT pass_key,child,reason='current_lookup_unresolved' FROM provider_active_decisions WHERE generation=?1 AND admission='deferred' AND next_retry_at<=?2")?;
            while let Some(row)=candidates.next()? {
                let root=load_generation(conn,&row.get::<_,String>(0)?)?;
                if row.get::<_,bool>(1)? {return Ok(Some(root))}
                let mut decisions=deferred.query(params![root.id,now])?;
                while let Some(decision)=decisions.next()? {
                    if !decision.get::<_,bool>(2)? || !lookup_has_verified_successor(conn,&root,&decision.get::<_,String>(0)?,&decision.get::<_,String>(1)?)? {
                        return Ok(Some(root));
                    }
                }
            }
            Ok(None)
        }).await
    }

    pub(crate) async fn retained_selection_decisions(
        &self,
        owner: account::AccountOwner,
        generation: String,
    ) -> Result<Vec<ActiveDecision>, StateError> {
        self.with_conn_mut("scheduling bounded retained destinations",move|conn|{
            let tx=conn.transaction_with_behavior(TransactionBehavior::Immediate)?;account::validate_authenticated(&tx,&owner)?;
            let root=load_generation(&tx,&generation)?;
            let stored:Option<String>=tx.query_row("SELECT replay_after FROM provider_active_generations WHERE id=?1",[&generation],|r|r.get(0))?;
            let after:Option<(String,String)>=stored.as_deref().map(|value|serde_json::from_str(value).map_err(|_invalid|StateError::ProviderSelectionInvalid)).transpose()?;
            let mut stmt = tx.prepare(&format!(
                "SELECT d.pass_key,d.child,length(d.manifest),d.admission='deferred' AND d.reason='current_lookup_unresolved'
                 FROM provider_active_decisions d
                 WHERE d.generation=?1 AND (?2 IS NULL OR (d.pass_key,d.child)>(?2,?3))
                   AND (?4 IS NULL OR (d.pass_key,d.child)<=(?4,?5))
                   AND d.next_retry_at<=?6
                   AND (d.admission='deferred' OR EXISTS(
                       SELECT 1 FROM provider_active_destinations w JOIN provider_active_generations g ON g.id=w.generation
                       WHERE w.generation=d.generation AND w.pass_key=d.pass_key AND w.child=d.child AND {UNFINISHED}))
                 ORDER BY d.pass_key,d.child LIMIT ?7"
            ))?;
            let now = Utc::now().timestamp();
            let mut candidates_left = RETAINED_REPLAY_CANDIDATE_LIMIT;
            let mut bytes = 0_usize;
            let mut decisions = Vec::new();
            let mut last = None;
            let mut pages = RankProofs::new(&tx, &root);
            // A suffix filled with settled lookups is not an actionable batch.
            // Wrap once, without reexamining the suffix or exceeding the budget.
            'window: for (lower, upper) in [(after.as_ref(), None), (None, after.as_ref())] {
                let mut rows = stmt.query(params![
                    generation, lower.map(|key| &key.0), lower.map(|key| &key.1),
                    upper.map(|key| &key.0), upper.map(|key| &key.1), now, candidates_left,
                ])?;
                while let Some(row) = rows.next()? {
                    let (key, child, length, lookup): (String, String, i64, bool) =
                        row.try_into()?;
                    candidates_left -= 1;
                    let length = usize::try_from(length)
                        .map_err(|_invalid| StateError::ProviderSelectionInvalid)?;
                    if length > MAX_CHUNK_BYTES {
                        return invalid();
                    }
                    if lookup && lookup_has_verified_successor(&tx, &root, &key, &child)? {
                        last = Some((key, child));
                        continue;
                    }
                    if bytes.saturating_add(length) > MAX_CHUNK_BYTES {
                        break 'window;
                    }
                    let decision = match load_decision_with_pages(&tx, &root, &key, &child, true, &mut pages) {
                        Ok(decision) => decision,
                        Err(StateError::ProviderSelectionFull) => break 'window,
                        Err(error) => return Err(error),
                    };
                    bytes += length;
                    decisions.push(decision);
                    last = Some((key, child));
                    if decisions.len() == RETAINED_REPLAY_DECISION_LIMIT {
                        break 'window;
                    }
                }
                if after.is_none() || candidates_left == 0 || !decisions.is_empty() {
                    break;
                }
            }
            drop(stmt);
            let cursor=last.map(|after|serde_json::to_string(&after).map_err(|_invalid|StateError::ProviderSelectionInvalid)).transpose()?.or(stored);
            tx.execute("UPDATE provider_active_generations SET replay_after=?2,last_replayed_at=?3 WHERE id=?1",params![generation,cursor,Utc::now().timestamp_millis()])?;
            tx.commit()?;Ok(decisions)
        }).await
    }

    pub(crate) async fn defer_selection_confirmation(
        &self,
        owner: account::AccountOwner,
        generation: String,
        key: String,
        child: String,
    ) -> Result<(), StateError> {
        self.with_conn_mut("retaining unavailable selected confirmation", move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            account::validate_authenticated(&tx, &owner)?;
            let root = load_generation(&tx, &generation)?;
            load_decision(&tx, &root, &key, &child)?;
            defer_attempt(&tx, &root, &key, &child)?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    #[cfg(test)]
    pub(crate) async fn make_selection_retry_due(
        &self,
        owner: account::AccountOwner,
        child: String,
    ) -> Result<(), StateError> {
        self.with_conn_mut("advancing synthetic selection retry clock",move|conn|{
            let tx=conn.transaction_with_behavior(TransactionBehavior::Immediate)?;account::validate_authenticated(&tx,&owner)?;
            let keys:Vec<(String,String)>=tx.prepare("SELECT generation,pass_key FROM provider_active_decisions WHERE child=?1 AND admission='deferred'")?.query_map([&child],|r|Ok((r.get(0)?,r.get(1)?)))?.collect::<Result<_,_>>()?;
            for (generation,key) in keys {
                let root=load_generation(&tx,&generation)?;load_decision(&tx,&root,&key,&child)?;
                tx.execute("UPDATE provider_active_decisions SET next_retry_at=1 WHERE generation=?1 AND pass_key=?2 AND child=?3",params![generation,key,child])?;
                refresh_admission_hash(&tx,&root,&key,&child)?;
            }
            tx.commit()?;Ok(())
        }).await
    }

    pub(crate) async fn selection_waiting_for_retry(
        &self,
        owner: account::AccountOwner,
        generation: String,
    ) -> Result<bool, StateError> {
        self.with_conn("checking unresolved selection backoff",move|conn|{
            account::validate_authenticated(conn,&owner)?;let root=load_generation(conn,&generation)?;
            let mut statement=conn.prepare("SELECT pass_key,child FROM provider_active_decisions WHERE generation=?1")?;
            let mut rows=statement.query([&generation])?;
            while let Some(row)=rows.next()? {let key:String=row.get(0)?;let child:String=row.get(1)?;load_decision_with_sources(conn,&root,&key,&child,false)?;}
            Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM provider_active_decisions WHERE generation=?1 AND admission<>'excluded' AND next_retry_at>?2) AND NOT EXISTS(SELECT 1 FROM provider_active_decisions WHERE generation=?1 AND admission='deferred' AND next_retry_at<=?2) AND NOT EXISTS(SELECT 1 FROM provider_active_destinations WHERE generation=?1 AND admitted=1 AND EXISTS(SELECT 1 FROM provider_active_decisions d WHERE d.generation=provider_active_destinations.generation AND d.pass_key=provider_active_destinations.pass_key AND d.child=provider_active_destinations.child AND d.next_retry_at<=?2) AND ((verified_media=0 AND preserved_local IS NULL) OR (?3=1 AND verified_metadata=0 AND metadata_successor IS NULL)))",params![generation,Utc::now().timestamp(),root.spec.metadata_enabled],|r|r.get(0))?)
        }).await
    }

    pub(crate) async fn selection_decision(
        &self,
        owner: account::AccountOwner,
        generation: String,
        key: String,
        child: String,
    ) -> Result<Option<ActiveDecision>, StateError> {
        self.with_conn("replaying current selection decision",move|conn|{
            account::validate_authenticated(conn,&owner)?;let root=load_generation(conn,&generation)?;
            let exists:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM provider_active_decisions WHERE generation=?1 AND pass_key=?2 AND child=?3)",params![generation,key,child],|r|r.get(0))?;
            if exists {Ok(Some(load_decision(conn,&root,&key,&child)?))} else {Ok(None)}
        }).await
    }

    /// Queue, mapping, grouping, independent debt and consumption commit as one
    /// bounded unit. A selected-but-blocked generation never overwrites old work.
    pub(crate) async fn project_selection_decision(
        &self,
        owner: account::AccountOwner,
        generation: String,
        manifest: ActiveDecision,
        records: Vec<AssetRecord>,
        capacity: u64,
    ) -> Result<GenerationProjection, StateError> {
        self.project_selection_decision_with_proof(
            owner, generation, manifest, records, None, capacity,
        )
        .await
    }

    pub(crate) async fn project_selection_decision_with_proof(
        &self,
        owner: account::AccountOwner,
        generation: String,
        manifest: ActiveDecision,
        records: Vec<AssetRecord>,
        fresh_confirmation: Option<Vec<u8>>,
        capacity: u64,
    ) -> Result<GenerationProjection, StateError> {
        self.project_selection_decision_with_layout_proof(
            owner,
            generation,
            manifest,
            records,
            fresh_confirmation,
            AdmissionChecks {
                inventory_observation: None,
                capacity,
                layouts: Vec::new(),
                archive_paths: Vec::new(),
            },
        )
        .await
    }

    /// The managed owner supplies cancellations only after measuring every
    /// retained media/sidecar fingerprint. Admission rechecks the exact journal
    /// snapshots and current provider proof in the same transaction.
    pub(crate) async fn project_selection_decision_with_layout_proof(
        &self,
        owner: account::AccountOwner,
        generation: String,
        manifest: ActiveDecision,
        records: Vec<AssetRecord>,
        fresh_confirmation: Option<Vec<u8>>,
        checks: AdmissionChecks,
    ) -> Result<GenerationProjection, StateError> {
        let AdmissionChecks {
            inventory_observation,
            capacity,
            layouts,
            archive_paths,
        } = checks;
        self.with_conn_mut("projecting selection destinations",move|conn|{
            let tx=conn.transaction_with_behavior(TransactionBehavior::Immediate)?;account::validate_authenticated(&tx,&owner)?;
            let root=load_generation(&tx,&generation)?;
            let changed_basis=basis(&tx,&root.spec.scope)?!=root.spec.basis;
            validate_decision(&tx,&root,&manifest,true)?;validate_records(&root,&manifest,&records)?;
            let d=&manifest.decision;let bytes=encode(&manifest)?;let hash=digest(&bytes);
            let existing:Option<(Vec<u8>,String)>=tx.query_row("SELECT manifest,admission FROM provider_active_decisions WHERE generation=?1 AND pass_key=?2 AND child=?3",params![generation,d.pass_key,d.child],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
            let mut share_catalog_entry = false;
            if root.spec.is_accepted_replay() && d.outcome == SelectionOutcome::Selected {
                if !accepted_replay_eligibility(&tx, &root, &manifest, fresh_confirmation.as_deref())? {
                    return Ok(GenerationProjection { admitted: false, reason: "accepted_replay_identity_unresolved".to_owned() });
                }
                if existing.as_ref().is_none_or(|(_, admission)| admission != "admitted") {
                    let present: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM assets WHERE library=?1 AND id IN (?2,?3))",params![manifest.media_zone(&root).get("zoneName").and_then(Value::as_str),manifest.state_id,d.child],|row|row.get(0))?;
                    if present {
                        share_catalog_entry = can_share_accepted_album_asset(&tx, &root, &manifest, &records)?;
                        if !share_catalog_entry {
                            return Ok(GenerationProjection { admitted: false, reason: "accepted_replay_existing_asset".to_owned() });
                        }
                    }
                }
            }
            let mut resolving=false;
            if let Some((previous,admission))=&existing {
                let old=load_decision(&tx,&root,&d.pass_key,&d.child)?;
                resolving=admission=="deferred" && old.decision.outcome==SelectionOutcome::Deferred && old.decision.confirmation.is_none() && manifest.decision.confirmation.is_some() && !root.sealed && old.sources.iter().all(|source|manifest.sources.contains(source));
                if previous!=&bytes && !resolving {return Err(StateError::ProviderWorkConflict("frozen_manifest_changed"))}
                if resolving {
                    if old.decision.media_source != manifest.decision.media_source {
                        return Err(StateError::ProviderWorkConflict("media_source_changed"));
                    }
                    // Across a changed basis the retained pair is all that
                    // identifies the old rendition set. Never resolve it with
                    // a lookup that would silently drop an aliased edit. No
                    // path was admitted, so the filename stem can change.
                    let agrees = if changed_basis {
                        crate::icloud::photos::PhotoAsset::same_unplanned_download
                    } else {
                        crate::icloud::photos::same_selected_facts
                    };
                    let observed=observed_asset_matching(&tx,&root,&old,agrees)?;
                    let current=crate::icloud::photos::current_asset(manifest.decision.confirmation.as_deref().ok_or(StateError::ProviderSelectionInvalid)?,&d.child,d.master.as_deref().ok_or(StateError::ProviderSelectionInvalid)?,manifest.media_zone(&root)).map_err(|_invalid|StateError::ProviderSelectionInvalid)?;
                    let mut membership_only = observed.is_none() && !changed_basis && old.decision.media_source.is_some();
                    if membership_only {
                        let mut proofs = RankProofs::new(&tx, &root);
                        for source in &old.sources {
                            membership_only &= proofs.is_membership_source(source, &d.pass_key, &d.child)?;
                        }
                        membership_only &= match fresh_confirmation.as_deref().filter(|body| body.len() <= MAX_CHUNK_BYTES) {
                            Some(body) => {
                                let fresh = crate::icloud::photos::current_asset(body, &d.child, d.master.as_deref().ok_or(StateError::ProviderSelectionInvalid)?, manifest.media_zone(&root)).map_err(|_invalid| StateError::ProviderSelectionInvalid)?;
                                crate::icloud::photos::same_selected_facts(&fresh, &current)
                            }
                            None => false,
                        };
                    }
                    if !membership_only && observed.as_ref().is_none_or(|observed|!agrees(observed,&current)) {
                        // The accepted replay gate above already matched every
                        // retained child, including sources in the old decision.
                        let recovered = !changed_basis && d.outcome == SelectionOutcome::Selected
                            && (root.spec.is_accepted_replay()
                                || (fresh_confirmation.as_deref() == d.confirmation.as_deref()
                                    && inventory_observation.as_ref().is_some_and(|rank|crate::icloud::photos::same_selected_facts(rank,&current))
                                    && first_inventory_identity_matches(&tx,&root,&old,&current)?));
                        if !recovered {
                            defer_attempt(&tx,&root,&d.pass_key,&d.child)?;tx.commit()?;
                            return Ok(GenerationProjection {admitted:false,reason:"original_selected_facts_unresolved".to_owned()});
                        }
                    }
                }
                if changed_basis && !resolving && admission=="deferred" && old.decision.confirmation.is_some() {
                    let Some(fresh)=fresh_confirmation.as_deref().filter(|body|body.len()<=MAX_CHUNK_BYTES) else {return Err(StateError::ProviderWorkConflict("fresh_confirmation_unavailable"))};
                    let master=old.decision.master.as_deref().ok_or(StateError::ProviderSelectionInvalid)?;
                    let previous=crate::icloud::photos::current_asset(old.decision.confirmation.as_deref().ok_or(StateError::ProviderSelectionInvalid)?,&d.child,master,old.media_zone(&root)).map_err(|_invalid|StateError::ProviderSelectionInvalid)?;
                    let current=crate::icloud::photos::current_asset(fresh,&d.child,master,manifest.media_zone(&root)).map_err(|_invalid|StateError::ProviderSelectionInvalid)?;
                    if !crate::icloud::photos::same_selected_facts(&previous,&current) {return Err(StateError::ProviderWorkConflict("confirmed_facts_changed"))}
                }
                if admission!="deferred" {tx.commit()?;return Ok(GenerationProjection {admitted:admission=="admitted",reason:String::new()})}
            } else if root.sealed {return invalid()} else if changed_basis {return Err(StateError::ProviderWorkConflict("selection_basis_changed"))}
            // This check and admission share the immediate transaction. Two
            // producers cannot freeze one physical path for different albums or
            // smart folders. Every deferred retry rechecks ownership, including
            // confirmed selected declarations whose original blocker cleared.
            // Already admitted replay returned above; conflicts leave frozen
            // declarations unchanged through transaction rollback.
            if root.spec.profile.get(INDEPENDENT_ALBUM_COPIES)==Some(&Value::Bool(true)) && (root.spec.mixed_favorites_replay() || matches!(pass(&root,&d.pass_key)?.get("kind").and_then(Value::as_str), Some("Album" | "SmartFolder"))) {
                for destination in &d.destinations {
                    if album_path_claimed(&tx, &root.spec.scope, &d.pass_key, &destination.path)? {
                        return Err(StateError::ProviderWorkConflict("album_path_claimed"));
                    }
                }
            }
            let mut reason=d.reason.clone();
            if ambiguous_primary_archive_path(&tx, &records, &archive_paths)? {
                reason = "ambiguous_primary_archive_path".to_owned();
            }
            let mut staged = false;
            if existing.is_none() && !layouts.is_empty() && d.outcome == SelectionOutcome::Selected && d.reason.is_empty() {
                let fresh = fresh_confirmation.as_deref().ok_or(StateError::ProviderWorkConflict("fresh_confirmation_unavailable"))?;
                let master = d.master.as_deref().ok_or(StateError::ProviderWorkConflict("master_unconfirmed"))?;
                let current = crate::icloud::photos::current_asset(fresh, &d.child, master, manifest.media_zone(&root)).map_err(|_invalid| StateError::ProviderSelectionInvalid)?;
                let selected = crate::icloud::photos::current_asset(d.confirmation.as_deref().ok_or(StateError::ProviderSelectionInvalid)?, &d.child, master, manifest.media_zone(&root)).map_err(|_invalid| StateError::ProviderSelectionInvalid)?;
                if !crate::icloud::photos::same_selected_facts(&current, &selected) { return Err(StateError::ProviderWorkConflict("confirmed_facts_changed")); }
                let predecessors = managed_predecessors(&tx, &root, &manifest, &layouts)?;
                if !predecessors.is_empty() {
                    insert_selection_rows(&tx, &root, &manifest, ("admitted", ""), capacity)?;
                    refresh_admission_hash(&tx, &root, &d.pass_key, &d.child)?;
                    for (old_root, old, operation) in predecessors {
                        let prior_reason: String = tx.query_row("SELECT reason FROM provider_active_decisions WHERE generation=?1 AND pass_key=?2 AND child=?3", params![old_root.id,old.decision.pass_key,old.decision.child], |row| row.get(0))?;
                        let proof = ManagedSuccessor { generation: generation.clone(), manifest_hash: hash.clone(), operation: operation.operation.clone(), operation_hash: digest(&encode(&operation)?), prior_reason };
                        let reason = format!("{MANAGED_SUCCESSOR_PREFIX}{}", String::from_utf8(encode(&proof)?).map_err(|_invalid| StateError::ProviderSelectionInvalid)?);
                        check_write_charge(u64::try_from(reason.len()).map_err(|_invalid| StateError::ProviderSelectionFull)?, capacity)?;
                        tx.execute("UPDATE provider_active_decisions SET admission='excluded',reason=?4,charged_bytes=charged_bytes+?5 WHERE generation=?1 AND pass_key=?2 AND child=?3", params![old_root.id,old.decision.pass_key,old.decision.child,reason,i64::try_from(reason.len()).map_err(|_invalid| StateError::ProviderSelectionFull)?])?;
                        tx.execute("UPDATE provider_active_destinations SET admitted=0 WHERE generation=?1 AND pass_key=?2 AND child=?3", params![old_root.id,old.decision.pass_key,old.decision.child])?;
                        refresh_admission_hash(&tx, &old_root, &old.decision.pass_key, &old.decision.child)?;
                        validate_decision_rows(&tx, &old_root, &old, confirmed_decision_asset(&old, &old_root)?.as_ref())?;
                    }
                    staged = true;
                }
            }
            if d.outcome==SelectionOutcome::Selected {
                // Historical master ambiguity protects its actual catalog identity,
                // not independently authenticated children sharing that master.
                let blocked:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM unattributed_legacy WHERE library=?1 AND asset_id=?2)",params![manifest.media_zone(&root).get("zoneName").and_then(Value::as_str),manifest.state_id],|r|r.get(0))?;
                if blocked {reason="identity_conflict".to_owned()}
                for record in &records {
                    match super::provider_work::guard_projected_generation(&tx,record) {
                        Ok(())=>{},Err(StateError::ProviderWorkConflict(_))=>{reason="unfinished_generation_conflict".to_owned();break},Err(error)=>return Err(error),
                    }
                }
                if let Some(master)=&d.master {
                    match super::provider_work::guard_projected_mapping(&tx,manifest.media_zone(&root).get("zoneName").and_then(Value::as_str).ok_or(StateError::ProviderSelectionInvalid)?,&d.child,master) {
                        Ok(())=>{},Err(StateError::ProviderWorkConflict(_))=>reason="unfinished_mapping_conflict".to_owned(),Err(error)=>return Err(error),
                    }
                }
            }
            let selected_pass=pass(&root,&d.pass_key)?;
            // Unfiled labels render paths but never establish album membership.
            let album=selected_pass.pointer("/scope/name").and_then(Value::as_str).filter(|name|!name.is_empty() && selected_pass.get("kind").and_then(Value::as_str)!=Some("Unfiled"));
            if d.outcome==SelectionOutcome::Selected
                && let Some(album)=album {
                let conflict:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM provider_active_destinations w JOIN provider_active_generations g ON g.id=w.generation WHERE w.library=?1 AND w.asset_id=?2 AND w.admitted=1 AND g.metadata_enabled=1 AND w.verified_metadata=0 AND w.prepared_hash IS NOT NULL) AND NOT EXISTS(SELECT 1 FROM asset_albums WHERE library=?1 AND asset_id=?2 AND album_name=?3)",params![manifest.media_zone(&root).get("zoneName").and_then(Value::as_str),manifest.state_id,album],|r|r.get(0))?;
                if conflict {reason="unfinished_grouping_conflict".to_owned()}
            }
            let admitted=d.outcome==SelectionOutcome::Selected && reason.is_empty();
            if staged && !admitted { return Err(StateError::ProviderWorkConflict("layout_successor_deferred")); }
            let admission=if admitted {"admitted"} else if d.outcome==SelectionOutcome::Excluded {"excluded"} else {"deferred"};
            if !staged && (existing.is_none() || resolving) {
                insert_selection_rows(&tx, &root, &manifest, (admission, &reason), capacity)?;
            }
            refresh_admission_hash(&tx,&root,&d.pass_key,&d.child)?;
            if admitted {
                let library=manifest.media_zone(&root).get("zoneName").and_then(Value::as_str).ok_or(StateError::ProviderSelectionInvalid)?;
                if !share_catalog_entry {
                    let now=Utc::now().timestamp();
                    for record in &records {
                        let drift:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM assets WHERE library=?1 AND id=?2 AND version_size=?3 AND metadata_hash IS NOT ?4)",params![record.library,record.id,record.version_size.as_str(),record.metadata.metadata_hash],|r|r.get(0))?;
                        upsert_asset_row(&tx,record,now)?;
                        if drift && root.spec.metadata_enabled {
                            tx.execute("UPDATE assets SET metadata_write_failed_at=COALESCE(metadata_write_failed_at,?4) WHERE library=?1 AND id=?2 AND version_size=?3",params![record.library,record.id,record.version_size.as_str(),now])?;
                            tx.execute("UPDATE asset_metadata_paths SET metadata_write_failed_at=COALESCE(metadata_write_failed_at,?4) WHERE library=?1 AND id=?2 AND version_size=?3",params![record.library,record.id,record.version_size.as_str(),now])?;
                        }
                    }
                    tx.execute("INSERT INTO asset_master_mappings(library,asset_record_name,master_record_name,updated_at) VALUES(?1,?2,?3,?4) ON CONFLICT(library,asset_record_name) DO UPDATE SET master_record_name=excluded.master_record_name,updated_at=excluded.updated_at",params![library,d.child,d.master,now])?;
                }
                let inserted_album = if let Some(album)=album {
                    tx.execute("INSERT OR IGNORE INTO asset_albums(library,asset_id,album_name,source) VALUES(?1,?2,?3,'icloud')",params![library,manifest.state_id,album])? > 0
                } else {false};
                refresh_asset_album_groupings_tx(&tx,library,&manifest.state_id,None)?;
                if inserted_album { guard_groupings(&tx,library,&manifest.state_id)?; }
                tx.execute("UPDATE provider_active_destinations SET admitted=1 WHERE generation=?1 AND pass_key=?2 AND child=?3",params![generation,d.pass_key,d.child])?;
            }
            let attempts:u32=tx.query_row("SELECT attempts FROM provider_active_decisions WHERE generation=?1 AND pass_key=?2 AND child=?3",params![generation,d.pass_key,d.child],|r|r.get(0))?;
            let delay=(3600_i64*(1_i64<<attempts.min(5))).min(86400);
            // Consumption is the final write, and never means verified publication.
            tx.execute("UPDATE provider_active_decisions SET admission=?4,reason=?5,attempts=?6,next_retry_at=?7 WHERE generation=?1 AND pass_key=?2 AND child=?3",params![generation,d.pass_key,d.child,admission,reason,if admission=="deferred" {attempts.saturating_add(1).min(32)} else {0},if admission=="deferred" {Utc::now().timestamp()+delay} else {0}])?;
            refresh_admission_hash(&tx,&root,&d.pass_key,&d.child)?;
            tx.commit()?;Ok(GenerationProjection {admitted,reason})
        }).await
    }

    pub(crate) async fn seal_selection_generation(
        &self,
        owner: account::AccountOwner,
        generation: String,
        checkpoint_ready: bool,
        veto: Option<String>,
    ) -> Result<(), StateError> {
        self.with_conn_mut("sealing observed selection coverage",move|conn|{
            let tx=conn.transaction_with_behavior(TransactionBehavior::Immediate)?;account::validate_authenticated(&tx,&owner)?;
            let root=load_generation(&tx,&generation)?;
            if root.spec.is_accepted_replay() { return invalid(); }
            validate_current_deleted_selection(&tx,&root.spec)?;
            if basis(&tx,&root.spec.scope)?!=root.spec.basis {return Err(StateError::ProviderWorkConflict("selection_basis_changed"))}
            if !root.sealed {
                let assets:Vec<(String,String)>=tx.prepare("SELECT DISTINCT library,asset_id FROM provider_active_destinations WHERE generation=?1")?.query_map([&generation],|r|Ok((r.get(0)?,r.get(1)?)))?.collect::<Result<_,_>>()?;
                for (library,asset) in assets {
                    let hash=grouping_hash(&tx,&library,&asset)?;
                    // This probe runs once per asset. A generation-only range
                    // scan makes sealing a large unfiled library quadratic.
                    let drift:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM provider_active_destinations INDEXED BY idx_active_destination_identity WHERE generation=?1 AND library=?2 AND asset_id=?3 AND grouping_hash IS NOT NULL AND grouping_hash<>?4)",params![generation,library,asset,hash],|r|r.get(0))?;
                    if drift {return Err(StateError::ProviderWorkConflict("frozen_grouping_changed"))}
                    freeze_asset_intents(&tx,&root,&library,&asset,&hash)?;
                }
            }
            let hash=seal_hash(&tx,&root,checkpoint_ready,veto.as_deref())?;
            let header=seal_header(&root,checkpoint_ready,veto.as_deref(),&hash)?;
            tx.execute("UPDATE provider_active_generations SET sealed=1,checkpoint_ready=?2,checkpoint_veto=?3,seal_hash=?4,seal_header_hash=?5 WHERE id=?1",params![generation,checkpoint_ready,veto,hash,header])?;
            tx.commit()?;Ok(())
        }).await
    }

    pub(crate) async fn pending_selection_decisions(
        &self,
        owner: account::AccountOwner,
        generation: String,
        key: String,
        after: Option<String>,
        limit: u32,
    ) -> Result<Vec<ActiveDecision>, StateError> {
        self.with_conn("reading unfinished selected destinations",move|conn|{
            account::validate_authenticated(conn,&owner)?;let root=load_generation(conn,&generation)?;pass(&root,&key)?;
            let children:Vec<String>=conn.prepare("SELECT d.child FROM provider_active_decisions d WHERE d.generation=?1 AND d.pass_key=?2 AND (?3 IS NULL OR d.child>?3) AND d.next_retry_at<=?4 AND (d.admission='deferred' OR EXISTS(SELECT 1 FROM provider_active_destinations w WHERE w.generation=d.generation AND w.pass_key=d.pass_key AND w.child=d.child AND w.admitted=1 AND ((w.verified_media=0 AND w.preserved_local IS NULL) OR (?5=1 AND w.verified_metadata=0 AND w.metadata_successor IS NULL)))) ORDER BY d.child LIMIT ?6")?.query_map(params![generation,key,after,Utc::now().timestamp(),root.spec.metadata_enabled,limit],|r|r.get(0))?.collect::<Result<_,_>>()?;
            children.iter().map(|child|load_decision(conn,&root,&key,child)).collect()
        }).await
    }

    pub(crate) async fn has_pending_selection(
        &self,
        owner: account::AccountOwner,
        generation: String,
    ) -> Result<bool, StateError> {
        self.with_conn("inspecting destination wake-up",move|conn|{
            account::validate_authenticated(conn,&owner)?;let root=load_generation(conn,&generation)?;
            Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM provider_active_decisions WHERE generation=?1 AND admission='deferred' AND next_retry_at<=?2) OR EXISTS(SELECT 1 FROM provider_active_destinations WHERE generation=?1 AND admitted=1 AND EXISTS(SELECT 1 FROM provider_active_decisions d WHERE d.generation=provider_active_destinations.generation AND d.pass_key=provider_active_destinations.pass_key AND d.child=provider_active_destinations.child AND d.next_retry_at<=?2) AND ((verified_media=0 AND preserved_local IS NULL) OR (?3=1 AND verified_metadata=0 AND metadata_successor IS NULL)))",params![generation,Utc::now().timestamp(),root.spec.metadata_enabled],|r|r.get(0))?)
        }).await
    }
}

pub(crate) struct MembershipLayout {
    pub(crate) native_seed: bool,
    pub(crate) binding: super::primary_layout::LayoutBinding,
    pub(crate) previous: super::primary_layout::LayoutOperation,
    pub(crate) container: String,
    pub(crate) selected: Vec<crate::icloud::photos::PhotoAsset>,
}

fn validate_deleted_selection(
    conn: &Connection,
    spec: &GenerationSpec,
) -> Result<Vec<BoundAlbumSelection>, StateError> {
    let Some(encoded) = spec.profile.get("deleted_album_bindings") else {
        return Ok(Vec::new());
    };
    let bindings: Vec<BoundAlbumSelection> = serde_json::from_value(encoded.clone())
        .map_err(|_invalid| StateError::ProviderSelectionInvalid)?;
    if bindings.is_empty()
        || bindings.len() > 128
        || spec
            .profile
            .get("passes")
            .and_then(Value::as_array)
            .is_none_or(|passes| !passes.is_empty())
    {
        return invalid();
    }
    let mut seen = HashSet::new();
    for binding in &bindings {
        if !binding.deleted || binding.components.is_empty() || !seen.insert(&binding.container_id)
        {
            return invalid();
        }
        // Reject self/cyclic recovery references before loading their headers.
        let ordinary:bool=conn.query_row("SELECT json_array_length(specification,'$.profile.passes')>0 FROM provider_active_generations WHERE id=?1",[&binding.generation],|row|row.get(0))?;
        if !ordinary {
            return invalid();
        }
        let prior = load_generation(conn, &binding.generation)?;
        if !prior.sealed
            || prior.spec.scope != spec.scope
            || prior.spec.zone != spec.zone
            || prior.spec.profile.get("selection_config") != spec.profile.get("selection_config")
            || spec
                .profile
                .get("selection_config")
                .and_then(Value::as_str)
                .is_none()
        {
            return invalid();
        }
        let prior_passes = prior
            .spec
            .profile
            .get("passes")
            .and_then(Value::as_array)
            .ok_or(StateError::ProviderSelectionInvalid)?;
        for pass in prior_passes {
            if pass.get("kind").and_then(Value::as_str) != Some("Album") {
                return invalid();
            }
            let container = pass
                .pointer("/scope/container_id")
                .and_then(Value::as_str)
                .ok_or(StateError::ProviderSelectionInvalid)?;
            if !bindings.iter().any(|bound| {
                bound.container_id == container
                    && pass.get("selector_components") == Some(&json!(bound.components))
            }) {
                return invalid();
            }
        }
        if !prior_passes.iter().any(|pass| {
            pass.pointer("/scope/container_id").and_then(Value::as_str)
                == Some(&binding.container_id)
                && pass.get("selector_components") == Some(&json!(binding.components))
        }) {
            return invalid();
        }
    }
    Ok(bindings)
}

fn validate_current_deleted_selection(
    conn: &Connection,
    spec: &GenerationSpec,
) -> Result<(), StateError> {
    // Mutable deletion status gates execution, not historical header integrity.
    for binding in validate_deleted_selection(conn, spec)? {
        let valid:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM album_containers WHERE library=?1 AND container_id=?2 AND pass_kind='album' AND is_deleted=1) AND EXISTS(SELECT 1 FROM provider_active_destinations w JOIN provider_active_generations g ON g.id=w.generation JOIN json_each(CAST(g.specification AS TEXT),'$.profile.passes') p ON json_extract(p.value,'$.key')=w.pass_key WHERE w.generation=?3 AND json_extract(p.value,'$.scope.container_id')=?2 AND w.admitted=1 AND w.verified_media=1)",params![spec.zone.get("zoneName").and_then(Value::as_str),binding.container_id,binding.generation],|row|row.get(0))?;
        if !valid {
            return invalid();
        }
    }
    Ok(())
}

/// Frozen source authority for an actual directory publication. Settlement is
/// recorded in its existing member journal, never inferred from path receipts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DirectoryPublicationSource {
    generation: String,
    pass: String,
    child: String,
    source: String,
    settled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    embedded: Option<Box<crate::state::types::EmbeddedMetadataReceipt>>,
}

pub(super) fn land_directory_publication(
    conn: &Connection,
    file: &super::primary_layout::directory::AlbumDirectoryFile,
) -> Result<(), StateError> {
    let Some(native) = &file.native else {
        return Ok(());
    };
    if conn.is_autocommit() || native.settled {
        return invalid();
    }
    let fingerprint = file
        .fingerprint
        .as_ref()
        .ok_or(StateError::ProviderSelectionInvalid)?;
    let root = load_generation(conn, &native.generation)?;
    load_decision(conn, &root, &native.pass, &native.child)?;
    let changed = conn.execute("UPDATE provider_active_destinations SET current_publication=0 WHERE generation=?1 AND pass_key=?2 AND child=?3 AND library=?4 AND asset_id=?5 AND version_size=?6 AND path=?7 AND checksum=?8 AND local_checksum=?9 AND source_checksum=?10 AND current_publication=1 AND verified_media=1 AND preserved_local IS NULL AND prepared_hash IS NULL", params![native.generation,native.pass,native.child,file.library,file.child,file.version,file.old.key()?,file.provider_checksum,data_encoding::HEXLOWER.encode(&fingerprint.sha256),native.source])?;
    if changed != 1 {
        return invalid();
    }
    Ok(())
}

// These are actual moved publications awaiting native receipt finalization.
// Only the current container binding may hand them to a current destination.
fn pending_directory_publications(
    conn: &Connection,
    filter: Option<(&str, &super::provider_selection::SelectionPath)>,
) -> Result<
    Vec<(
        String,
        i64,
        super::primary_layout::directory::AlbumDirectoryFile,
    )>,
    StateError,
> {
    let mut statement = conn.prepare("SELECT o.operation,m.member,m.evidence FROM primary_layout_operations o JOIN primary_layout_members m ON m.operation=o.operation WHERE o.policy='album-directory-v1' AND o.phase='committed' AND json_extract(CAST(m.evidence AS TEXT),'$.native.settled')=0")?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, Vec<u8>>(2)?,
        ))
    })?;
    let mut pending = Vec::new();
    for row in rows {
        let (operation, member, bytes) = row?;
        let file: super::primary_layout::directory::AlbumDirectoryFile =
            super::primary_layout::decode(&bytes)?;
        if filter.is_some_and(|(library, path)| file.library != library || file.next != *path) {
            continue;
        }
        let (op, _) = super::primary_layout::directory::load_operation(conn, &operation)?;
        if op.fallback.is_some() {
            continue;
        }
        let current: Option<Vec<u8>> = conn.query_row("SELECT directory_binding FROM album_containers WHERE library=?1 AND container_id=?2", params![op.next.library,op.next.container], |row| row.get(0)).optional()?.flatten();
        if current.as_deref() != Some(super::primary_layout::encode(&op.next)?.as_slice()) {
            continue;
        }
        pending.push((operation, member, file));
    }
    Ok(pending)
}

fn settle_directory_publications(
    conn: &Connection,
    library: &str,
    path: &super::provider_selection::SelectionPath,
) -> Result<(), StateError> {
    for (operation, member, mut file) in
        pending_directory_publications(conn, Some((library, path)))?
    {
        let native = file
            .native
            .as_mut()
            .ok_or(StateError::ProviderSelectionInvalid)?;
        native.settled = true;
        conn.execute(
            "UPDATE primary_layout_members SET evidence=?3 WHERE operation=?1 AND member=?2",
            params![operation, member, super::primary_layout::encode(&file)?],
        )?;
    }
    Ok(())
}

pub(super) fn has_unsettled_directory_publication(
    conn: &Connection,
    binding: &super::primary_layout::directory::AlbumDirectoryBinding,
) -> Result<bool, StateError> {
    for (operation, _, _) in pending_directory_publications(conn, None)? {
        let (op, _) = super::primary_layout::directory::load_operation(conn, &operation)?;
        if op.next.library == binding.library
            && op.next.container == binding.container
            && op.next.generation == binding.generation
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn directory_publication_source(
    conn: &Connection,
    scope: &str,
    key: &str,
    child: &str,
    library: &str,
    path: &super::provider_selection::SelectionPath,
    fingerprint: (&str, u64),
) -> Result<Option<(SelectionDestination, DirectoryPublicationSource)>, StateError> {
    let mut source = None;
    for (operation, _, file) in pending_directory_publications(conn, Some((library, path)))? {
        let native = file
            .native
            .as_ref()
            .ok_or(StateError::ProviderSelectionInvalid)?;
        let observed = file
            .fingerprint
            .as_ref()
            .ok_or(StateError::ProviderSelectionInvalid)?;
        if native.child != child
            || observed.size != fingerprint.1
            || data_encoding::HEXLOWER.encode(&observed.sha256) != fingerprint.0
        {
            continue;
        }
        let (op, _) = super::primary_layout::directory::load_operation(conn, &operation)?;
        if op.next.pass != key {
            continue;
        }
        let root = load_generation(conn, &native.generation)?;
        if root.spec.scope != scope {
            continue;
        }
        let manifest = load_decision(conn, &root, &native.pass, &native.child)?;
        let destination = manifest
            .decision
            .destinations
            .iter()
            .find(|destination| {
                destination.path == file.old
                    && destination.version_size == file.version
                    && destination.checksum == file.provider_checksum
            })
            .ok_or(StateError::ProviderSelectionInvalid)?;
        let valid: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM provider_active_destinations WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5 AND verified_media=1 AND preserved_local IS NULL AND local_checksum=?6 AND source_checksum=?7)", params![native.generation,native.pass,native.child,file.version,file.old.key()?,fingerprint.0,native.source], |row| row.get(0))?;
        if !valid || source.is_some() {
            return invalid();
        }
        source = Some((destination.clone(), native.clone()));
    }
    Ok(source)
}

/// Exact native publication evidence, not a legacy asset or path row.
pub(super) fn album_directory_native_sources(
    conn: &Connection,
    pass_key: Option<&str>,
    prefix: &Path,
) -> Result<Vec<super::primary_layout::directory::AlbumDirectorySource>, StateError> {
    let mut statement = conn.prepare("SELECT generation,pass_key,child,version_size,path,local_checksum,source_checksum,embedded_metadata_receipt FROM provider_active_destinations WHERE current_publication=1 AND verified_media=1 AND preserved_local IS NULL AND (?1 IS NULL OR pass_key=?1)")?;
    let rows = statement.query_map([pass_key], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, String>(6)?,
            row.get::<_, Option<String>>(7)?,
        ))
    })?;
    let mut sources = Vec::new();
    for row in rows {
        let (generation, key, child, version, path, local_checksum, source, embedded) = row?;
        let embedded = embedded
            .map(|raw| {
                serde_json::from_str::<crate::state::types::EmbeddedMetadataReceipt>(&raw)
                    .map_err(|_invalid| StateError::ProviderSelectionInvalid)
            })
            .transpose()?
            .and_then(|mut receipt| {
                if !receipt.published
                    && receipt.input_checksum == local_checksum
                    && receipt.output_checksum != local_checksum
                {
                    receipt.written.clone_from(&receipt.previous);
                    receipt.output_checksum.clone_from(&local_checksum);
                }
                receipt.published = true;
                (receipt.output_checksum == local_checksum).then_some(receipt)
            })
            .map(Box::new);
        let path: super::provider_selection::SelectionPath =
            serde_json::from_str(&path).map_err(|_error| StateError::ProviderSelectionInvalid)?;
        if !path.to_path().starts_with(prefix) {
            continue;
        }
        let managed: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM primary_layout_claims WHERE path_key=?1)",
            [super::primary_layout::path_key(&path.to_path())?],
            |row| row.get(0),
        )?;
        if managed {
            continue;
        }
        let root = load_generation(conn, &generation)?;
        let manifest = load_decision(conn, &root, &key, &child)?;
        let destination = manifest
            .decision
            .destinations
            .iter()
            .find(|destination| destination.path == path && destination.version_size == version)
            .ok_or(StateError::ProviderSelectionInvalid)?;
        let zone = manifest.media_zone(&root);
        let library = zone
            .get("zoneName")
            .and_then(Value::as_str)
            .ok_or(StateError::ProviderSelectionInvalid)?
            .to_owned();
        sources.push(super::primary_layout::directory::AlbumDirectorySource {
            library,
            child: manifest.state_id.clone(),
            version,
            provider_checksum: destination.checksum.clone(),
            path,
            local_checksum,
            expected_sidecar: None,
            created: None,
            native: Some(DirectoryPublicationSource {
                embedded,
                generation,
                pass: key,
                child,
                source,
                settled: false,
            }),
        });
    }
    Ok(sources)
}

/// Exact native publication evidence, not a legacy asset or path row.
pub(crate) struct MembershipDestination {
    pub(crate) generation: String,
    pub(crate) pass: String,
    pub(crate) child: String,
    pub(crate) state_id: String,
    pub(crate) library: String,
    pub(crate) container: String,
    pub(crate) destination: SelectionDestination,
    pub(crate) local: String,
    pub(crate) source: String,
    pub(crate) selected: Vec<crate::icloud::photos::PhotoAsset>,
}

pub(crate) type MembershipCursor = (String, String, String, String, String);

pub(super) fn validate_membership_seed(
    conn: &Connection,
    op: &super::primary_layout::LayoutOperation,
    binding: &super::primary_layout::LayoutBinding,
) -> Result<(), StateError> {
    let file = binding
        .files
        .first()
        .ok_or(StateError::ProviderSelectionInvalid)?;
    if !op.is_membership_retirement()
        || !op.family.starts_with("native-membership:")
        || binding.files.len() != 1
        || op.generation != 1
        || binding.generation != 1
        || binding.family != op.family
        || binding.library != op.library
        || binding.child != op.child
        || binding.source_library != op.source_library
        || binding.decision != op.decision
        || binding.qualifier != op.qualifier
        || file.sidecar.is_some()
        || file.local_preserved
    {
        return invalid();
    }
    let root = load_generation(conn, &op.qualifier)?;
    let manifest = load_decision(conn, &root, &op.pass, &op.asset_record_name)?;
    let current = load_generation(
        conn,
        op.decision
            .strip_prefix(super::primary_layout::MEMBERSHIP_RETIREMENT_PREFIX)
            .ok_or(StateError::ProviderSelectionInvalid)?,
    )?;
    validate_current_deleted_selection(conn, &current.spec)?;
    if !current.sealed
        || !current.checkpoint_ready
        || basis(conn, &current.spec.scope)? != current.spec.basis
        || current.spec.scope != root.spec.scope
        || current.spec.profile.get("selection_config") != root.spec.profile.get("selection_config")
    {
        return invalid();
    }
    if manifest.state_id != op.child
        || manifest
            .media_zone(&root)
            .get("zoneName")
            .and_then(Value::as_str)
            != Some(&op.library)
        || root.spec.zone.get("zoneName").and_then(Value::as_str) != Some(&op.source_library)
    {
        return invalid();
    }
    let destination = manifest
        .decision
        .destinations
        .iter()
        .find(|d| {
            d.path == file.path
                && d.version_size == file.version
                && d.checksum == file.provider_checksum
        })
        .ok_or(StateError::ProviderSelectionInvalid)?;
    if native_membership_contested(conn, &root, &manifest, destination)? {
        return invalid();
    }
    let current_work:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM provider_active_decisions WHERE generation=?1 AND (admission='deferred' OR (pass_key=?2 AND child=?3))) OR EXISTS(SELECT 1 FROM provider_active_destinations WHERE generation=?1 AND admitted=1 AND (path=?4 OR (verified_media=0 AND preserved_local IS NULL) OR (?5=1 AND verified_metadata=0)))",params![current.id,op.pass,op.asset_record_name,destination.path.key()?,current.spec.metadata_enabled],|r|r.get(0))?;
    if current_work {
        return invalid();
    }
    let valid:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM provider_active_destinations WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5 AND admitted=1 AND verified_media=1 AND current_publication=1 AND preserved_local IS NULL AND (?13=0 OR verified_metadata=1) AND prepared_hash IS NULL AND local_checksum=?6 AND source_checksum=?7) AND NOT EXISTS(SELECT 1 FROM primary_layout_claims WHERE path_key=?8) AND NOT EXISTS(SELECT 1 FROM primary_layout_superseded_paths WHERE library=?9 AND child=?10 AND version=?4 AND provider_checksum=?11 AND native_path=?12)",params![root.id,op.pass,op.asset_record_name,file.version,destination.path.key()?,data_encoding::HEXLOWER.encode(&file.fingerprint.sha256),file.source_checksum,super::primary_layout::path_key(&file.path.to_path())?,op.library,op.child,file.provider_checksum,encode(&file.path)?,root.spec.metadata_enabled],|r|r.get(0))?;
    if !valid {
        return invalid();
    }
    Ok(())
}

/// Called within the layout retirement/detach transaction. Historical proof stays
/// intact; a newer or preserved publication never loses its authority here.
pub(super) fn retire_membership_publication(
    conn: &Connection,
    op: &super::primary_layout::LayoutOperation,
    file: &super::primary_layout::LayoutFile,
) -> Result<(), StateError> {
    if op.is_membership_retirement()
        && op.family.starts_with("native-membership:")
        && !file.local_preserved
    {
        conn.execute("UPDATE provider_active_destinations SET current_publication=0 WHERE generation=?1 AND pass_key=?2 AND child=?3 AND library=?4 AND asset_id=?5 AND version_size=?6 AND path=?7 AND checksum=?8 AND local_checksum=?9 AND source_checksum IS ?10 AND current_publication=1 AND verified_media=1 AND preserved_local IS NULL",params![op.qualifier,op.pass,op.asset_record_name,op.library,op.child,file.version,file.path.key()?,file.provider_checksum,data_encoding::HEXLOWER.encode(&file.fingerprint.sha256),file.source_checksum])?;
    }
    Ok(())
}

fn native_membership_contested(
    conn: &Connection,
    root: &ActiveGeneration,
    manifest: &ActiveDecision,
    destination: &SelectionDestination,
) -> Result<bool, StateError> {
    // Other passes and sources defeat exclusive ownership, even for equal bytes.
    Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM provider_active_destinations w WHERE w.path=?1 AND (w.pass_key<>?2 OR w.child<>?3 OR w.library<>?4 OR w.preserved_local IS NOT NULL OR (w.admitted=1 AND w.verified_media=0) OR (w.current_publication=1 AND w.generation<>?5)))",params![destination.path.key()?,manifest.decision.pass_key,manifest.decision.child,manifest.media_zone(root).get("zoneName").and_then(Value::as_str),root.id],|r|r.get(0))?)
}

impl SqliteStateDb {
    /// Durable tombstones are refresh hints; retirement still checks exact receipts.
    pub(crate) async fn pending_deleted_membership_containers(
        &self,
        owner: account::AccountOwner,
        scope: String,
        selection_config: String,
    ) -> Result<Vec<String>, StateError> {
        self.with_conn("reading pending deleted album copies",move |conn| {
            account::validate_authenticated(conn,&owner)?;
            Ok(conn.prepare("SELECT DISTINCT c.container_id FROM provider_active_generations g JOIN provider_active_destinations w ON w.generation=g.id JOIN json_each(CAST(g.specification AS TEXT),'$.profile.passes') p ON json_extract(p.value,'$.key')=w.pass_key JOIN album_containers c ON c.library=json_extract(g.specification,'$.zone.zoneName') AND c.container_id=json_extract(p.value,'$.scope.container_id') AND c.pass_kind='album' WHERE g.scope=?1 AND json_extract(g.specification,'$.profile.selection_config')=?2 AND c.is_deleted=1 AND w.admitted=1 AND w.verified_media=1 AND w.current_publication=1 AND w.preserved_local IS NULL AND NOT EXISTS(SELECT 1 FROM primary_layout_superseded_paths s WHERE s.library=w.library AND s.child=w.asset_id AND s.version=w.version_size AND s.provider_checksum=w.checksum AND CAST(s.native_path AS TEXT)=w.path)")?.query_map(params![scope,selection_config],|row|row.get(0))?.collect::<Result<_,_>>()?)
        }).await
    }

    pub(crate) async fn membership_destinations(
        &self,
        owner: account::AccountOwner,
        generation: String,
        after: Option<MembershipCursor>,
    ) -> Result<(Vec<MembershipDestination>, Option<MembershipCursor>), StateError> {
        self.with_conn("reading native membership publications", move |conn| {
            account::validate_authenticated(conn, &owner)?;
            let root = load_generation(conn, &generation)?;
            let Some(config) = root.spec.profile.get("selection_config").and_then(Value::as_str) else { return Ok((Vec::new(),None)) };
            let after=after.unwrap_or_default();
            let rows: Vec<MembershipCursor> = conn.prepare("SELECT w.generation,w.pass_key,w.child,w.version_size,w.path FROM provider_active_destinations w JOIN provider_active_generations g ON g.id=w.generation WHERE g.scope=?1 AND w.generation<>?2 AND w.admitted=1 AND w.verified_media=1 AND w.current_publication=1 AND w.preserved_local IS NULL AND (g.metadata_enabled=0 OR w.verified_metadata=1) AND w.prepared_hash IS NULL AND (w.generation,w.pass_key,w.child,w.version_size,w.path)>(?3,?4,?5,?6,?7) ORDER BY w.generation,w.pass_key,w.child,w.version_size,w.path LIMIT 256")?.query_map(params![root.spec.scope,generation,after.0,after.1,after.2,after.3,after.4], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?.collect::<Result<_,_>>()?;
            let next=if rows.len()==256 {rows.last().cloned()} else {None};
            let mut result = Vec::new();
            for (prior,key,child,version,path) in rows {
                let old_root = load_generation(conn,&prior)?;
                if old_root.spec.profile.get("selection_config").and_then(Value::as_str)!=Some(config) { continue; }
                let old_pass = pass(&old_root,&key)?;
                if old_pass.get("kind").and_then(Value::as_str)!=Some("Album") { continue; }
                let Some(container)=old_pass.pointer("/scope/container_id").and_then(Value::as_str) else { continue };
                let present:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM provider_active_decisions WHERE generation=?1 AND pass_key=?2 AND child=?3) OR EXISTS(SELECT 1 FROM provider_active_destinations WHERE generation=?1 AND path=?4 AND admitted=1)",params![generation,key,child,path],|r|r.get(0))?;
                if present { continue; }
                let manifest=load_decision(conn,&old_root,&key,&child)?;
                let destination=manifest.decision.destinations.iter().find(|d|d.version_size==version && d.path.key().ok().as_deref()==Some(&path)).ok_or(StateError::ProviderSelectionInvalid)?.clone();
                if native_membership_contested(conn,&old_root,&manifest,&destination)? { continue; }
                let library=manifest.media_zone(&old_root).get("zoneName").and_then(Value::as_str).ok_or(StateError::ProviderSelectionInvalid)?.to_owned();
                let protected:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM primary_layout_claims WHERE path_key=?1) OR EXISTS(SELECT 1 FROM primary_layout_superseded_paths WHERE library=?2 AND child=?3 AND version=?4 AND provider_checksum=?5 AND native_path=?6) OR EXISTS(SELECT 1 FROM assets WHERE library=?2 AND id=?3 AND is_deleted=1)",params![super::primary_layout::path_key(&destination.path.to_path())?,library,manifest.state_id,version,destination.checksum,encode(&destination.path)?],|r|r.get(0))?;
                if protected { continue; }
                let (local,source):(String,String)=conn.query_row("SELECT local_checksum,source_checksum FROM provider_active_destinations WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5 AND verified_media=1 AND current_publication=1 AND preserved_local IS NULL AND prepared_hash IS NULL",params![prior,key,child,version,path],|r|Ok((r.get(0)?,r.get(1)?)))?;
                let keys:Vec<String>=conn.prepare("SELECT pass_key FROM provider_active_decisions WHERE generation=?1 AND child=?2 AND admission='admitted'")?.query_map(params![generation,child],|r|r.get(0))?.collect::<Result<_,_>>()?;
                let mut selected=Vec::new();
                for selected_key in keys {
                    let selected_manifest=load_decision(conn,&root,&selected_key,&child)?;
                    let decision=&selected_manifest.decision;
                    selected.push(crate::icloud::photos::current_asset(decision.confirmation.as_deref().ok_or(StateError::ProviderSelectionInvalid)?,&child,decision.master.as_deref().ok_or(StateError::ProviderSelectionInvalid)?,selected_manifest.media_zone(&root)).map_err(|_invalid|StateError::ProviderSelectionInvalid)?);
                }
                result.push(MembershipDestination {generation:prior,pass:key,child,state_id:manifest.state_id,library,container:container.to_owned(),destination,local,source,selected});
            }
            Ok((result,next))
        }).await
    }

    /// Current destinations must complete before old selected bindings can retire.
    pub(crate) async fn completed_membership_layouts(
        &self,
        owner: account::AccountOwner,
        generation: String,
    ) -> Result<Option<Vec<MembershipLayout>>, StateError> {
        self.with_conn("reading completed membership layouts", move |conn| {
            account::validate_authenticated(conn, &owner)?;
            let root = load_generation(conn, &generation)?;
            validate_current_deleted_selection(conn,&root.spec)?;
            let incomplete: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM provider_active_decisions WHERE generation=?1 AND admission='deferred') OR EXISTS(SELECT 1 FROM provider_active_destinations WHERE generation=?1 AND admitted=1 AND ((verified_media=0 AND preserved_local IS NULL) OR (?2=1 AND verified_metadata=0)))", params![generation, root.spec.metadata_enabled], |row| row.get(0))?;
            if !root.sealed || !root.checkpoint_ready || incomplete || basis(conn, &root.spec.scope)? != root.spec.basis { return Ok(None); }
            let library = root.spec.zone.get("zoneName").and_then(Value::as_str).ok_or(StateError::ProviderSelectionInvalid)?;
            let rows: Vec<(Vec<u8>, Vec<u8>)> = conn.prepare("SELECT b.binding,o.header FROM primary_layout_bindings b JOIN primary_layout_operations o ON o.family=b.family AND ((o.prior_generation+1=b.generation AND o.phase='committed') OR (b.family LIKE 'native-membership:%' AND o.prior_generation=b.generation AND o.phase NOT IN ('committed','cancelled'))) WHERE b.source_library=?1 ORDER BY b.family")?.query_map([library], |row| Ok((row.get(0)?, row.get(1)?)))?.collect::<Result<_, _>>()?;
            let current_passes = root.spec.profile.get("passes").and_then(Value::as_array).ok_or(StateError::ProviderSelectionInvalid)?;
            let mut layouts = Vec::new();
            for (binding, previous) in rows {
                let binding: super::primary_layout::LayoutBinding = super::primary_layout::decode(&binding)?;
                let previous: super::primary_layout::LayoutOperation = super::primary_layout::decode(&previous)?;
                if previous.family != binding.family || previous.child != binding.child || previous.library != binding.library || previous.source_library != binding.source_library
                    || (previous.generation.checked_add(1) != Some(binding.generation) && !(previous.is_membership_retirement() && previous.generation == binding.generation && previous.phase != "committed"))
                    || previous.decision != binding.decision || previous.policy != binding.policy { return invalid(); }
                if binding.files.is_empty() || (previous.is_membership_retirement() && previous.phase == "committed") { continue; }
                if previous.is_membership_retirement() && previous.family.starts_with("native-membership:") {
                    let publication = load_generation(conn, &previous.qualifier)?;
                    if publication.spec.scope != root.spec.scope || publication.spec.profile.get("selection_config") != root.spec.profile.get("selection_config") { return Ok(None); }
                    for file in &binding.files {
                        let authoritative:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM provider_active_destinations WHERE generation=?1 AND pass_key=?2 AND child=?3 AND library=?4 AND asset_id=?5 AND version_size=?6 AND path=?7 AND checksum=?8 AND local_checksum=?9 AND source_checksum IS ?10 AND current_publication=1 AND verified_media=1 AND preserved_local IS NULL)",params![previous.qualifier,previous.pass,previous.asset_record_name,previous.library,previous.child,file.version,file.path.key()?,file.provider_checksum,data_encoding::HEXLOWER.encode(&file.fingerprint.sha256),file.source_checksum],|row|row.get(0))?;
                        if file.local_preserved || !authoritative { return Ok(None); }
                    }
                }
                let current = current_passes.iter().find(|pass| pass.get("key").and_then(Value::as_str) == Some(&previous.pass));
                let selected_pass = if let Some(pass) = current { Some(pass.clone()) }
                else if let Some(config) = root.spec.profile.get("selection_config").and_then(Value::as_str) {
                    let encoded: Option<String> = conn.query_row("SELECT p.value FROM provider_active_generations g,json_each(CAST(g.specification AS TEXT),'$.profile.passes') p WHERE g.scope=?1 AND json_extract(g.specification,'$.profile.selection_config')=?2 AND json_extract(p.value,'$.key')=?3 ORDER BY g.created_at DESC,g.id DESC LIMIT 1", params![root.spec.scope, config, previous.pass], |row| row.get(0)).optional()?;
                    encoded.map(|value| serde_json::from_str::<Value>(&value).map_err(|_invalid|StateError::ProviderSelectionInvalid)).transpose()?
                } else { None };
                let Some(pass) = selected_pass.filter(|pass| pass.get("kind").and_then(Value::as_str) == Some("Album")) else { continue };
                let Some(container) = pass.pointer("/scope/container_id").and_then(Value::as_str) else { continue };
                let still_present: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM provider_active_decisions WHERE generation=?1 AND pass_key=?2 AND child=?3)", params![generation, previous.pass, previous.asset_record_name], |row|row.get(0))?;
                let deleted: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM assets WHERE library=?1 AND id=?2 AND is_deleted=1)", params![binding.library, binding.child], |row|row.get(0))?;
                if !still_present && !deleted {
                    let keys:Vec<String>=conn.prepare("SELECT pass_key FROM provider_active_decisions WHERE generation=?1 AND child=?2 AND admission='admitted'")?.query_map(params![generation,previous.asset_record_name],|row|row.get(0))?.collect::<Result<_,_>>()?;
                    let mut selected=Vec::new();
                    for key in keys {
                        let manifest=load_decision(conn,&root,&key,&previous.asset_record_name)?;
                        let decision=&manifest.decision;
                        selected.push(crate::icloud::photos::current_asset(decision.confirmation.as_deref().ok_or(StateError::ProviderSelectionInvalid)?, &decision.child,decision.master.as_deref().ok_or(StateError::ProviderSelectionInvalid)?,manifest.media_zone(&root)).map_err(|_invalid|StateError::ProviderSelectionInvalid)?);
                    }
                    layouts.push(MembershipLayout { native_seed: false, binding, previous, container: container.to_owned(), selected });
                }
            }
            Ok(Some(layouts))
        }).await
    }
}

fn seal_hash(
    conn: &Connection,
    root: &ActiveGeneration,
    ready: bool,
    veto: Option<&str>,
) -> Result<String, StateError> {
    seal_hash_with_pages(conn, root, ready, veto, |page| {
        load_rank_proof(conn, root, page)?;
        Ok(())
    })
}

fn seal_hash_with_pages(
    conn: &Connection,
    root: &ActiveGeneration,
    ready: bool,
    veto: Option<&str>,
    mut verify_page: impl FnMut(&str) -> Result<(), StateError>,
) -> Result<String, StateError> {
    let mut hash = Sha256::new();
    hash.update(encode(&(&root.spec, ready, veto))?);
    let pages: Vec<String> = conn
        .prepare("SELECT id FROM provider_selection_rank_pages WHERE generation=?1 ORDER BY id")?
        .query_map([&root.id], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    if pages.is_empty() && root.spec.profile.get("deleted_album_bindings").is_none() {
        return invalid();
    }
    for page in pages {
        verify_page(&page)?;
        hash.update(page.as_bytes());
    }
    let keys:Vec<(String,String,String)>=conn.prepare("SELECT pass_key,child,manifest_hash FROM provider_active_decisions WHERE generation=?1 ORDER BY pass_key,child")?.query_map([&root.id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?.collect::<Result<_,_>>()?;
    for (key, child, manifest_hash) in keys {
        load_decision_with_sources(conn, root, &key, &child, false)?;
        hash.update(encode(&(key, child, manifest_hash))?);
    }
    let groups:Vec<(String,String,String,Option<String>)>=conn.prepare("SELECT pass_key,child,path,grouping_hash FROM provider_active_destinations WHERE generation=?1 ORDER BY pass_key,child,path")?.query_map([&root.id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?.collect::<Result<_,_>>()?;
    for group in groups {
        hash.update(encode(&group)?);
    }
    Ok(format!("{:x}", hash.finalize()))
}

/// Hash the same ordered grouping rows consumed by the existing metadata owner.
pub(super) fn grouping_hash(
    conn: &Connection,
    library: &str,
    asset: &str,
) -> Result<String, StateError> {
    Ok(digest(&encode(&grouping_names(conn, library, asset)?)?))
}

pub(crate) fn writer_grouping_hash(
    albums: &[String],
    people: &[String],
) -> Result<String, StateError> {
    Ok(digest(&encode(&[albums, people])?))
}

fn grouping_names(
    conn: &Connection,
    library: &str,
    asset: &str,
) -> Result<Vec<Vec<String>>, StateError> {
    let mut groups = Vec::new();
    for (table, column) in [
        ("asset_albums", "album_name"),
        ("asset_people", "person_name"),
    ] {
        let names: Vec<String> = conn
            .prepare(&format!(
                "SELECT {column} FROM {table} WHERE library=?1 AND asset_id=?2 ORDER BY {column}"
            ))?
            .query_map(params![library, asset], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        groups.push(names);
    }
    Ok(groups)
}

fn freeze_asset_intents(
    conn: &Connection,
    root: &ActiveGeneration,
    library: &str,
    asset: &str,
    group: &str,
) -> Result<(), StateError> {
    let keys:Vec<(String,String)>=conn.prepare("SELECT DISTINCT pass_key,child FROM provider_active_destinations INDEXED BY idx_active_destination_identity WHERE generation=?1 AND library=?2 AND asset_id=?3 ORDER BY pass_key,child")?.query_map(params![root.id,library,asset],|r|Ok((r.get(0)?,r.get(1)?)))?.collect::<Result<_,_>>()?;
    for (key, child) in keys {
        let manifest = load_decision(conn, root, &key, &child)?;
        for destination in &manifest.decision.destinations {
            let previous:Option<String>=conn.query_row("SELECT grouping_hash FROM provider_active_destinations WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5",params![root.id,key,child,destination.version_size,destination.path.key()?],|r|r.get(0))?;
            // Previously frozen intent, including completed historical work,
            // stays immutable. Seal separately refuses changed dependencies.
            if previous.is_some() {
                continue;
            }
            if previous.is_none() {
                let intent = intent_hash(root, &manifest, destination, group)?;
                conn.execute("UPDATE provider_active_destinations SET grouping_hash=?6,intent_hash=?7 WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5",params![root.id,key,child,destination.version_size,destination.path.key()?,group,intent])?;
                refresh_progress_hash(conn, root, &manifest, destination)?;
            }
        }
    }
    Ok(())
}

/// Freeze writer dependencies for previously interrupted obligations before a
/// new selection root can mutate groupings. This is metadata intent only;
/// inventory remains unsealed and cannot authorize source progress or absence.
fn freeze_interrupted_groupings(conn: &Connection, scope: &str) -> Result<(), StateError> {
    loop {
        let rows:Vec<(String,String,String)>=conn.prepare("SELECT DISTINCT w.generation,w.library,w.asset_id FROM provider_active_destinations w JOIN provider_active_generations g ON g.id=w.generation WHERE g.scope=?1 AND g.sealed=0 AND g.metadata_enabled=1 AND w.admitted=1 AND w.verified_metadata=0 AND w.grouping_hash IS NULL ORDER BY w.generation,w.library,w.asset_id LIMIT 64")?.query_map([scope],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?.collect::<Result<_,_>>()?;
        if rows.is_empty() {
            break;
        }
        for (generation, library, asset) in rows {
            let root = load_generation(conn, &generation)?;
            let children:Vec<(String,String)>=conn.prepare("SELECT DISTINCT pass_key,child FROM provider_active_destinations WHERE generation=?1 AND library=?2 AND asset_id=?3")?.query_map(params![generation,library,asset],|r|Ok((r.get(0)?,r.get(1)?)))?.collect::<Result<_,_>>()?;
            for (key, child) in children {
                load_decision(conn, &root, &key, &child)?;
            }
            let hash = grouping_hash(conn, &library, &asset)?;
            freeze_asset_intents(conn, &root, &library, &asset, &hash)?;
        }
    }
    Ok(())
}

pub(super) fn guard_groupings(
    conn: &Connection,
    library: &str,
    asset: &str,
) -> Result<(), StateError> {
    validate_identity_decisions(conn, library, asset)?;
    let current = grouping_hash(conn, library, asset)?;
    let conflict:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM provider_active_destinations w JOIN provider_active_generations g ON g.id=w.generation WHERE w.library=?1 AND w.asset_id=?2 AND w.admitted=1 AND g.metadata_enabled=1 AND w.verified_metadata=0 AND w.prepared_hash IS NOT NULL AND w.grouping_hash IS NOT ?3)",params![library,asset,current],|r|r.get(0))?;
    if conflict {
        Err(StateError::ProviderWorkConflict("frozen_grouping_changed"))
    } else {
        Ok(())
    }
}

/// Validate declaration and normalized destinations before eligibility filters
/// can hide old work. Indexed links avoid decoding every historical raw page
/// for each shared writer mutation; current confirmation/intent remain checked.
fn validate_identity_decisions(
    conn: &Connection,
    library: &str,
    asset: &str,
) -> Result<(), StateError> {
    // Probe both identity indexes. An OR can make SQLite scan the entire
    // library on each admission. UNION validates each decision only once
    // even when its asset and child identities coincide.
    let mut statement = conn.prepare(
        "SELECT generation,pass_key,child FROM provider_active_destinations WHERE library=?1 AND asset_id=?2
         UNION SELECT generation,pass_key,child FROM provider_active_destinations WHERE library=?1 AND child=?2
         ORDER BY generation,pass_key,child",
    )?;
    let mut rows = statement.query(params![library, asset])?;
    while let Some(row) = rows.next()? {
        let generation: String = row.get(0)?;
        let key: String = row.get(1)?;
        let child: String = row.get(2)?;
        let root = load_generation(conn, &generation)?;
        load_decision_with_sources(conn, &root, &key, &child, false)?;
    }
    Ok(())
}

// Retry scheduling and checkpoint debt share current configuration authority.
// This predicate must not narrow admission or physical publication guards.
const CURRENT_SELECTION_CONFIGURATION: &str = "g.scope=?1 AND g.config_hash=?2";

#[derive(Debug)]
pub(crate) struct SelectionInventory {
    pub(crate) config_hash: String,
    pub(crate) generation: String,
}

impl SelectionInventory {
    #[must_use]
    pub(crate) fn into_metadata_update(self) -> (String, String) {
        (
            format!("selection_inventory_v1:{}", self.config_hash),
            self.generation,
        )
    }
}

// Old plans remain available for ownership and publication checks, but not for
// ordinary scheduling. The checkpoint transaction installs this boundary.
const SUPERSEDED_SELECTION: &str = "EXISTS(
    SELECT 1 FROM metadata m JOIN provider_active_generations baseline ON baseline.id=m.value
    WHERE m.key='selection_inventory_v1:'||g.config_hash
      AND baseline.scope=g.scope AND baseline.config_hash=g.config_hash
      AND baseline.sealed=1 AND baseline.checkpoint_ready=1
      AND (baseline.created_at,baseline.id)>(g.created_at,g.id))";

fn commit_observed_destination(
    tx: rusqlite::Transaction<'_>,
    file_still_matches: &impl Fn() -> bool,
) -> Result<(), StateError> {
    if !file_still_matches() {
        return Err(StateError::ProviderWorkConflict("selected_file_changed"));
    }
    Ok(tx.commit()?)
}

// A never-published path is planning evidence, not another required copy, once
// a later complete query with the same selection and source facts has finalized
// this rendition at its current path. Keep every old row and prepared output.
// Callers validate generation/decision proofs before relying on these columns.
const UNFINISHED: &str = "w.admitted=1
    AND ((w.verified_media=0 AND w.preserved_local IS NULL) OR (g.metadata_enabled=1 AND w.verified_metadata=0 AND w.metadata_successor IS NULL))
    AND NOT (w.verified_media=0 AND w.preserved_local IS NULL AND w.prepared_hash IS NULL AND EXISTS (
        SELECT 1 FROM provider_active_destinations current
        JOIN provider_active_generations newer ON newer.id=current.generation
        WHERE newer.scope=g.scope AND newer.config_hash=g.config_hash
          AND newer.created_at>g.created_at AND newer.sealed=1 AND newer.checkpoint_ready=1
          AND current.pass_key=w.pass_key AND current.child=w.child
          AND current.library=w.library AND current.asset_id=w.asset_id AND current.master=w.master
          AND current.version_size=w.version_size AND current.path<>w.path
          AND current.checksum=w.checksum AND current.size_bytes=w.size_bytes
          AND current.metadata_hash=w.metadata_hash
          AND current.created_at IS w.created_at AND current.added_at IS w.added_at
          AND current.admitted=1 AND current.verified_media=1
          AND (newer.metadata_enabled=0 OR current.verified_metadata=1)
    ))";

pub(super) fn guard_generation(conn: &Connection, record: &AssetRecord) -> Result<(), StateError> {
    validate_identity_decisions(conn, &record.library, &record.id)?;
    let pending:bool=conn.query_row(&format!("SELECT EXISTS(SELECT 1 FROM provider_active_destinations w JOIN provider_active_generations g ON g.id=w.generation WHERE w.library=?1 AND w.asset_id=?2 AND w.version_size=?3 AND {UNFINISHED})"),params![record.library,record.id,record.version_size.as_str()],|row|row.get(0))?;
    if !pending {
        return Ok(());
    }
    let hash = record
        .metadata
        .metadata_hash
        .clone()
        .unwrap_or_else(|| record.metadata.compute_hash());
    let conflict:bool=conn.query_row(&format!("SELECT EXISTS(SELECT 1 FROM provider_active_destinations w JOIN provider_active_generations g ON g.id=w.generation WHERE w.library=?1 AND w.asset_id=?2 AND w.version_size=?3 AND {UNFINISHED} AND (w.checksum<>?4 OR w.size_bytes IS NOT ?5 OR w.created_at IS NOT ?6 OR w.added_at IS NOT ?7 OR w.metadata_hash<>?8))"),params![record.library,record.id,record.version_size.as_str(),record.checksum,i64::try_from(record.size_bytes).ok(),encode_asset_date(record.created_at),record.added_at.map(encode_asset_date),hash],|r|r.get(0))?;
    if conflict {
        Err(StateError::ProviderWorkConflict("unfinished_asset_changed"))
    } else {
        Ok(())
    }
}

pub(super) fn guard_mapping(
    conn: &Connection,
    library: &str,
    child: &str,
    master: &str,
) -> Result<(), StateError> {
    validate_identity_decisions(conn, library, child)?;
    let conflict:bool=conn.query_row(&format!("SELECT EXISTS(SELECT 1 FROM provider_active_destinations w JOIN provider_active_generations g ON g.id=w.generation WHERE w.library=?1 AND w.child=?2 AND w.master<>?3 AND {UNFINISHED})"),params![library,child,master],|r|r.get(0))?;
    if conflict {
        Err(StateError::ProviderWorkConflict(
            "unfinished_mapping_changed",
        ))
    } else {
        Ok(())
    }
}

pub(super) fn guard_metadata(
    conn: &Connection,
    library: &str,
    asset: &str,
    version: &str,
    metadata: &crate::state::AssetMetadata,
    created: f64,
    added: Option<f64>,
) -> Result<(), StateError> {
    validate_identity_decisions(conn, library, asset)?;
    let hash = metadata
        .metadata_hash
        .clone()
        .unwrap_or_else(|| metadata.compute_hash());
    let conflict:bool=conn.query_row(&format!("SELECT EXISTS(SELECT 1 FROM provider_active_destinations w JOIN provider_active_generations g ON g.id=w.generation WHERE w.library=?1 AND w.asset_id=?2 AND w.version_size=?3 AND {UNFINISHED} AND (w.metadata_hash<>?4 OR w.created_at IS NOT ?5 OR w.added_at IS NOT ?6))"),params![library,asset,version,hash,created,added],|r|r.get(0))?;
    if conflict {
        Err(StateError::ProviderWorkConflict(
            "unfinished_metadata_changed",
        ))
    } else {
        Ok(())
    }
}

/// Called inside the existing verified-download transaction using the actual
/// native path and SHA-verified source checksum, never a lossy legacy path row.
/// A preservation-backed layout commit proves the selected destination using
/// pinned provider identity, independent local bytes, and prepared metadata.
/// Commit a managed journal's installed output. Binding acknowledgement uses
/// the generation-specific reuse path and does not establish new authority.
pub(super) fn record_layout_destination(
    conn: &Connection,
    record: &AssetRecord,
    file: &super::primary_layout::LayoutFile,
    flags: u16,
) -> Result<(), StateError> {
    record_layout_destination_for_generation(conn, record, file, flags, None)?;
    let path = file.path.key()?;
    let local = data_encoding::HEXLOWER.encode(&file.fingerprint.sha256);
    let current: Option<(String, String, String)> = if file.local_preserved
        || file.source_checksum.is_none()
    {
        None
    } else {
        conn.query_row("SELECT generation,pass_key,child FROM provider_active_destinations WHERE library=?1 AND asset_id=?2 AND version_size=?3 AND path=?4 AND checksum=?5 AND local_checksum=?6 AND source_checksum IS ?7 AND admitted=1 AND verified_media=1 AND preserved_local IS NULL ORDER BY generation,pass_key,child LIMIT 1", params![record.library,record.id,record.version_size.as_str(),path,file.provider_checksum,local,file.source_checksum], |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?
    };
    replace_current_publication(
        conn,
        &record.library,
        &path,
        current.as_ref().map(|(generation, key, child)| {
            (
                generation.as_str(),
                key.as_str(),
                child.as_str(),
                record.version_size.as_str(),
            )
        }),
    )?;
    if let Some((generation, key, child)) = current {
        let fields = serde_json::to_string(&crate::state::types::EmbeddedMetadataReceipt {
            published: true,
            input_checksum: local.clone(),
            output_checksum: local,
            previous: Default::default(),
            written: (*file.embedded_fields).clone(),
        })
        .map_err(|_invalid| StateError::ProviderSelectionInvalid)?;
        conn.execute("UPDATE provider_active_destinations SET embedded_metadata_receipt=?6 WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5 AND current_publication=1",params![generation,key,child,record.version_size.as_str(),path,fields])?;
    }
    Ok(())
}

fn record_layout_destination_for_generation(
    conn: &Connection,
    record: &AssetRecord,
    file: &super::primary_layout::LayoutFile,
    flags: u16,
    selected_generation: Option<&str>,
) -> Result<(), StateError> {
    validate_identity_decisions(conn, &record.library, &record.id)?;
    let path = file.path.key()?;
    let rows:Vec<(String,String,String)>=conn.prepare("SELECT generation,pass_key,child FROM provider_active_destinations WHERE library=?1 AND asset_id=?2 AND version_size=?3 AND path=?4 AND checksum=?5 AND size_bytes=?6 AND metadata_hash IS ?7 AND admitted=1")?.query_map(params![record.library,record.id,record.version_size.as_str(),path,record.checksum.as_ref(),i64::try_from(record.size_bytes).map_err(|_error|StateError::ProviderSelectionInvalid)?,record.metadata.metadata_hash],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?.collect::<Result<_,_>>()?;
    for (generation, key, child) in rows {
        if selected_generation.is_some_and(|selected| selected != generation) {
            continue;
        }
        let root = load_generation(conn, &generation)?;
        if root.spec.metadata_enabled && selected_generation.is_none() {
            // Journal recovery can precede selection collection. Only its
            // managed recipe acknowledgement may complete native writer work.
            continue;
        }
        if root.spec.metadata_flags & flags != root.spec.metadata_flags {
            return Err(StateError::ProviderWorkConflict("managed_recipe_changed"));
        }
        let manifest = load_decision(conn, &root, &key, &child)?;
        let destination = manifest
            .decision
            .destinations
            .iter()
            .find(|d| {
                d.version_size == record.version_size.as_str()
                    && d.path.key().ok().as_deref() == Some(path.as_str())
            })
            .ok_or(StateError::ProviderSelectionInvalid)?;
        let local = data_encoding::HEXLOWER.encode(&file.fingerprint.sha256);
        let group: Option<String> = conn.query_row(
            "SELECT grouping_hash FROM provider_active_destinations WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5",
            params![generation,key,child,record.version_size.as_str(),path], |row| row.get(0),
        )?;
        // A journal may be finishing an interrupted older recipe. Its commit
        // proves media, while the current recipe is acknowledged separately by
        // the managed owner against this one frozen selection generation.
        let metadata_complete = !root.spec.metadata_enabled
            || (selected_generation.is_some()
                && root.spec.metadata_flags == flags
                && file.metadata_decision.is_some()
                && group.as_deref()
                    == Some(grouping_hash(conn, &record.library, &record.id)?.as_str()));
        if file.local_preserved {
            if file.source_checksum.is_some() {
                return invalid();
            }
            let preserved = PreservedLocal {
                intent: preservation_intent(&root, &manifest, destination)?,
                checksum: local.clone(),
                size: file.fingerprint.size,
                modified: file.local_modified,
            };
            conn.execute("UPDATE provider_active_destinations SET preserved_local=?8,prepared_checksum=NULL,prepared_size=NULL,prepared_hash=NULL,verified_media=0,verified_metadata=?7,local_checksum=?6,source_checksum=NULL WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5",params![generation,key,child,record.version_size.as_str(),path,local,metadata_complete,encode(&preserved)?])?;
        } else {
            conn.execute("UPDATE provider_active_destinations SET current_publication=CASE WHEN local_checksum IS ?6 AND source_checksum IS ?7 THEN current_publication ELSE 0 END,confirmed_repair=NULL,preserved_local=NULL,prepared_checksum=NULL,prepared_size=NULL,prepared_hash=NULL,verified_media=1,verified_metadata=?8,local_checksum=?6,source_checksum=?7 WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5",params![generation,key,child,record.version_size.as_str(),path,local,file.source_checksum,metadata_complete])?;
        }
        let fields = serde_json::to_string(&crate::state::types::EmbeddedMetadataReceipt {
            published: true,
            input_checksum: local.clone(),
            output_checksum: local.clone(),
            previous: Default::default(),
            written: (*file.embedded_fields).clone(),
        })
        .map_err(|_invalid| StateError::ProviderSelectionInvalid)?;
        refresh_progress_hash(conn, &root, &manifest, destination)?;
        if selected_generation.is_some()
            && file.local_preserved
            && metadata_complete
            && ((root.spec.metadata_enabled && file.metadata_decision.is_some())
                || *file.embedded_fields != Default::default())
        {
            replace_current_publication(
                conn,
                &record.library,
                &path,
                Some((&generation, &key, &child, record.version_size.as_str())),
            )?;
        }
        conn.execute("UPDATE provider_active_destinations SET embedded_metadata_receipt=?6 WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5",params![generation,key,child,record.version_size.as_str(),path,fields])?;
        validate_decision_rows(
            conn,
            &root,
            &manifest,
            confirmed_decision_asset(&manifest, &root)?.as_ref(),
        )?;
    }
    Ok(())
}

impl SqliteStateDb {
    /// Reuse an unchanged journal binding after its owner has checked the exact
    /// recipe and measured both media and sidecars under the destination locks.
    pub(crate) async fn reuse_layout_selection_publication(
        &self,
        owner: account::AccountOwner,
        generation: String,
        binding: super::primary_layout::LayoutBinding,
    ) -> Result<(), StateError> {
        self.with_conn_mut("acknowledging selected managed layout", move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            account::validate_authenticated(&tx, &owner)?;
            let root = load_generation(&tx, &generation)?;
            let stored: Vec<u8> = tx.query_row(
                "SELECT binding FROM primary_layout_bindings WHERE family=?1 AND library=?2 AND child=?3 AND generation=?4",
                params![binding.family,binding.library,binding.child,binding.generation], |row| row.get(0),
            )?;
            if stored != encode(&binding)? {
                return Err(StateError::ProviderWorkConflict("managed_binding_changed"));
            }
            for file in &binding.files {
                let selected: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM provider_active_destinations WHERE generation=?1 AND library=?2 AND asset_id=?3 AND version_size=?4 AND path=?5 AND checksum=?6 AND admitted=1)",
                    params![generation,binding.library,binding.child,file.version,file.path.key()?,file.provider_checksum], |row| row.get(0),
                )?;
                if !selected {
                    // Retained unselected originals belong to the layout, but
                    // do not acquire a provider selection or writer obligation.
                    continue;
                }
                super::primary_layout::guard_downloaded_checksum(
                    &tx, &file.path.to_path(), &data_encoding::HEXLOWER.encode(&file.fingerprint.sha256),
                )?;
                let record = tx.query_row(
                    &format!("SELECT {} FROM assets WHERE library=?1 AND id=?2 AND version_size=?3 AND checksum=?4 AND is_deleted=0", super::rows::ASSET_COLUMNS),
                    params![binding.library,binding.child,file.version,file.provider_checksum], super::rows::row_to_asset_record,
                )?;
                record_layout_destination_for_generation(&tx, &record, file, root.spec.metadata_flags, Some(&generation))?;
            }
            tx.commit()?;
            Ok(())
        }).await
    }
}

enum VerifiedDestinationSource<'a> {
    PathPublication(&'a str),
    // An unfinished admitted destination independently measured as its exact
    // current raw SHA-256 provider resource after publication lost state.
    RecoveredPublication(&'a str),
    // The existing directory journal proves an actual durable move. Receipt
    // settlement consumes that handoff in the same native publication transaction.
    DirectoryPublication(&'a str),
    // Only the reuse owner constructs this after validating the retained
    // native generation/decision and exact source, path and local fingerprint.
    RetainedNative(&'a str),
    // The reuse owner independently fingerprints an exact durable import.
    // This is its pre-rewrite local input, not provider download provenance.
    ImportedLocal(&'a str),
    // Exact prior canonical publication with matching per-path download proof.
    PriorPublication(&'a str),
}

/// Move exact native-path authority only inside the transaction finalizing a
/// checked publication. Historical verification remains intact. Selection,
/// import and receipt reuse must never call this to establish authority.
pub(super) fn replace_current_publication(
    conn: &Connection,
    library: &str,
    path: &str,
    current: Option<(&str, &str, &str, &str)>,
) -> Result<(), StateError> {
    if conn.is_autocommit() {
        return invalid();
    }
    let native_path =
        serde_json::from_str(path).map_err(|_error| StateError::ProviderSelectionInvalid)?;
    settle_directory_publications(conn, library, &native_path)?;
    if let Some((generation, key, child, version)) = current {
        // Only the current physical publication can lend unchanged-byte field
        // ownership to its successor. Historical generations are not sources.
        conn.execute("UPDATE provider_active_destinations AS next SET embedded_metadata_receipt=
            (SELECT CASE
                WHEN json_extract(old.embedded_metadata_receipt,'$.output_checksum')=next.local_checksum
                    THEN json_set(old.embedded_metadata_receipt,'$.published',json('true'))
                WHEN json_extract(old.embedded_metadata_receipt,'$.published')=0 AND json_extract(old.embedded_metadata_receipt,'$.input_checksum')=next.local_checksum
                    THEN json_set(old.embedded_metadata_receipt,'$.published',json('true'),'$.output_checksum',next.local_checksum,'$.written',json_extract(old.embedded_metadata_receipt,'$.previous'))
                END FROM provider_active_destinations old WHERE old.library=?6 AND old.path=?5 AND old.current_publication=1 AND old.local_checksum=next.local_checksum)
            WHERE next.generation=?1 AND next.pass_key=?2 AND next.child=?3 AND next.version_size=?4 AND next.path=?5 AND next.library=?6",
            params![generation,key,child,version,path,library])?;
    }
    conn.execute("UPDATE provider_active_destinations SET current_publication=0 WHERE library=?1 AND path=?2 AND current_publication=1", params![library,path])?;
    if let Some((generation, key, child, version)) = current {
        let root = load_generation(conn, generation)?;
        load_decision(conn, &root, key, child)?;
        let changed = conn.execute("UPDATE provider_active_destinations SET current_publication=1 WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5 AND library=?6 AND admitted=1 AND length(local_checksum)=64 AND ((verified_media=1 AND preserved_local IS NULL AND length(source_checksum)=64) OR (verified_media=0 AND preserved_local IS NOT NULL AND source_checksum IS NULL)) AND EXISTS(SELECT 1 FROM provider_active_generations g JOIN account_owner a ON a.account_key=g.account_key AND a.provider_key=g.provider_key WHERE g.id=?1)", params![generation,key,child,version,path,library])?;
        if changed != 1 {
            return invalid();
        }
    }
    Ok(())
}

/// Finalize actual provider publication under the caller's durable transaction.
/// Confirmation, import and receipt reuse must use their non-publishing paths.
pub(super) fn record_verified_destination(
    conn: &Connection,
    library: &str,
    asset: &str,
    version: &str,
    path: &Path,
    local: &str,
    source: &str,
) -> Result<(), StateError> {
    record_destination_source(
        conn,
        library,
        asset,
        version,
        path,
        local,
        VerifiedDestinationSource::PathPublication(source),
    )
}

fn record_destination_source(
    conn: &Connection,
    library: &str,
    asset: &str,
    version: &str,
    path: &Path,
    local: &str,
    source: VerifiedDestinationSource<'_>,
) -> Result<(), StateError> {
    let (source, require_path_receipt, published) = match source {
        VerifiedDestinationSource::PathPublication(source) => (source, true, true),
        VerifiedDestinationSource::RecoveredPublication(source) => (source, false, true),
        VerifiedDestinationSource::DirectoryPublication(source) => (source, false, true),
        VerifiedDestinationSource::RetainedNative(source) => (source, false, false),
        VerifiedDestinationSource::ImportedLocal(source) => (source, false, false),
        VerifiedDestinationSource::PriorPublication(source) => (source, false, false),
    };
    validate_identity_decisions(conn, library, asset)?;
    let exists:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM provider_active_destinations WHERE library=?1 AND asset_id=?2 AND version_size=?3 AND admitted=1)",params![library,asset,version],|r|r.get(0))?;
    if !exists {
        return Ok(());
    }
    let path = super::provider_selection::SelectionPath::from_path(
        &std::path::absolute(path).map_err(|error| StateError::TempPath {
            path: path.to_path_buf(),
            source: error,
        })?,
    )
    .key()?;
    if source.len() != 64
        || local.len() != 64
        || !source
            .bytes()
            .chain(local.bytes())
            .all(|b| b.is_ascii_hexdigit())
    {
        return invalid();
    }
    // Fresh/fallback publication requires its independent per-path source.
    // Validated native reuse has its own lossless receipt, including prepared
    // rewrite outputs that must not update a lossy TEXT sibling's checksum.
    // Bind MMCS as a provider generation, never as a local content digest.
    let rows:Vec<(String,String,String)>=conn.prepare("SELECT generation,pass_key,child FROM provider_active_destinations WHERE library=?1 AND asset_id=?2 AND version_size=?3 AND path=?4 AND admitted=1 AND EXISTS(SELECT 1 FROM assets a WHERE a.library=?1 AND a.id=?2 AND a.version_size=?3 AND a.checksum=provider_active_destinations.checksum AND a.size_bytes=provider_active_destinations.size_bytes AND a.metadata_hash=provider_active_destinations.metadata_hash AND a.created_at IS provider_active_destinations.created_at AND a.added_at IS provider_active_destinations.added_at AND a.status='downloaded' AND a.is_deleted=0 AND (?7=0 OR EXISTS(SELECT 1 FROM asset_metadata_paths p WHERE p.library=a.library AND p.id=a.id AND p.version_size=a.version_size AND p.provider_checksum=a.checksum AND p.local_path=provider_active_destinations.compat_path AND p.local_checksum=?5 AND p.source_checksum=?6)))")?.query_map(params![library,asset,version,path,local,source,require_path_receipt],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?.collect::<Result<_,_>>()?;
    for (generation, key, child) in &rows {
        let root = load_generation(conn, generation)?;
        let manifest = load_decision(conn, &root, key, child)?;
        let destination = manifest
            .decision
            .destinations
            .iter()
            .find(|d| {
                d.version_size == version && d.path.key().ok().as_deref() == Some(path.as_str())
            })
            .ok_or(StateError::ProviderSelectionInvalid)?;
        conn.execute("UPDATE provider_active_destinations SET current_publication=CASE WHEN local_checksum IS ?6 AND source_checksum IS ?7 THEN current_publication ELSE 0 END,confirmed_repair=NULL,preserved_local=NULL,prepared_checksum=NULL,prepared_size=NULL,prepared_hash=NULL,verified_media=1,local_checksum=?6,source_checksum=?7 WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5",params![generation,key,child,version,path,local,source])?;
        refresh_progress_hash(conn, &root, &manifest, destination)?;
    }
    if published {
        replace_current_publication(
            conn,
            library,
            &path,
            rows.first().map(|(generation, key, child)| {
                (generation.as_str(), key.as_str(), child.as_str(), version)
            }),
        )?;
    }
    Ok(())
}

impl SqliteStateDb {
    /// Reuse independently verified active publication at the same lossless
    /// physical path across compatible passes. Legacy receipts require exact
    /// UTF-8 identity; every reuse requires an independently re-read local SHA.
    pub(crate) async fn reuse_selection_publication(
        &self,
        owner: account::AccountOwner,
        generation: String,
        key: String,
        child: String,
        path: super::provider_selection::SelectionPath,
        fingerprint: (String, u64),
        file_still_matches: impl Fn() -> bool + Send + 'static,
    ) -> Result<bool, StateError> {
        let (local, size) = fingerprint;
        self.with_conn_mut("reusing exact destination publication",move|conn|{
            let tx=conn.transaction_with_behavior(TransactionBehavior::Immediate)?;account::validate_authenticated(&tx,&owner)?;
            let root=load_generation(&tx,&generation)?;let manifest=load_decision(&tx,&root,&key,&child)?;
            if !manifest.decision.destinations.iter().any(|d|d.path==path) {return invalid()}
            let native=path.to_path();
            let selected=manifest.decision.destinations.iter().find(|d|d.path==path).ok_or(StateError::ProviderSelectionInvalid)?;
            let library=manifest.media_zone(&root).get("zoneName").and_then(Value::as_str).ok_or(StateError::ProviderSelectionInvalid)?;
            if let Some((moved, handoff)) = directory_publication_source(&tx,&root.spec.scope,&key,&child,library,&path,(&local,size))?
                && moved.version_size == selected.version_size && moved.checksum == selected.checksum && moved.size == selected.size {
                let source = handoff.source;
                super::asset_writes::update_status_to_downloaded(&tx,library,&manifest.state_id,&selected.version_size,&native,&local,Some(&source),false,chrono::Utc::now().timestamp())?;
                record_destination_source(&tx,library,&manifest.state_id,&selected.version_size,&native,&local,VerifiedDestinationSource::DirectoryPublication(&source))?;
                if let Some(embedded) = handoff.embedded.filter(|receipt| receipt.published && receipt.output_checksum == local) {
                    let raw = serde_json::to_string(&embedded).map_err(|_invalid| StateError::ProviderSelectionInvalid)?;
                    let installed = tx.execute("UPDATE provider_active_destinations SET embedded_metadata_receipt=?4 WHERE library=?1 AND path=?2 AND local_checksum=?3 AND current_publication=1 AND preserved_local IS NULL",params![library,path.key()?,local,raw])?;
                    if installed != 1 { return invalid(); }
                }
                let completed:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM provider_active_destinations WHERE generation=?1 AND pass_key=?2 AND child=?3 AND path=?4 AND version_size=?5 AND verified_media=1 AND preserved_local IS NULL AND local_checksum=?6 AND source_checksum=?7)",params![generation,key,child,path.key()?,selected.version_size,local,source],|row|row.get(0))?;
                if !completed { return invalid(); }
                commit_observed_destination(tx, &file_still_matches)?;
                return Ok(true);
            }
            let previous:Option<(String,String,String)>=tx.query_row("SELECT w.generation,w.source_checksum,w.pass_key FROM provider_active_destinations w JOIN provider_active_generations g ON g.id=w.generation WHERE g.scope=?1 AND w.child=?3 AND w.version_size=?4 AND w.path=?5 AND w.checksum=?6 AND w.size_bytes=?7 AND w.master=?8 AND w.admitted=1 AND w.verified_media=1 AND (w.local_checksum=?9 OR (w.prepared_checksum=?9 AND w.prepared_size=?10)) ORDER BY g.created_at DESC LIMIT 1",params![root.spec.scope,key,child,selected.version_size,path.key()?,selected.checksum,i64::try_from(selected.size).map_err(|_invalid|StateError::ProviderSelectionInvalid)?,manifest.decision.master,local,i64::try_from(size).map_err(|_invalid|StateError::ProviderSelectionInvalid)?],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
            if let Some((previous,source,previous_key))=previous {
                let previous_root=load_generation(&tx,&previous)?;load_decision(&tx,&previous_root,&previous_key,&child)?;
                let prepared:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM provider_active_destinations WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5 AND prepared_checksum=?6 AND prepared_size=?7)",params![previous,previous_key,child,selected.version_size,path.key()?,local,i64::try_from(size).map_err(|_invalid|StateError::ProviderSelectionInvalid)?],|r|r.get(0))?;
                let recovered_fields: Option<String> = tx.query_row("SELECT embedded_metadata_receipt FROM provider_active_destinations WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5 AND json_extract(embedded_metadata_receipt,'$.output_checksum')=?6 AND (?7 OR json_extract(embedded_metadata_receipt,'$.published')=0)",params![previous,previous_key,child,selected.version_size,path.key()?,local,prepared],|r|r.get(0)).optional()?.flatten();
                if prepared {
                    let pending=metadata_receipt(&tx,&previous,&previous_key,&child,&selected.version_size,&path.key()?)?;
                    super::metadata::settle_selection_prepared_input(&tx,&pending,&(local.clone(),size))?;
                }
                // A provider revert can restore an earlier exact publication
                // while the canonical row is pending for the reverted version.
                // Restore that row only with independent downloaded provenance;
                // an imported native receipt must not invent download history.
                let library=manifest.media_zone(&root).get("zoneName").and_then(Value::as_str).ok_or(StateError::ProviderSelectionInvalid)?;
                let restorable:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM assets a JOIN asset_metadata_paths p USING(library,id,version_size) WHERE a.library=?1 AND a.id=?2 AND a.version_size=?3 AND a.status IN ('pending','failed') AND a.is_deleted=0 AND a.checksum=?4 AND a.size_bytes=?5 AND p.provider_checksum=a.checksum AND p.local_path=?6 AND p.local_checksum=?7 AND p.source_checksum=?8)",params![library,manifest.state_id,selected.version_size,selected.checksum,i64::try_from(selected.size).map_err(|_invalid|StateError::ProviderSelectionInvalid)?,native.to_str(),local,source],|r|r.get(0))?;
                if restorable {
                    super::asset_writes::update_status_to_downloaded(&tx,library,&manifest.state_id,&selected.version_size,&native,&local,Some(&source),false,chrono::Utc::now().timestamp())?;
                }
                record_destination_source(&tx,manifest.media_zone(&root).get("zoneName").and_then(Value::as_str).ok_or(StateError::ProviderSelectionInvalid)?,&manifest.state_id,&selected.version_size,&native,&local,VerifiedDestinationSource::RetainedNative(&source))?;
                if let Some(fields) = recovered_fields {
                    // The validated prepared writer and measured output prove this
                    // field handoff. They do not complete today's metadata intent.
                    tx.execute("UPDATE provider_active_destinations SET embedded_metadata_receipt=?6 WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5 AND local_checksum=?7 AND verified_media=1",params![generation,key,child,selected.version_size,path.key()?,fields,local])?;
                }
                commit_observed_destination(tx, &file_still_matches)?;return Ok(true)
            }
            let text=native.to_str();
            // Legacy metadata publication retains its original download hash
            // separately from the final local bytes. Neither hash can transfer
            // from another path or provider version.
            let prior:Option<String>=tx.query_row("SELECT a.download_checksum FROM assets a JOIN asset_metadata_paths p USING(library,id,version_size) WHERE a.library=?1 AND a.id=?2 AND a.version_size=?3 AND a.status='downloaded' AND a.is_deleted=0 AND a.downloaded_at IS NOT NULL AND a.local_path=?4 AND a.local_checksum=?5 AND length(a.download_checksum)=64 AND p.local_path=a.local_path AND p.local_checksum=a.local_checksum AND p.download_checksum=a.download_checksum AND p.provider_checksum=?6 AND a.checksum=p.provider_checksum AND a.size_bytes=?7 AND (a.download_checksum<>a.local_checksum OR a.size_bytes=?8)",params![manifest.media_zone(&root).get("zoneName").and_then(Value::as_str),manifest.state_id,selected.version_size,text,local,selected.checksum,i64::try_from(selected.size).map_err(|_invalid|StateError::ProviderSelectionInvalid)?,i64::try_from(size).map_err(|_invalid|StateError::ProviderSelectionInvalid)?],|r|r.get(0)).optional()?;
            if let Some(source)=prior {
                record_destination_source(&tx,manifest.media_zone(&root).get("zoneName").and_then(Value::as_str).ok_or(StateError::ProviderSelectionInvalid)?,&manifest.state_id,&selected.version_size,&native,&local,VerifiedDestinationSource::PriorPublication(&source))?;
                commit_observed_destination(tx, &file_still_matches)?;return Ok(true);
            }
            // A raw landed publication can outlive its failed finalization
            // transaction. Only an exact current SHA-256 source proves those
            // bytes without a receipt; opaque provider identities and rewritten
            // media must use the receipt paths below. Metadata intent stays
            // unfinished: exact raw media proves no sidecar or embedded output.
            // Same-resource root drift preserves the old downloaded catalog row
            // until new-path finalization commits. That old-path status is not
            // completion of the current admitted destination.
            let raw_source = data_encoding::BASE64.decode(selected.checksum.as_bytes())
                .ok().filter(|digest| digest.len() == 32)
                .map(|digest| data_encoding::HEXLOWER.encode(&digest));
            if size == selected.size && raw_source.as_deref() == Some(local.as_str()) {
                let library=manifest.media_zone(&root).get("zoneName").and_then(Value::as_str).ok_or(StateError::ProviderSelectionInvalid)?;
                let unfinished:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM assets a JOIN provider_active_destinations d ON d.library=a.library AND d.asset_id=a.id AND d.version_size=a.version_size WHERE d.generation=?1 AND d.pass_key=?2 AND d.child=?3 AND d.path=?4 AND d.admitted=1 AND d.verified_media=0 AND d.prepared_checksum IS NULL AND (a.status IN ('pending','failed') OR (a.status='downloaded' AND a.downloaded_at IS NOT NULL AND a.local_path IS NOT NULL AND a.local_path<>d.compat_path)) AND a.is_deleted=0 AND (a.downloaded_at IS NULL OR a.local_path IS NOT d.compat_path) AND a.imported_size IS NULL AND a.checksum=d.checksum AND a.size_bytes=d.size_bytes AND a.metadata_hash IS d.metadata_hash AND a.created_at IS d.created_at AND a.added_at IS d.added_at)",params![generation,key,child,path.key()?],|r|r.get(0))?;
                if unfinished {
                    let now=chrono::Utc::now().timestamp();
                    super::asset_writes::update_status_to_downloaded(&tx,library,&manifest.state_id,&selected.version_size,&native,&local,Some(&local),false,now)?;
                    tx.execute("UPDATE asset_metadata_paths SET source_checksum=?5 WHERE library=?1 AND id=?2 AND version_size=?3 AND local_path=?4",params![library,manifest.state_id,selected.version_size,text,local])?;
                    record_destination_source(&tx,library,&manifest.state_id,&selected.version_size,&native,&local,VerifiedDestinationSource::RecoveredPublication(&local))?;
                    super::asset_writes::record_metadata_capture_revision(&tx,library,&manifest.state_id,crate::state::types::METADATA_CAPTURE_REVISION,now)?;
                    commit_observed_destination(tx, &file_still_matches)?;
                    return Ok(true);
                }
            }
            let receipt:Option<String>=tx.query_row("SELECT p.source_checksum FROM asset_metadata_paths p JOIN assets a USING(library,id,version_size) WHERE p.library=?1 AND p.id=?2 AND p.local_path=?3 AND p.local_checksum=?4 AND p.source_checksum IS NOT NULL AND a.metadata_hash IS ?5 AND p.version_size=?6 AND p.provider_checksum=?7 AND a.checksum=p.provider_checksum",params![manifest.media_zone(&root).get("zoneName").and_then(Value::as_str),manifest.state_id,text,local,manifest.decision.destinations.iter().find(|d|d.path==path).map(|d|&d.metadata_hash),manifest.decision.destinations.iter().find(|d|d.path==path).map(|d|&d.version_size),selected.checksum],|r|r.get(0)).optional()?;
            if let Some(source)=receipt {
                record_destination_source(&tx,manifest.media_zone(&root).get("zoneName").and_then(Value::as_str).ok_or(StateError::ProviderSelectionInvalid)?,&manifest.state_id,
                    &manifest.decision.destinations.iter().find(|d|d.path==path).ok_or(StateError::ProviderSelectionInvalid)?.version_size,&native,&local,VerifiedDestinationSource::PriorPublication(&source))?;
                commit_observed_destination(tx, &file_still_matches)?;Ok(true)
            } else {
                // An import or prior exact adoption has no download/source receipt. Its durable local
                // fingerprint can establish media ownership without inventing
                // a provider download checksum or fetching comparison bytes.
                let imported:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM assets a JOIN asset_metadata_paths p USING(library,id,version_size) WHERE a.library=?1 AND a.id=?2 AND a.version_size=?3 AND a.status='downloaded' AND a.is_deleted=0 AND a.local_path=?4 AND a.local_checksum=?5 AND (a.imported_size=?6 OR (a.imported_size IS NULL AND a.downloaded_at IS NOT NULL AND a.size_bytes=?6)) AND a.download_checksum IS NULL AND p.local_path=a.local_path AND p.local_checksum=a.local_checksum AND p.provider_checksum=?7 AND a.checksum=p.provider_checksum AND p.source_checksum IS NULL)",params![manifest.media_zone(&root).get("zoneName").and_then(Value::as_str),manifest.state_id,selected.version_size,text,local,i64::try_from(size).map_err(|_invalid|StateError::ProviderSelectionInvalid)?,selected.checksum],|r|r.get(0))?;
                if imported {
                    record_destination_source(&tx,manifest.media_zone(&root).get("zoneName").and_then(Value::as_str).ok_or(StateError::ProviderSelectionInvalid)?,&manifest.state_id,&selected.version_size,&native,&local,VerifiedDestinationSource::ImportedLocal(&local))?;
                }
                if imported { commit_observed_destination(tx, &file_still_matches)?; }
                Ok(imported)
            }
        }).await
    }

    /// Metadata completion follows the existing option-aware rewrite drain and
    /// exact current path debt. Media completion is a separate verified receipt.
    pub(crate) async fn complete_selection_metadata(
        &self,
        owner: account::AccountOwner,
        generation: String,
    ) -> Result<(), StateError> {
        self.with_conn_mut("recording configured destination metadata completion",move|conn|{
            let tx=conn.transaction_with_behavior(TransactionBehavior::Immediate)?;account::validate_authenticated(&tx,&owner)?;
            let root=load_generation(&tx,&generation)?;
            if !root.spec.metadata_enabled {
                let keys:Vec<(String,String)>=tx.prepare("SELECT DISTINCT pass_key,child FROM provider_active_destinations WHERE generation=?1 AND admitted=1 AND (verified_media=1 OR preserved_local IS NOT NULL)")?.query_map([&generation],|r|Ok((r.get(0)?,r.get(1)?)))?.collect::<Result<_,_>>()?;
                for (key,child) in keys {
                    let manifest=load_decision(&tx,&root,&key,&child)?;
                    tx.execute("UPDATE provider_active_destinations SET verified_metadata=1 WHERE generation=?1 AND pass_key=?2 AND child=?3 AND admitted=1 AND (verified_media=1 OR preserved_local IS NOT NULL)",params![generation,key,child])?;
                    for destination in &manifest.decision.destinations {refresh_progress_hash(&tx,&root,&manifest,destination)?;}
                }
            }
            tx.commit()?;Ok(())
        }).await
    }
}

/// Validate the fresh selection that still depends on the older catalogue input.
pub(super) fn validate_blocked_successor(
    conn: &Connection,
    generation: &str,
    key: &str,
    child: &str,
) -> Result<(), StateError> {
    let root = load_generation(conn, generation)?;
    let manifest = load_decision(conn, &root, key, child)?;
    if manifest.decision.outcome != SelectionOutcome::Selected
        || manifest.decision.confirmation.is_none()
    {
        return invalid();
    }
    Ok(())
}

/// Adapt one exact native destination into the existing metadata write owner.
/// Legacy path TEXT is neither decoded nor used as publication evidence.
pub(super) fn metadata_receipt(
    conn: &Connection,
    generation: &str,
    key: &str,
    child: &str,
    version: &str,
    path_key: &str,
) -> Result<super::PendingMetadataRewrite, StateError> {
    let pending = queued_metadata_receipt(conn, generation, key, child, version, path_key)?;
    let receipt = pending
        .selection_receipt
        .as_ref()
        .ok_or(StateError::ProviderSelectionInvalid)?;
    if grouping_hash(conn, &pending.asset.library, &pending.asset.id)? != receipt.grouping_hash {
        return Err(StateError::ProviderWorkConflict("frozen_grouping_changed"));
    }
    Ok(pending)
}

pub(super) fn queued_metadata_receipt(
    conn: &Connection,
    generation: &str,
    key: &str,
    child: &str,
    version: &str,
    path_key: &str,
) -> Result<super::PendingMetadataRewrite, StateError> {
    let root = load_generation(conn, generation)?;
    let manifest = load_decision(conn, &root, key, child)?;
    let destination = manifest
        .decision
        .destinations
        .iter()
        .find(|d| d.version_size == version && d.path.key().ok().as_deref() == Some(path_key))
        .ok_or(StateError::ProviderSelectionInvalid)?;
    let (local,source,group):(String,Option<String>,String)=conn.query_row("SELECT local_checksum,source_checksum,grouping_hash FROM provider_active_destinations WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5 AND admitted=1 AND (verified_media=1 OR preserved_local IS NOT NULL) AND verified_metadata=0 AND metadata_successor IS NULL",params![generation,key,child,version,path_key],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
    let library = manifest
        .media_zone(&root)
        .get("zoneName")
        .and_then(Value::as_str)
        .ok_or(StateError::ProviderSelectionInvalid)?;
    let mut asset=conn.query_row(&format!("SELECT {} FROM assets WHERE library=?1 AND id=?2 AND version_size=?3 AND checksum=?4 AND metadata_hash=?5 AND is_deleted=0",super::rows::ASSET_COLUMNS),params![library,manifest.state_id,version,destination.checksum,destination.metadata_hash],super::rows::row_to_asset_record)?;
    asset.local_path = Some(destination.path.to_path());
    asset.local_checksum = Some(local.clone());
    asset.download_checksum = source.clone();
    asset.status = crate::state::AssetStatus::Downloaded;
    Ok(super::PendingMetadataRewrite {
        asset,
        capture_repair_receipt: None,
        source_checksum: source,
        selection_receipt: Some(super::contracts::SelectionMetadataReceipt {
            writer_output: None,
            preserved_local: preserved_local(conn, &root, &manifest, destination)?,
            generation: generation.to_owned(),
            pass_key: key.to_owned(),
            child: child.to_owned(),
            grouping_hash: group,
            metadata_flags: root.spec.metadata_flags,
            prepared: prepared_metadata(conn, &root, &manifest, destination)?,
        }),
    })
}

/// Recover measured prior writer output without completing the current intent.
pub(super) fn recover_metadata_field_output(
    conn: &Connection,
    pending: &super::PendingMetadataRewrite,
    fields: &crate::state::types::EmbeddedMetadataReceipt,
    output_size: u64,
) -> Result<usize, StateError> {
    let Some(receipt) = &pending.selection_receipt else {
        return Ok(0);
    };
    let path = super::provider_selection::SelectionPath::from_path(
        pending
            .asset
            .local_path
            .as_deref()
            .ok_or(StateError::ProviderSelectionInvalid)?,
    )
    .key()?;
    let current = metadata_receipt(
        conn,
        &receipt.generation,
        &receipt.pass_key,
        &receipt.child,
        pending.asset.version_size.as_str(),
        &path,
    )?;
    if current.asset.local_checksum != pending.asset.local_checksum
        || current.source_checksum != pending.source_checksum
        || current.selection_receipt.as_ref().is_none_or(|r| {
            r.preserved_local != receipt.preserved_local
                || r.grouping_hash != receipt.grouping_hash
                || r.metadata_flags != receipt.metadata_flags
        })
    {
        return Err(StateError::ProviderWorkConflict("embedded_input_changed"));
    }
    let preserved = receipt
        .preserved_local
        .as_ref()
        .map(|prior| {
            let mut evidence = prior.clone();
            evidence.checksum = fields.output_checksum.clone();
            evidence.size = output_size;
            encode(&evidence)
        })
        .transpose()?;
    let raw =
        serde_json::to_string(fields).map_err(|_invalid| StateError::ProviderSelectionInvalid)?;
    let root = load_generation(conn, &receipt.generation)?;
    let manifest = load_decision(conn, &root, &receipt.pass_key, &receipt.child)?;
    let changed = conn.execute("UPDATE provider_active_destinations SET local_checksum=?7,preserved_local=?11,embedded_metadata_receipt=?5,prepared_checksum=NULL,prepared_size=NULL,prepared_hash=NULL
        WHERE library=?1 AND asset_id=?2 AND version_size=?3 AND path=?4 AND admitted=1
        AND (embedded_metadata_receipt=?5 OR EXISTS(SELECT 1 FROM provider_active_destinations old WHERE old.library=?1 AND old.asset_id=?2 AND old.version_size=?3 AND old.path=?4 AND old.current_publication=1 AND old.embedded_metadata_receipt=?5))
        AND generation=?8 AND pass_key=?9 AND child=?10 AND (preserved_local IS NULL)=(?11 IS NULL)
        AND local_checksum IN (?6,?7)", params![pending.asset.library,pending.asset.id,pending.asset.version_size.as_str(),path,raw,fields.input_checksum,fields.output_checksum,receipt.generation,receipt.pass_key,receipt.child,preserved])?;
    if changed != 1 {
        return Err(StateError::ProviderWorkConflict("embedded_output_changed"));
    }
    let destination = manifest
        .decision
        .destinations
        .iter()
        .find(|d| {
            d.version_size == pending.asset.version_size.as_str()
                && d.path.key().ok().as_deref() == Some(path.as_str())
        })
        .ok_or(StateError::ProviderSelectionInvalid)?;
    refresh_progress_hash(conn, &root, &manifest, destination)?;
    Ok(changed)
}

/// Actual writer completion compares immutable selection, current grouping
/// dependencies, exact native path and independently selected input checksum.
/// Completion of A cannot acknowledge another destination B.
pub(super) fn finish_metadata_receipt(
    conn: &Connection,
    pending: &super::PendingMetadataRewrite,
    local: Option<&str>,
    complete: bool,
) -> Result<Option<bool>, StateError> {
    let Some(receipt) = &pending.selection_receipt else {
        return Ok(None);
    };
    let root = load_generation(conn, &receipt.generation)?;
    let manifest = load_decision(conn, &root, &receipt.pass_key, &receipt.child)?;
    if root.spec.metadata_flags != receipt.metadata_flags
        || grouping_hash(conn, &pending.asset.library, &manifest.state_id)? != receipt.grouping_hash
    {
        return Err(StateError::ProviderWorkConflict(
            "writer_dependencies_changed",
        ));
    }
    let path = super::provider_selection::SelectionPath::from_path(
        pending
            .asset
            .local_path
            .as_deref()
            .ok_or(StateError::ProviderSelectionInvalid)?,
    )
    .key()?;
    if conn.query_row("SELECT metadata_successor IS NOT NULL FROM provider_active_destinations WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5",
        params![root.id,receipt.pass_key,receipt.child,pending.asset.version_size.as_str(),path], |r|r.get::<_,bool>(0))? {
        return Err(StateError::ProviderWorkConflict("metadata_writer_replaced"));
    }
    let selected_destination = manifest
        .decision
        .destinations
        .iter()
        .find(|d| {
            d.version_size == pending.asset.version_size.as_str()
                && d.path.key().ok().as_deref() == Some(path.as_str())
        })
        .ok_or(StateError::ProviderSelectionInvalid)?;
    if prepared_metadata(conn, &root, &manifest, selected_destination)? != receipt.prepared {
        return Err(StateError::ProviderWorkConflict("prepared_receipt_changed"));
    }
    if local.is_some()
        && local != pending.asset.local_checksum.as_deref()
        && receipt
            .prepared
            .as_ref()
            .is_none_or(|(output, _size)| Some(output.as_str()) != local)
    {
        return invalid();
    }
    let changed = if let Some(expected) = &receipt.preserved_local {
        if preserved_local(conn, &root, &manifest, selected_destination)?.as_ref() != Some(expected)
            || pending.source_checksum.is_some()
        {
            return Err(StateError::ProviderWorkConflict("preserved_input_changed"));
        }
        let mut updated = expected.clone();
        if let Some(local) = local {
            updated.checksum = local.to_owned();
            if let Some((output, size)) = &receipt.prepared
                && output == local
            {
                updated.size = *size;
            }
        }
        let changed = conn.execute(
            "UPDATE provider_active_destinations SET preserved_local=?6,local_checksum=?7,verified_metadata=?8,prepared_checksum=NULL,prepared_size=NULL,prepared_hash=NULL WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5 AND admitted=1 AND verified_media=0 AND source_checksum IS NULL AND preserved_local=?9",
            params![root.id,receipt.pass_key,receipt.child,selected_destination.version_size,path,encode(&updated)?,updated.checksum,complete,encode(expected)?],
        )?;
        refresh_progress_hash(conn, &root, &manifest, selected_destination)?;
        changed
    } else {
        conn.execute("UPDATE provider_active_destinations SET prepared_checksum=NULL,prepared_size=NULL,prepared_hash=NULL,local_checksum=COALESCE(?8,local_checksum),verified_metadata=CASE WHEN ?9=1 THEN 1 ELSE verified_metadata END WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5 AND admitted=1 AND verified_media=1 AND verified_metadata=0 AND local_checksum IS ?6 AND grouping_hash=?7 AND source_checksum IS ?10 AND metadata_hash IS ?11 AND EXISTS(SELECT 1 FROM assets a WHERE a.library=provider_active_destinations.library AND a.id=provider_active_destinations.asset_id AND a.version_size=provider_active_destinations.version_size AND a.checksum=provider_active_destinations.checksum AND a.metadata_hash=provider_active_destinations.metadata_hash AND a.created_at IS provider_active_destinations.created_at AND a.added_at IS provider_active_destinations.added_at AND a.is_deleted=0)",params![receipt.generation,receipt.pass_key,receipt.child,pending.asset.version_size.as_str(),path,pending.asset.local_checksum,receipt.grouping_hash,local,complete,pending.source_checksum,pending.asset.metadata.metadata_hash])?
    };
    if changed > 0 && receipt.preserved_local.is_none() {
        if complete && let (Some(writer), Some(media)) = (receipt.writer_output, local) {
            writer_output::record(
                conn,
                pending,
                &root,
                &manifest,
                selected_destination,
                media,
                writer,
            )?;
        }
        if let Some(prepared) = receipt.prepared.as_ref()
            && local == Some(prepared.0.as_str())
        {
            super::metadata::settle_selection_prepared_input(conn, pending, prepared)?;
        }
        let destination = manifest
            .decision
            .destinations
            .iter()
            .find(|d| {
                d.version_size == pending.asset.version_size.as_str()
                    && d.path.key().ok().as_deref() == Some(path.as_str())
            })
            .ok_or(StateError::ProviderSelectionInvalid)?;
        refresh_progress_hash(conn, &root, &manifest, destination)?;
    }
    let embedded: Option<String> = if changed > 0 && local.is_some() {
        let raw: Option<String> = conn.query_row("SELECT embedded_metadata_receipt FROM provider_active_destinations WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5 AND json_extract(embedded_metadata_receipt,'$.output_checksum')=?6 AND (json_extract(embedded_metadata_receipt,'$.published')=0 OR current_publication=1)",params![receipt.generation,receipt.pass_key,receipt.child,pending.asset.version_size.as_str(),path,local],|r|r.get(0)).optional()?.flatten();
        if let Some(raw) = raw {
            let mut evidence: crate::state::types::EmbeddedMetadataReceipt =
                serde_json::from_str(&raw)
                    .map_err(|_invalid| StateError::ProviderSelectionInvalid)?;
            evidence.published = true;
            let published = serde_json::to_string(&evidence)
                .map_err(|_invalid| StateError::ProviderSelectionInvalid)?;
            conn.execute("UPDATE provider_active_destinations SET embedded_metadata_receipt=?6 WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5",params![receipt.generation,receipt.pass_key,receipt.child,pending.asset.version_size.as_str(),path,published])?;
            Some(published)
        } else {
            None
        }
    } else {
        None
    };
    if changed > 0 && (complete || local != pending.asset.local_checksum.as_deref()) {
        // A single physical write can satisfy multiple passes only when every
        // frozen input and writer dependency is exactly compatible. Different
        // native paths always retain independent publication obligations.
        let mut statement=conn.prepare("SELECT generation,pass_key,child FROM provider_active_destinations WHERE library=?1 AND asset_id=?2 AND version_size=?3 AND path=?4 AND checksum=?5 AND metadata_hash IS ?6 AND created_at IS ?7 AND added_at IS ?8 AND admitted=1 AND (verified_media=1 OR preserved_local IS NOT NULL) AND verified_metadata=0 AND metadata_successor IS NULL AND local_checksum IS ?9 AND source_checksum IS ?10 AND grouping_hash=?11 AND size_bytes=?12 ORDER BY generation,pass_key,child")?;
        let aliases = statement.query_map(
            params![
                pending.asset.library,
                manifest.state_id,
                pending.asset.version_size.as_str(),
                path,
                pending.asset.checksum,
                pending.asset.metadata.metadata_hash,
                encode_asset_date(pending.asset.created_at),
                pending.asset.added_at.map(encode_asset_date),
                pending.asset.local_checksum,
                pending.source_checksum,
                receipt.grouping_hash,
                i64::try_from(pending.asset.size_bytes)
                    .map_err(|_invalid| StateError::ProviderSelectionInvalid)?
            ],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            },
        )?;
        for alias_key in aliases {
            let (generation, key, child) = alias_key?;
            let alias_root = load_generation(conn, &generation)?;
            if alias_root.spec.scope != root.spec.scope
                || alias_root.spec.metadata_flags != receipt.metadata_flags
            {
                continue;
            }
            let alias = load_decision(conn, &alias_root, &key, &child)?;
            if alias.decision.master != manifest.decision.master {
                continue;
            }
            let destination = alias
                .decision
                .destinations
                .iter()
                .find(|d| {
                    d.version_size == pending.asset.version_size.as_str()
                        && d.path.key().ok().as_deref() == Some(path.as_str())
                })
                .ok_or(StateError::ProviderSelectionInvalid)?;
            let preserved = preserved_local(conn, &alias_root, &alias, destination)?;
            match (preserved, &receipt.preserved_local) {
                (Some(mut evidence), Some(expected))
                    if evidence.checksum == expected.checksum
                        && evidence.size == expected.size
                        && evidence.modified == expected.modified =>
                {
                    if let Some(local) = local {
                        evidence.checksum = local.to_owned();
                        if let Some((output, size)) = &receipt.prepared
                            && output == local
                        {
                            evidence.size = *size;
                        }
                    }
                    conn.execute("UPDATE provider_active_destinations SET preserved_local=?6,local_checksum=?7,verified_metadata=?8,prepared_checksum=NULL,prepared_size=NULL,prepared_hash=NULL WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5",params![generation,key,child,destination.version_size,path,encode(&evidence)?,evidence.checksum,complete])?;
                }
                (None, None) => {
                    conn.execute("UPDATE provider_active_destinations SET prepared_checksum=NULL,prepared_size=NULL,prepared_hash=NULL,local_checksum=COALESCE(?6,local_checksum),verified_metadata=CASE WHEN ?10=1 THEN 1 ELSE verified_metadata END WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5 AND local_checksum IS ?7 AND source_checksum IS ?8 AND grouping_hash=?9 AND verified_media=1 AND verified_metadata=0",params![generation,key,child,pending.asset.version_size.as_str(),path,local,pending.asset.local_checksum,pending.source_checksum,receipt.grouping_hash,complete])?;
                }
                _ => continue,
            }
            if let Some(embedded) = &embedded {
                conn.execute("UPDATE provider_active_destinations SET embedded_metadata_receipt=?6 WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5 AND local_checksum=?7",params![generation,key,child,destination.version_size,path,embedded,local])?;
            }
            refresh_progress_hash(conn, &alias_root, &alias, destination)?;
        }
    }
    if changed > 0 && local.is_some() {
        replace_current_publication(
            conn,
            &pending.asset.library,
            &path,
            Some((
                receipt.generation.as_str(),
                receipt.pass_key.as_str(),
                receipt.child.as_str(),
                pending.asset.version_size.as_str(),
            )),
        )?;
        if let Some(embedded) = &embedded {
            // This receipt was measured by this completion, not borrowed from
            // the selected row's historical verification.
            conn.execute("UPDATE provider_active_destinations SET embedded_metadata_receipt=?6 WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5 AND local_checksum=?7",params![receipt.generation,receipt.pass_key,receipt.child,pending.asset.version_size.as_str(),path,embedded,local])?;
        }
    }
    Ok(Some(changed > 0 && complete))
}

/// Destination policy is separate from source receipt compatibility. Historical
/// receipts from another album remain sources, but cannot assign its path to a
/// new album copy. Validate the retained declaration before trusting ownership.
fn album_path_claimed(
    conn: &Connection,
    scope: &str,
    key: &str,
    path: &super::provider_selection::SelectionPath,
) -> Result<bool, StateError> {
    let mut statement = conn.prepare("SELECT DISTINCT w.generation,w.pass_key,w.child FROM provider_active_destinations w JOIN provider_active_generations g ON g.id=w.generation WHERE g.scope=?1 AND w.path=?2 AND w.pass_key<>?3 AND w.admitted=1 ORDER BY w.generation,w.pass_key,w.child LIMIT 33")?;
    let rows = statement.query_map(params![scope, path.key()?, key], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    for (index, row) in rows.enumerate() {
        if index == 32 {
            return Err(StateError::ProviderSelectionFull);
        }
        let (generation, previous_key, child) = row?;
        let root = load_generation(conn, &generation)?;
        if root.spec.mixed_favorites_replay()
            || matches!(
                pass(&root, &previous_key)?
                    .get("kind")
                    .and_then(Value::as_str),
                Some("Album" | "SmartFolder")
            )
        {
            load_decision(conn, &root, &previous_key, &child)?;
            // A committed/pending managed family keeps its own physical slot.
            // Legacy aliases remain historical source proof; another current
            // independent declaration still cannot claim this managed slot.
            if root.spec.profile.get(INDEPENDENT_ALBUM_COPIES) != Some(&Value::Bool(true))
                && super::primary_layout::path_owned_by_pass(conn, &path.to_path(), key)?
            {
                continue;
            }
            return Ok(true);
        }
    }
    Ok(false)
}

impl SqliteStateDb {
    pub(crate) async fn selection_album_path_claimed(
        &self,
        owner: account::AccountOwner,
        scope: String,
        key: String,
        path: super::provider_selection::SelectionPath,
    ) -> Result<bool, StateError> {
        self.with_conn(
            "checking independent album destination ownership",
            move |conn| {
                account::validate_authenticated(conn, &owner)?;
                album_path_claimed(conn, &scope, &key, &path)
            },
        )
        .await
    }

    /// Lossless paths from independently verified generations, limited before
    /// materialization. Current provider facts supplied by the caller decide
    /// version compatibility; generation order only schedules candidate reuse.
    /// Return each retained pass key so callers can distinguish its established
    /// destination from another pass's reusable copy after template changes.
    pub(crate) async fn verified_selection_paths(
        &self,
        owner: account::AccountOwner,
        scope: String,
        key: String,
        child: String,
        master: String,
        version: String,
        resource: (String, u64),
    ) -> Result<Vec<(String, super::provider_selection::SelectionPath)>, StateError> {
        let (checksum, size) = resource;
        self.with_conn("reading independent native publication paths",move|conn|{
            account::validate_authenticated(conn,&owner)?;
            let candidates:Vec<(String,String,String)>=conn.prepare("SELECT w.generation,w.path,w.pass_key FROM provider_active_destinations w JOIN provider_active_generations g ON g.id=w.generation WHERE g.scope=?1 AND w.child=?3 AND w.master=?4 AND w.version_size=?5 AND w.checksum=?6 AND w.size_bytes=?7 AND w.admitted=1 AND w.verified_media=1 ORDER BY (w.pass_key=?2) DESC,g.created_at DESC,w.path LIMIT 32")?.query_map(params![scope,key,child,master,version,checksum,i64::try_from(size).map_err(|_invalid|StateError::ProviderSelectionInvalid)?],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?.collect::<Result<_,_>>()?;
            let mut paths=Vec::with_capacity(candidates.len());
            for (generation,path,previous_key) in candidates {
                let root=load_generation(conn,&generation)?;load_decision(conn,&root,&previous_key,&child)?;
                paths.push((previous_key, serde_json::from_str(&path).map_err(|_invalid|StateError::ProviderSelectionInvalid)?));
            }
            Ok(paths)
        }).await
    }
}

#[derive(Debug)]
pub(crate) struct CompletedSelectionFile {
    pub(crate) pass_key: String,
    pub(crate) child: String,
    pub(crate) version: String,
    pub(crate) path: super::provider_selection::SelectionPath,
}

const COMPLETED_SELECTION_PAGE_SIZE: i64 = 256;

impl SqliteStateDb {
    pub(crate) async fn completed_selection_files(
        &self,
        owner: account::AccountOwner,
        generation: String,
        after: Option<(String, String, String, String)>,
    ) -> Result<Vec<CompletedSelectionFile>, StateError> {
        self.with_conn("reading completed selection files", move |conn| {
            account::validate_authenticated(conn, &owner)?;
            load_generation(conn, &generation)?;
            let mut statement = conn.prepare("SELECT pass_key,child,version_size,path FROM provider_active_destinations WHERE generation=?1 AND admitted=1 AND (verified_media=1 OR preserved_local IS NOT NULL) AND (?2 IS NULL OR (pass_key,child,version_size,path)>(?2,?3,?4,?5)) ORDER BY pass_key,child,version_size,path LIMIT ?6")?;
            let rows = statement.query_map(params![generation, after.as_ref().map(|v| &v.0), after.as_ref().map(|v| &v.1), after.as_ref().map(|v| &v.2), after.as_ref().map(|v| &v.3), COMPLETED_SELECTION_PAGE_SIZE], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, String>(3)?))
            })?;
            rows.map(|row| {
                let (pass_key, child, version, path) = row?;
                Ok(CompletedSelectionFile { pass_key, child, version,
                    path: serde_json::from_str(&path).map_err(|_invalid| StateError::ProviderSelectionInvalid)?,
                })
            }).collect()
        }).await
    }

    pub(crate) async fn reopen_missing_selection_file(
        &self,
        owner: account::AccountOwner,
        generation: String,
        file: CompletedSelectionFile,
    ) -> Result<(), StateError> {
        self.with_conn_mut("reopening missing selection file", move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            account::validate_authenticated(&tx, &owner)?;
            let root = load_generation(&tx, &generation)?;
            let manifest = load_decision(&tx, &root, &file.pass_key, &file.child)?;
            if !manifest.decision.destinations.iter().any(|destination| destination.path == file.path && destination.version_size == file.version) {
                return invalid();
            }
            // A committed directory move preserves historical completion while
            // current selection verifies successors. Never revive its old slot.
            let operations: Vec<String> = tx.prepare("SELECT operation FROM primary_layout_operations WHERE policy='album-directory-v1' AND phase='committed'")?.query_map([], |row| row.get(0))?.collect::<Result<_,_>>()?;
            for operation in operations {
                let (op, _) = super::primary_layout::directory::load_operation(&tx, &operation)?;
                if op.old.pass != file.pass_key || album_pass_scope(&root, &file.pass_key) != Some(&op.old.scope) { continue; }
                let mut statement = tx.prepare("SELECT evidence FROM primary_layout_members WHERE operation=?1")?;
                for row in statement.query_map([operation], |row| row.get::<_,Vec<u8>>(0))? {
                    let member: super::primary_layout::directory::AlbumDirectoryFile = super::primary_layout::decode(&row?)?;
                    if member.old == file.path && member.child == manifest.state_id && member.version == file.version
                        && super::primary_layout::directory::relocated_path(&op, &member.old)? == member.next {
                        return Ok(());
                    }
                }
            }
            let zone = manifest.media_zone(&root);
            let library = zone.get("zoneName").and_then(Value::as_str).ok_or(StateError::ProviderSelectionInvalid)?;
            replace_current_publication(&tx, library, &file.path.key()?, None)?;
            // Keep catalog/path receipts and provider retry timing. Only the
            // active completion proof for the absent copy becomes unfinished.
            tx.execute("UPDATE provider_active_destinations SET confirmed_repair=NULL,preserved_local=NULL,verified_media=0,verified_metadata=0,local_checksum=NULL,source_checksum=NULL,progress_hash=NULL,prepared_checksum=NULL,prepared_size=NULL,prepared_hash=NULL WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5 AND admitted=1 AND (verified_media=1 OR preserved_local IS NOT NULL)", params![generation, file.pass_key, file.child, file.version, file.path.key()?])?;
            tx.commit()?;
            Ok(())
        }).await
    }

    pub(crate) async fn selection_path_matches_local(
        &self,
        owner: account::AccountOwner,
        scope: String,
        key: String,
        identity: (String, String),
        path: super::provider_selection::SelectionPath,
        fingerprint: (String, u64),
    ) -> Result<bool, StateError> {
        let (child, library) = identity;
        let (local, size) = fingerprint;
        self.with_conn("checking independent publication fingerprint",move|conn|{
            account::validate_authenticated(conn,&owner)?;
            // Only actual publication or checked metadata completion advances
            // this lossless authority. History and generation dates never do.
            let previous:Option<(String,String)>=conn.query_row("SELECT w.generation,w.pass_key FROM provider_active_destinations w JOIN provider_active_generations g ON g.id=w.generation WHERE g.scope=?1 AND w.child=?3 AND w.path=?4 AND w.local_checksum=?5 AND w.library=?6 AND w.current_publication=1 AND w.admitted=1 AND w.verified_media=1 AND w.preserved_local IS NULL ORDER BY (w.pass_key=?2) DESC,w.generation,w.pass_key LIMIT 1",params![scope,key,child,path.key()?,local,library],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
            if let Some((generation,previous_key))=previous {let root=load_generation(conn,&generation)?;load_decision(conn,&root,&previous_key,&child)?;Ok(true)} else {
                Ok(directory_publication_source(conn,&scope,&key,&child,&library,&path,(&local,size))?.is_some())
            }
        }).await
    }
}

pub(crate) struct SelectionCheckpointHold {
    pub(crate) blocker: &'static str,
    pub(crate) retained_generation: bool,
}

pub(crate) enum SelectionDebt {
    IndependentLookup,
    Blocking(SelectionCheckpointHold),
}

fn independently_retryable_lookup(
    conn: &Connection,
    old: &ActiveGeneration,
    latest: &ActiveGeneration,
    key: &str,
    child: &str,
) -> Result<bool, StateError> {
    // recover_retained can add a confirmation only to an unsealed declaration.
    // A different scan must already have proved its own complete query coverage.
    // The current metadata writer also needs a destination in its own queue.
    if old.sealed
        || !latest.sealed
        || !latest.checkpoint_ready
        || latest.checkpoint_veto.is_some()
        || old.spec.profile != latest.spec.profile
        || old.spec.zone != latest.spec.zone
        || old.spec.metadata_enabled
        || latest.spec.metadata_enabled
    {
        return Ok(false);
    }
    // A conflicting later observation (including a changed rendition set)
    // cannot be excused as an independent retry of the old version.
    if conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM provider_active_generations g
         CROSS JOIN json_each(CAST(g.specification AS TEXT),'$.profile.passes') p
         CROSS JOIN provider_active_decisions d
         WHERE g.scope=?1 AND g.config_hash=?2 AND g.id<>?3
         AND g.created_at>=(SELECT created_at FROM provider_active_generations WHERE id=?3)
         AND (d.generation,d.pass_key,d.child)=(g.id,json_extract(p.value,'$.key'),?4))",
        params![old.spec.scope, old.spec.config_hash, old.id, child],
        |r| r.get(0),
    )? {
        return Ok(false);
    }
    let manifest = load_decision(conn, old, key, child)?;
    if manifest.decision.reason != "current_lookup_unresolved"
        || manifest.decision.confirmation.is_some()
        || !manifest.decision.destinations.is_empty()
        || !manifest
            .decision
            .media_source
            .as_ref()
            .is_some_and(|s| s.scope == old.spec.scope && s.zone == old.spec.zone)
    {
        return Ok(false);
    }
    let asset = observed_asset_matching(
        conn,
        old,
        &manifest,
        crate::icloud::photos::PhotoAsset::same_download_observation,
    )?;
    Ok(asset.is_some_and(|a| !a.versions().is_empty() && a.malformed_resources().is_empty()))
}

impl SelectionCheckpointHold {
    fn destination(row: &rusqlite::Row<'_>, column: usize) -> rusqlite::Result<Self> {
        let blocker = if row.get::<_, bool>(column)? {
            "prepared_publication"
        } else if row.get::<_, bool>(column + 1)? {
            "confirmed_repair"
        } else if row.get::<_, bool>(column + 2)? {
            "unverified_media"
        } else {
            "unfinished_metadata"
        };
        Ok(Self {
            blocker,
            retained_generation: row.get(column + 3)?,
        })
    }
}

impl SqliteStateDb {
    /// Include unfinished current-configuration obligations, even when a lookup
    /// can be retried without the old cursor.
    /// Other configurations retain physical publication guards, but uncertainty
    /// alone cannot hold a successor they cannot recover through current selection.
    /// Exact completed current-selection proof can supersede an unstarted old
    /// path without acknowledging or modifying its retained publication evidence.
    pub(crate) async fn unfinished_selection_debt(
        &self,
        owner: account::AccountOwner,
        scope: String,
        config_hash: String,
    ) -> Result<bool, StateError> {
        Ok(self
            .selection_debt(owner, scope, config_hash, false, None)
            .await?
            .is_some())
    }

    /// Does not include metadata that the post-checkpoint writer can recover
    /// from the selected SQLite receipts alone.
    #[cfg(test)]
    pub(crate) async fn provider_selection_debt(
        &self,
        owner: account::AccountOwner,
        scope: String,
        config_hash: String,
    ) -> Result<bool, StateError> {
        Ok(matches!(
            self.selection_debt(owner, scope, config_hash, true, None)
                .await?,
            Some(SelectionDebt::Blocking(_))
        ))
    }

    /// Local recovery is bound to the last inventory for this exact plan.
    /// Advancing a provider cursor must not make its writer unreachable.
    pub(crate) async fn local_writer_generation(
        &self,
        owner: account::AccountOwner,
        scope: String,
        config_hash: String,
    ) -> Result<Option<ActiveGeneration>, StateError> {
        self.with_conn("reading local writer inventory", move |conn| {
            let tx = conn.unchecked_transaction()?;
            account::validate_authenticated(&tx, &owner)?;
            let id: Option<String> = tx
                .query_row(LATEST_LOCAL_GENERATION, params![scope, config_hash], |r| {
                    r.get(0)
                })
                .optional()?;
            id.map(|id| load_generation(&tx, &id)).transpose()
        })
        .await
    }

    /// Return the first checkpoint blocker in preference to independent work.
    pub(crate) async fn selection_debt(
        &self,
        owner: account::AccountOwner,
        scope: String,
        config_hash: String,
        local_writer_available: bool,
        replacement: Option<String>,
    ) -> Result<Option<SelectionDebt>, StateError> {
        self.with_conn("inspecting retained selection debt",move|conn|{
            let tx=conn.unchecked_transaction()?;
            account::validate_authenticated(&tx,&owner)?;
            let conn=&tx;
            if let Some(id) = &replacement {
                let latest: Option<String> = conn.query_row(LATEST_LOCAL_GENERATION, params![scope,config_hash], |r| r.get(0)).optional()?;
                let root = load_generation(conn, id)?;
                if latest.as_deref() != Some(id.as_str()) || !root.sealed || !root.checkpoint_ready {
                    return invalid();
                }
            }
            validate_scope_decisions(conn, &scope, replacement.as_deref().map(|id| (config_hash.as_str(),id)))?;
            // A prepared output can already be on disk while its receipt is
            // unfinished. Preserve its fence at an overlapping selected path,
            // even if current writer options differ. Historical planned paths
            // without such an artifact do not create current selection.
            // A later complete inventory accounts for its own selected repair
            // destinations. Keep historical confirmations for future matching
            // selections without charging them to the current checkpoint.
            let retained = format!("g.id IS NOT ({LATEST_LOCAL_GENERATION})");
            let eligible = format!("g.id=?3 OR (?3 IS NULL AND NOT ({SUPERSEDED_SELECTION}))");
            let mut independent = None;
            let mut latest = None;
            {
                let mut stmt=conn.prepare(&format!("SELECT d.reason='current_lookup_unresolved', {retained},g.id,d.pass_key,d.child FROM provider_active_decisions d JOIN provider_active_generations g ON g.id=d.generation WHERE {CURRENT_SELECTION_CONFIGURATION} AND ({eligible}) AND d.admission='deferred'"))?;
                let mut rows=stmt.query(params![scope,config_hash,replacement])?;
                while let Some(row)=rows.next()? {
                    let lookup=row.get::<_,bool>(0)?;
                    let retained_generation=row.get(1)?;
                    if lookup && retained_generation {
                        let root=load_generation(conn,&row.get::<_,String>(2)?)?;
                        let key:String=row.get(3)?;
                        let child:String=row.get(4)?;
                        if lookup_has_verified_successor(conn,&root,&key,&child)? {continue}
                        let latest = match &latest {
                            Some(root) => root,
                            None => {
                                let id:String=conn.query_row(LATEST_LOCAL_GENERATION,params![scope,config_hash],|r|r.get(0))?;
                                let root=load_generation(conn,&id)?;
                                if root.spec.basis!=basis(conn,&scope)? {
                                    return Ok(Some(SelectionDebt::Blocking(SelectionCheckpointHold {
                                        blocker:"deferred_identity", retained_generation,
                                    })));
                                }
                                latest.insert(root)
                            }
                        };
                        match independently_retryable_lookup(conn,&root,latest,&key,&child) {
                            Ok(true) => {
                                independent=Some(SelectionDebt::IndependentLookup);
                                continue
                            }
                            Ok(false) | Err(StateError::ProviderSelectionFull) => {}
                            Err(error) => return Err(error),
                        }
                    }
                    return Ok(Some(SelectionDebt::Blocking(SelectionCheckpointHold {
                        blocker: if lookup {"deferred_identity"} else {"deferred_admission"},
                        retained_generation,
                    })))
                }
            }
            let debt=format!("FROM provider_active_destinations w JOIN provider_active_generations g ON g.id=w.generation WHERE g.scope=?1 AND ((g.config_hash=?2 AND (({eligible}) OR w.prepared_hash IS NOT NULL)) OR (w.prepared_hash IS NOT NULL AND EXISTS(SELECT 1 FROM provider_active_destinations selected JOIN provider_active_generations current ON current.id=selected.generation WHERE current.scope=?1 AND current.config_hash=?2 AND selected.admitted=1 AND selected.path=w.path))) AND {UNFINISHED}
                AND NOT (w.confirmed_repair IS NOT NULL AND w.prepared_hash IS NULL AND EXISTS(SELECT 1 FROM provider_active_generations newer WHERE newer.scope=g.scope AND newer.config_hash=g.config_hash AND newer.created_at>g.created_at AND newer.sealed=1 AND newer.checkpoint_ready=1))");
            let destination_fields = format!("w.prepared_hash IS NOT NULL, w.confirmed_repair IS NOT NULL, (w.verified_media=0 AND w.preserved_local IS NULL), {retained}");
            let unfinished = conn.query_row(&format!("SELECT {destination_fields} {debt} LIMIT 1"),params![scope,config_hash,replacement],|r|SelectionCheckpointHold::destination(r,0)).optional()?;
            if unfinished.is_none() || !local_writer_available {return Ok(unfinished.map(SelectionDebt::Blocking).or(independent))}
            // Refresh the local preservation cache before opening the destination
            // cursor: rebuilding its TEMP schema would invalidate a live cursor.
            let protected=super::legacy_preservation::protected_paths(conn)?.into_iter()
                .map(|path|crate::fs_util::confined_path_key(&path).map_err(|_invalid_path|StateError::ProviderSelectionInvalid))
                .collect::<Result<HashSet<_>,_>>()?;
            // The local writer reads the latest generation and can finish
            // compatible aliases. Don't excuse a historical-only path, a
            // lost catalogue input, an outstanding repair, or missing media.
            let mut stmt=conn.prepare(&format!("SELECT DISTINCT w.library,w.asset_id,CASE WHEN
                g.config_hash=?2 AND (w.verified_media=1 OR w.preserved_local IS NOT NULL) AND w.confirmed_repair IS NULL
                AND w.intent_hash IS NOT NULL AND w.local_checksum IS NOT NULL
                AND EXISTS(SELECT 1 FROM assets a WHERE a.library=w.library AND a.id=w.asset_id
                    AND a.version_size=w.version_size AND a.checksum=w.checksum AND a.is_deleted=0
                    AND a.metadata_hash=w.metadata_hash AND a.created_at IS w.created_at AND a.added_at IS w.added_at)
                AND EXISTS(SELECT 1 FROM provider_active_destinations n JOIN provider_active_generations newest ON newest.id=n.generation
                    WHERE n.generation=({LATEST_LOCAL_GENERATION}) AND newest.sealed=1 AND newest.checkpoint_ready=1
                    AND n.pass_key=w.pass_key AND n.child=w.child AND n.version_size=w.version_size AND n.path=w.path
                    AND n.library=w.library AND n.asset_id=w.asset_id AND n.master=w.master AND n.checksum=w.checksum
                    AND n.size_bytes=w.size_bytes AND n.metadata_hash=w.metadata_hash AND n.added_at IS w.added_at AND n.created_at IS w.created_at
                    AND n.admitted=1 AND (n.verified_media=1 OR n.preserved_local IS NOT NULL) AND n.verified_metadata=0 AND n.metadata_successor IS NULL
                    AND n.confirmed_repair IS NULL AND n.preserved_local IS w.preserved_local AND n.grouping_hash=w.grouping_hash
                    AND n.local_checksum=w.local_checksum AND n.source_checksum IS w.source_checksum
                    AND (w.generation=n.generation OR (w.prepared_hash IS NULL AND n.prepared_hash IS NULL)))
                THEN w.grouping_hash END,w.path,{destination_fields} {debt}"))?;
            let mut rows=stmt.query(params![scope,config_hash,replacement])?;
            while let Some(r)=rows.next()? {
                let blocked=|| Ok(Some(SelectionDebt::Blocking(SelectionCheckpointHold::destination(r,4)?)));
                let Some(group)=r.get::<_,Option<String>>(2)? else {return blocked()};
                let path:super::provider_selection::SelectionPath=serde_json::from_str(&r.get::<_,String>(3)?)
                    .map_err(|_invalid|StateError::ProviderSelectionInvalid)?;
                let path=path.to_path();
                if super::primary_layout::path_is_managed(conn,&path)? {return blocked()}
                if !protected.is_empty() {
                    let mut sidecar=path.as_os_str().to_owned();
                    sidecar.push(".xmp");
                    for p in [path.as_path(),Path::new(&sidecar)] {
                        if protected.contains(&crate::fs_util::confined_path_key(p).map_err(|_invalid_path|StateError::ProviderSelectionInvalid)?) {return blocked()}
                    }
                }
                if grouping_hash(conn,&r.get::<_,String>(0)?,&r.get::<_,String>(1)?)?!=group {return blocked()}
            }
            drop(rows);
            drop(stmt);
            tx.commit()?;
            Ok(independent)
        }).await
    }
}

const LATEST_LOCAL_GENERATION: &str = "SELECT id FROM provider_active_generations WHERE scope=?1 AND config_hash=?2 ORDER BY created_at DESC,id DESC LIMIT 1";

impl SqliteStateDb {
    /// Established destination ownership is independent of current resource proof.
    /// This is never a reusable source candidate for another destination.
    pub(crate) async fn established_selection_path(
        &self,
        owner: account::AccountOwner,
        scope: String,
        key: String,
        child: String,
        master: String,
        version: String,
    ) -> Result<Option<super::provider_selection::SelectionPath>, StateError> {
        self.with_conn("reading established local destination", move |conn| {
            account::validate_authenticated(conn, &owner)?;
            let mut landed = None;
            for (operation, _, file) in pending_directory_publications(conn, None)? {
                let native = file.native.as_ref().ok_or(StateError::ProviderSelectionInvalid)?;
                if native.child != child || file.version != version { continue; }
                let (op, _) = super::primary_layout::directory::load_operation(conn, &operation)?;
                if op.next.pass != key { continue; }
                let root = load_generation(conn, &native.generation)?;
                if root.spec.scope != scope { continue; }
                let manifest = load_decision(conn, &root, &native.pass, &native.child)?;
                if manifest.decision.master.as_deref() != Some(master.as_str()) { continue; }
                if landed.as_ref().is_some_and(|path| *path != file.next) { return invalid(); }
                landed = Some(file.next);
            }
            if landed.is_some() { return Ok(landed); }
            let previous: Option<(String, String)> = conn.query_row(
                "SELECT w.generation,w.path FROM provider_active_destinations w JOIN provider_active_generations g ON g.id=w.generation WHERE g.scope=?1 AND w.pass_key=?2 AND w.child=?3 AND w.version_size=?4 AND w.master=?5 AND w.admitted=1 AND (w.verified_media=1 OR w.preserved_local IS NOT NULL) ORDER BY g.created_at DESC LIMIT 1",
                params![scope,key,child,version,master], |row| Ok((row.get(0)?,row.get(1)?)),
            ).optional()?;
            let Some((generation,path)) = previous else { return Ok(None) };
            let root = load_generation(conn, &generation)?;
            load_decision(conn, &root, &key, &child)?;
            Ok(Some(serde_json::from_str(&path).map_err(|_invalid| StateError::ProviderSelectionInvalid)?))
        }).await
    }

    /// Resolve only this copy by local preservation, never by provider verification.
    pub(crate) async fn preserve_selection_local(
        &self,
        owner: account::AccountOwner,
        generation: String,
        key: String,
        child: String,
        path: super::provider_selection::SelectionPath,
        observation: (String, u64, Option<chrono::DateTime<Utc>>),
        file_still_matches: impl Fn() -> bool + Send + 'static,
    ) -> Result<(), StateError> {
        self.with_conn_mut("preserving selected local media", move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            account::validate_authenticated(&tx, &owner)?;
            let root = load_generation(&tx, &generation)?;
            let manifest = load_decision(&tx, &root, &key, &child)?;
            let destination = manifest.decision.destinations.iter().find(|d| d.path == path)
                .ok_or(StateError::ProviderSelectionInvalid)?;
            let zone = manifest.media_zone(&root);
            let library = zone.get("zoneName").and_then(Value::as_str).ok_or(StateError::ProviderSelectionInvalid)?;
            // Re-observing unchanged preserved bytes keeps their latest checked
            // metadata owner. A changed file revokes that authority; selection
            // itself never creates a new current publication.
            let unchanged_preserved: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM provider_active_destinations WHERE library=?1 AND path=?2 AND current_publication=1 AND preserved_local IS NOT NULL AND local_checksum=?3)", params![library,path.key()?,observation.0], |row| row.get(0))?;
            if !unchanged_preserved {
                replace_current_publication(&tx, library, &path.key()?, None)?;
            }
            let evidence = PreservedLocal {
                intent: preservation_intent(&root, &manifest, destination)?,
                checksum: observation.0, size: observation.1, modified: observation.2,
            };
            if let Some(previous) = preserved_local(&tx, &root, &manifest, destination)?
                && (previous == evidence || (previous.modified == evidence.modified
                    && prepared_metadata(&tx, &root, &manifest, destination)? == Some((evidence.checksum.clone(), evidence.size))))
            {
                commit_observed_destination(tx, &file_still_matches)?;
                return Ok(());
            }
            if evidence.checksum.len() != 64 || !evidence.checksum.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return invalid();
            }
            tx.execute("UPDATE provider_active_destinations SET preserved_local=?6,verified_media=0,verified_metadata=0,local_checksum=?7,source_checksum=NULL,prepared_checksum=NULL,prepared_size=NULL,prepared_hash=NULL WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5 AND admitted=1",
                params![generation,key,child,destination.version_size,path.key()?,encode(&evidence)?,evidence.checksum])?;
            refresh_progress_hash(&tx, &root, &manifest, destination)?;
            commit_observed_destination(tx, &file_still_matches)
        }).await
    }

    pub(crate) async fn selection_destination_resolved(
        &self,
        owner: account::AccountOwner,
        generation: String,
        key: String,
        child: String,
        version: String,
    ) -> Result<bool, StateError> {
        self.with_conn("checking independent destination completion",move|conn|{
            account::validate_authenticated(conn,&owner)?;let root=load_generation(conn,&generation)?;load_decision(conn,&root,&key,&child)?;
            Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM provider_active_destinations WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND admitted=1 AND (verified_media=1 OR preserved_local IS NOT NULL))",params![generation,key,child,version],|r|r.get(0))?)
        }).await
    }
}

/// A single admitted physical destination selected by explicit integrity inspection.
#[derive(Clone, Debug)]
pub(crate) struct IntegrityDestination {
    generation: String,
    pass_key: String,
    child: String,
    destination: SelectionDestination,
    pub(crate) local_checksum: Option<String>,
    pub(crate) preserved: bool,
    verified_media: bool,
    /// The currently bound explicit confirmation for these bytes, if any.
    pub(crate) confirmed: Option<(String, u64)>,
    /// Exact stored confirmation used to compare-and-swap an explicit replacement.
    previous_confirmation: Option<Vec<u8>>,
}

type IntegrityInspectionRow = (
    String,
    String,
    String,
    String,
    Option<String>,
    bool,
    bool,
    Option<Vec<u8>>,
);

type StoredConfirmation = (Vec<u8>, String, u64);

#[derive(serde::Serialize, serde::Deserialize)]
struct ConfirmedRepair {
    checksum: String,
    size: u64,
    binding: String,
}

fn repair_binding(
    root: &ActiveGeneration,
    manifest: &ActiveDecision,
    destination: &SelectionDestination,
    checksum: &str,
    size: u64,
) -> Result<String, StateError> {
    Ok(digest(&encode(&(
        "confirmed_integrity_repair_v1",
        &root.id,
        &root.spec,
        manifest,
        destination,
        checksum,
        size,
    ))?))
}

fn parse_confirmed_repair(
    bytes: &[u8],
    root: &ActiveGeneration,
    manifest: &ActiveDecision,
    destination: &SelectionDestination,
) -> Result<(String, u64), StateError> {
    if bytes.len() > 1024 {
        return invalid();
    }
    let repair: ConfirmedRepair =
        serde_json::from_slice(bytes).map_err(|_invalid| StateError::ProviderSelectionInvalid)?;
    if repair.checksum.len() != 64
        || !repair.checksum.bytes().all(|byte| byte.is_ascii_hexdigit())
        || repair.binding
            != repair_binding(root, manifest, destination, &repair.checksum, repair.size)?
    {
        return invalid();
    }
    Ok((repair.checksum, repair.size))
}

fn loaded_confirmed_repair(
    conn: &Connection,
    root: &ActiveGeneration,
    manifest: &ActiveDecision,
    destination: &SelectionDestination,
) -> Result<Option<StoredConfirmation>, StateError> {
    let bytes: Option<Vec<u8>> = conn.query_row(
        "SELECT confirmed_repair FROM provider_active_destinations WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5 AND (confirmed_repair IS NULL OR length(confirmed_repair)<=1024)",
        params![root.id, manifest.decision.pass_key, manifest.decision.child, destination.version_size, destination.path.key()?],
        |row| row.get(0),
    )?;
    let Some(bytes) = bytes else { return Ok(None) };
    let (checksum, size) = parse_confirmed_repair(&bytes, root, manifest, destination)?;
    Ok(Some((bytes, checksum, size)))
}

fn confirmed_repair(
    conn: &Connection,
    root: &ActiveGeneration,
    manifest: &ActiveDecision,
    destination: &SelectionDestination,
) -> Result<Option<(String, u64)>, StateError> {
    Ok(loaded_confirmed_repair(conn, root, manifest, destination)?
        .map(|(_, checksum, size)| (checksum, size)))
}

impl SqliteStateDb {
    /// Resolve a current, independently owned path without mutating its receipts.
    pub(crate) async fn inspect_integrity_destination(
        &self,
        path: std::path::PathBuf,
    ) -> Result<Option<IntegrityDestination>, StateError> {
        self.with_conn("inspecting explicit integrity destination", move |conn| {
            let path = super::provider_selection::SelectionPath::from_path(&path).key()?;
            let row: Option<IntegrityInspectionRow> = conn.query_row(
                "SELECT w.generation,w.pass_key,w.child,w.version_size,w.local_checksum,w.preserved_local IS NOT NULL,w.verified_media,w.confirmed_repair FROM provider_active_destinations w JOIN provider_active_generations g ON g.id=w.generation WHERE w.path=?1 AND w.admitted=1 AND (w.verified_media=1 OR w.preserved_local IS NOT NULL OR w.confirmed_repair IS NOT NULL) AND NOT EXISTS(SELECT 1 FROM provider_active_generations newer WHERE newer.scope=g.scope AND newer.created_at>g.created_at) ORDER BY g.created_at DESC,w.current_publication DESC,w.generation,w.pass_key LIMIT 1",
                [&path], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?,row.get(6)?,row.get(7)?)),
            ).optional()?;
            let Some((generation, pass_key, child, version, local_checksum, preserved, verified_media, previous_confirmation)) = row else { return Ok(None) };
            let root = load_generation(conn, &generation)?;
            let manifest = load_decision(conn, &root, &pass_key, &child)?;
            let destination = manifest.decision.destinations.iter()
                .find(|destination| destination.version_size == version && destination.path.key().ok().as_deref() == Some(path.as_str()))
                .ok_or(StateError::ProviderSelectionInvalid)?.clone();
            let confirmed = previous_confirmation.as_deref()
                .map(|bytes| parse_confirmed_repair(bytes, &root, &manifest, &destination))
                .transpose()?;
            Ok(Some(IntegrityDestination { generation, pass_key, child, destination, local_checksum, preserved, verified_media, confirmed, previous_confirmation }))
        }).await
    }

    /// Planning may precede a reseeded manifest. Retain explicit intent only
    /// for the same selected provider resource and exact independently owned copy.
    pub(crate) async fn retained_integrity_confirmation(
        &self,
        owner: account::AccountOwner,
        scope: String,
        key: String,
        identity: (String, String),
        resource: (
            super::provider_selection::SelectionPath,
            String,
            String,
            u64,
        ),
    ) -> Result<Option<(String, u64)>, StateError> {
        self.with_conn("reading retained integrity confirmation", move |conn| {
            account::validate_authenticated(conn, &owner)?;
            let (child, master) = identity;
            let (path, version, checksum, size) = resource;
            let generation: Option<String> = conn.query_row(
                &format!("SELECT w.generation FROM provider_active_destinations w JOIN provider_active_generations g ON g.id=w.generation WHERE g.scope=?1 AND w.pass_key=?2 AND w.child=?3 AND w.master=?4 AND w.path=?5 AND w.version_size=?6 AND w.checksum=?7 AND w.size_bytes=?8 AND w.admitted=1 AND w.verified_media=0 AND w.confirmed_repair IS NOT NULL AND {UNFINISHED} ORDER BY g.created_at DESC LIMIT 1"),
                params![scope,key,child,master,path.key()?,version,checksum,i64::try_from(size).map_err(|_invalid| StateError::ProviderSelectionInvalid)?],
                |row| row.get(0),
            ).optional()?;
            let Some(generation) = generation else { return Ok(None) };
            let root = load_generation(conn, &generation)?;
            let manifest = load_decision(conn, &root, &key, &child)?;
            let destination = manifest.decision.destinations.iter().find(|destination| destination.path == path && destination.version_size == version).ok_or(StateError::ProviderSelectionInvalid)?;
            confirmed_repair(conn, &root, &manifest, destination)
        }).await
    }

    /// Reopen only the inspected destination. The old publication receipts remain
    /// historical evidence; neither the fingerprint nor confirmation is cloud proof.
    pub(crate) async fn confirm_integrity_damage(
        &self,
        inspected: IntegrityDestination,
        fingerprint: (String, u64),
    ) -> Result<(), StateError> {
        self.with_conn_mut("confirming explicit integrity damage", move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let root = load_generation(&tx, &inspected.generation)?;
            let manifest = load_decision(&tx, &root, &inspected.pass_key, &inspected.child)?;
            let destination = manifest.decision.destinations.iter()
                .find(|destination| destination.version_size == inspected.destination.version_size && destination.path == inspected.destination.path)
                .ok_or(StateError::ProviderSelectionInvalid)?;
            // An explicit confirmation may replace an already pending one only
            // while that exact bound confirmation is still present.
            let existing = loaded_confirmed_repair(&tx, &root, &manifest, destination)?;
            if existing
                .as_ref()
                .map(|(bytes, _, _)| bytes)
                != inspected.previous_confirmation.as_ref()
            {
                return invalid();
            }
            let repair = ConfirmedRepair {
                binding: repair_binding(&root, &manifest, destination, &fingerprint.0, fingerprint.1)?,
                checksum: fingerprint.0,
                size: fingerprint.1,
            };
            if !inspected.preserved
                && inspected.local_checksum.as_deref() == Some(repair.checksum.as_str())
            {
                return invalid();
            }
            let changed = tx.execute(
                "UPDATE provider_active_destinations SET confirmed_repair=?6,current_publication=0,preserved_local=NULL,verified_media=0,verified_metadata=0,local_checksum=NULL,source_checksum=NULL,progress_hash=NULL,prepared_checksum=NULL,prepared_size=NULL,prepared_hash=NULL WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5 AND admitted=1 AND verified_media=?9 AND (preserved_local IS NOT NULL)=?8 AND local_checksum IS ?7 AND confirmed_repair IS ?10 AND prepared_hash IS NULL AND NOT EXISTS(SELECT 1 FROM provider_active_generations g JOIN provider_active_generations newer ON newer.scope=g.scope AND newer.created_at>g.created_at WHERE g.id=?1)",
                params![root.id, inspected.pass_key, inspected.child, destination.version_size, destination.path.key()?, encode(&repair)?, inspected.local_checksum, inspected.preserved, inspected.verified_media, inspected.previous_confirmation],
            )?;
            if changed != 1 { return invalid() }
            tx.execute("UPDATE provider_active_decisions SET next_retry_at=0 WHERE generation=?1 AND pass_key=?2 AND child=?3", params![root.id, inspected.pass_key, inspected.child])?;
            refresh_admission_hash(&tx, &root, &inspected.pass_key, &inspected.child)?;
            tx.commit()?;
            Ok(())
        }).await
    }

    pub(crate) async fn selection_confirmed_repair(
        &self,
        owner: account::AccountOwner,
        generation: String,
        key: String,
        child: String,
        path: super::provider_selection::SelectionPath,
    ) -> Result<Option<(String, u64)>, StateError> {
        self.with_conn_mut("binding confirmed destination repair", move |connection| {
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let conn = &tx;
            account::validate_authenticated(conn, &owner)?;
            let root = load_generation(conn, &generation)?;
            let manifest = load_decision(conn, &root, &key, &child)?;
            let destination = manifest.decision.destinations.iter().find(|destination| destination.path == path)
                .ok_or(StateError::ProviderSelectionInvalid)?;
            let complete: bool = conn.query_row("SELECT verified_media FROM provider_active_destinations WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5", params![generation,key,child,destination.version_size,path.key()?], |row| row.get(0))?;
            if complete { return Ok(None) }
            if let Some(repair) = confirmed_repair(conn, &root, &manifest, destination)? {
                return Ok(Some(repair));
            }
            // A reseed may change enumeration/configuration evidence without
            // changing this physical copy or provider resource. Retain its
            // explicit intent through the existing historical destination owner.
            let previous: Option<String> = conn.query_row(
                &format!("SELECT w.generation FROM provider_active_destinations w JOIN provider_active_generations g ON g.id=w.generation WHERE g.scope=?1 AND w.pass_key=?2 AND w.child=?3 AND w.master=?4 AND w.version_size=?5 AND w.path=?6 AND w.checksum=?7 AND w.size_bytes=?8 AND w.admitted=1 AND w.verified_media=0 AND w.confirmed_repair IS NOT NULL AND {UNFINISHED} ORDER BY g.created_at DESC LIMIT 1"),
                params![root.spec.scope,key,child,manifest.decision.master,destination.version_size,path.key()?,destination.checksum,i64::try_from(destination.size).map_err(|_invalid| StateError::ProviderSelectionInvalid)?],
                |row| row.get(0),
            ).optional()?;
            let Some(previous) = previous else { return Ok(None) };
            let previous_root = load_generation(conn, &previous)?;
            let previous_manifest = load_decision(conn, &previous_root, &key, &child)?;
            let previous_destination = previous_manifest.decision.destinations.iter()
                .find(|previous| previous.path == path && previous.version_size == destination.version_size)
                .ok_or(StateError::ProviderSelectionInvalid)?;
            let inherited = confirmed_repair(conn, &previous_root, &previous_manifest, previous_destination)?;
            if let Some((checksum,size)) = &inherited {
                let repair = ConfirmedRepair { checksum:checksum.clone(), size:*size, binding:repair_binding(&root,&manifest,destination,checksum,*size)? };
                conn.execute("UPDATE provider_active_destinations SET confirmed_repair=?6 WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5 AND admitted=1 AND verified_media=0 AND confirmed_repair IS NULL AND NOT EXISTS(SELECT 1 FROM provider_active_generations newer WHERE newer.scope=?7 AND newer.created_at>(SELECT created_at FROM provider_active_generations WHERE id=?1))",params![generation,key,child,destination.version_size,path.key()?,encode(&repair)?,root.spec.scope])?;
            }
            tx.commit()?;
            Ok(inherited)
        }).await
    }
}

impl SqliteStateDb {
    pub(crate) async fn record_selection_metadata_prepared(
        &self,
        pending: &super::PendingMetadataRewrite,
        output_checksum: &str,
        output_size: u64,
    ) -> Result<Option<super::SelectionMetadataReceipt>, StateError> {
        let pending = pending.clone();
        let output = output_checksum.to_owned();
        self.with_conn_mut("recording prepared selected metadata output",move|conn|{
            let tx=conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let receipt=pending.selection_receipt.as_ref().ok_or(StateError::ProviderSelectionInvalid)?;
            let path=super::provider_selection::SelectionPath::from_path(pending.asset.local_path.as_deref().ok_or(StateError::ProviderSelectionInvalid)?).key()?;
            let current=metadata_receipt(&tx,&receipt.generation,&receipt.pass_key,&receipt.child,pending.asset.version_size.as_str(),&path)?;
            if current.asset.local_checksum!=pending.asset.local_checksum || current.source_checksum!=pending.source_checksum || current.selection_receipt.as_ref().is_none_or(|r|r.grouping_hash!=receipt.grouping_hash || r.metadata_flags!=receipt.metadata_flags || r.prepared!=receipt.prepared || r.preserved_local!=receipt.preserved_local) {return Ok(None)}
            let root=load_generation(&tx,&receipt.generation)?;
            let manifest=load_decision(&tx,&root,&receipt.pass_key,&receipt.child)?;
            let destination=manifest.decision.destinations.iter().find(|d|d.version_size==pending.asset.version_size.as_str() && d.path.key().ok().as_deref()==Some(path.as_str())).ok_or(StateError::ProviderSelectionInvalid)?;
            let hash=prepared_metadata_hash(&tx,&root,&manifest,destination,&output,output_size)?;
            tx.execute("UPDATE provider_active_destinations SET prepared_checksum=?6,prepared_size=?7,prepared_hash=?8 WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5",params![root.id,receipt.pass_key,receipt.child,pending.asset.version_size.as_str(),path,output,i64::try_from(output_size).map_err(|_invalid|StateError::ProviderSelectionInvalid)?,hash])?;
            let mut prepared=receipt.clone();prepared.prepared=Some((output,output_size));
            tx.commit()?;Ok(Some(prepared))
        }).await
    }
}

#[cfg(test)]
pub(crate) fn inspect_selection_decode_cost(
    conn: &Connection,
    root: &ActiveGeneration,
    manifest: &ActiveDecision,
) -> Result<(usize, usize), StateError> {
    RANK_DECODE_METRICS.with(|metrics| metrics.set((0, 0)));
    validate_decision(conn, root, manifest, true)?;
    Ok(RANK_DECODE_METRICS.with(std::cell::Cell::get))
}

#[cfg(test)]
pub(crate) fn inspect_selection_validation_budget(
    conn: &Connection,
    root: &ActiveGeneration,
    manifest: &ActiveDecision,
    capacity: usize,
) -> (Result<(), StateError>, (usize, usize)) {
    RANK_DECODE_METRICS.with(|metrics| metrics.set((0, 0)));
    let mut pages = RankProofs::new(conn, root);
    pages.capacity = capacity;
    let result = validate_decision_with_pages(conn, root, manifest, true, &mut pages).map(drop);
    (result, RANK_DECODE_METRICS.with(std::cell::Cell::get))
}
